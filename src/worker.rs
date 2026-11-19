use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use mu_ci_ipc::{
    create_fory_runtime, IpcHeader, StageCancelTask, StageCompletionReport, StageDispatchTask,
    StageStarted, WorkerHeartbeat, WorkerRegistration, WorkerShutdown, MSG_TYPE_ARROW_LOG,
    MSG_TYPE_STAGE_CANCEL, MSG_TYPE_STAGE_COMPLETION, MSG_TYPE_STAGE_DISPATCH,
    MSG_TYPE_STAGE_STARTED, MSG_TYPE_WORKER_HEARTBEAT, MSG_TYPE_WORKER_REGISTRATION,
    MSG_TYPE_WORKER_SHUTDOWN,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock, Semaphore};
use tokio::time::sleep;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

use crate::concurrency::NodeConcurrencyLimiter;
use crate::config::WorkerConfig;
use crate::stage_runner::{StageExecutionResult, StageRunner};
use crate::telemetry_emitter::{HardwareProfile, StageTraceSession};

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("Received SIGINT shutdown signal");
                }
                _ = term.recv() => {
                    tracing::info!("Received SIGTERM shutdown signal from OS/systemd");
                }
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("Received Ctrl-C shutdown signal");
}

/// Parse Linux `/proc/meminfo` text and return `(ram_total_mb, ram_used_mb)`.
///
/// `ram_total_mb` is derived from `MemTotal` (in KiB / 1024).
/// `ram_used_mb` is derived from `(MemTotal - MemAvailable)` (or `MemFree + Buffers + Cached`
/// when `MemAvailable` is absent).
pub fn parse_proc_meminfo(contents: &str) -> Option<(u64, u64)> {
    let mut mem_total_kb: Option<u64> = None;
    let mut mem_available_kb: Option<u64> = None;
    let mut mem_free_kb: u64 = 0;
    let mut buffers_kb: u64 = 0;
    let mut cached_kb: u64 = 0;

    for line in contents.lines() {
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else {
            continue;
        };
        let Some(val_str) = parts.next() else {
            continue;
        };
        let Ok(val_kb) = val_str.parse::<u64>() else {
            continue;
        };
        match key {
            "MemTotal:" => mem_total_kb = Some(val_kb),
            "MemAvailable:" => mem_available_kb = Some(val_kb),
            "MemFree:" => mem_free_kb = val_kb,
            "Buffers:" => buffers_kb = val_kb,
            "Cached:" => cached_kb = val_kb,
            _ => {}
        }
    }

    let total_kb = mem_total_kb.filter(|&t| t > 0)?;
    let avail_kb = mem_available_kb
        .unwrap_or_else(|| {
            mem_free_kb
                .saturating_add(buffers_kb)
                .saturating_add(cached_kb)
        })
        .min(total_kb);
    let used_kb = total_kb.saturating_sub(avail_kb);

    let total_mb = (total_kb / 1024).max(1);
    let used_mb = (used_kb / 1024).min(total_mb);
    Some((total_mb, used_mb))
}

/// Detect the host's physical total RAM and currently used RAM in MiB.
///
/// Reads `/proc/meminfo` on Linux, with optional override via
/// `MU_CI_WORKER_RAM_TOTAL_MB` (falling back to `ACTION_RUNNER_RAM_TOTAL_MB`).
/// Falls back to `(32768, 1024)` on non-Linux hosts where `/proc/meminfo` is unavailable.
pub fn detect_system_memory_mb() -> (u64, u64) {
    let detected = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| parse_proc_meminfo(&s));

    let (mut total_mb, mut used_mb) = detected.unwrap_or((32768, 1024));

    if let Some(override_total) = crate::config::first_parsed_env::<u64>(&[
        "MU_CI_WORKER_RAM_TOTAL_MB",
        "ACTION_RUNNER_RAM_TOTAL_MB",
    ])
    .filter(|&v| v > 0)
    {
        total_mb = override_total;
        used_mb = used_mb.min(total_mb);
    }

    (total_mb, used_mb)
}

type StageKey = (String, String);
type CancelNotifierMap = HashMap<StageKey, tokio::sync::watch::Sender<Option<String>>>;

fn normalize_cancel_reason(reason: &str) -> String {
    let trimmed = reason.trim();
    if trimmed.is_empty() {
        "Stage cancelled by coordinator".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Builds a [`StageCompletionReport`] for a stage cancelled while queued or active.
pub fn build_cancelled_completion_report(
    task: &StageDispatchTask,
    reason: &str,
    before_permit: bool,
) -> StageCompletionReport {
    let normalized = normalize_cancel_reason(reason);
    let prefix = if before_permit {
        "Stage cancelled before permit acquisition"
    } else {
        "Stage cancelled"
    };
    StageCompletionReport {
        job_id: task.job_id.clone(),
        stage_id: task.stage_id.clone(),
        exit_code: 130, // 128 + SIGINT
        duration_secs: 0,
        error_message: Some(format!("{prefix}: {normalized}")),
        security_grant_denied: None,
        traceparent: task.traceparent.clone(),
    }
}

/// Finalizes a [`StageCompletionReport`] after `run_stage` returns, preserving any active
/// `StageCancelTask` reason when the stage was cancelled while running.
pub fn finalize_stage_completion_report(
    task: &StageDispatchTask,
    res: StageExecutionResult,
    cancel_reason: Option<String>,
) -> StageCompletionReport {
    let traceparent = res.traceparent.or_else(|| task.traceparent.clone());
    if let Some(reason) = cancel_reason {
        if res.exit_code != 0 {
            let normalized = normalize_cancel_reason(&reason);
            return StageCompletionReport {
                job_id: task.job_id.clone(),
                stage_id: task.stage_id.clone(),
                exit_code: 130,
                duration_secs: res.duration_secs,
                error_message: Some(format!("Stage cancelled: {normalized}")),
                security_grant_denied: res.security_grant_denied,
                traceparent,
            };
        }
    }

    StageCompletionReport {
        job_id: task.job_id.clone(),
        stage_id: task.stage_id.clone(),
        exit_code: res.exit_code,
        duration_secs: res.duration_secs,
        error_message: res.error_message,
        security_grant_denied: res.security_grant_denied,
        traceparent,
    }
}

async fn prepare_stage_execution_state(
    cancel_notifiers: &RwLock<CancelNotifierMap>,
    cancelled_stages: &RwLock<HashMap<StageKey, String>>,
    job_id: &str,
    stage_id: &str,
) -> Result<tokio::sync::watch::Receiver<Option<String>>, String> {
    let key = (job_id.to_string(), stage_id.to_string());
    let mut notifiers = cancel_notifiers.write().await;
    let mut cancelled = cancelled_stages.write().await;
    if let Some(reason) = cancelled.remove(&key) {
        return Err(reason);
    }
    let (tx, rx) = tokio::sync::watch::channel(None);
    notifiers.insert(key, tx);
    Ok(rx)
}

async fn consume_stage_cancellation(
    cancel_notifiers: &RwLock<CancelNotifierMap>,
    cancelled_stages: &RwLock<HashMap<StageKey, String>>,
    job_id: &str,
    stage_id: &str,
) -> Option<String> {
    let key = (job_id.to_string(), stage_id.to_string());
    let mut notifiers = cancel_notifiers.write().await;
    notifiers.remove(&key);
    let mut cancelled = cancelled_stages.write().await;
    cancelled.remove(&key)
}

async fn send_completion_report(
    out_tx: &mpsc::Sender<Vec<u8>>,
    fory: &fory::Fory,
    report: &StageCompletionReport,
) {
    if let Ok(comp_payload) = fory.serialize(report) {
        let comp_hdr = IpcHeader::new(MSG_TYPE_STAGE_COMPLETION, comp_payload.len() as u32, 0, 999);
        let mut frame = Vec::with_capacity(mu_ci_ipc::HEADER_SIZE + comp_payload.len());
        frame.extend_from_slice(zerocopy::IntoBytes::as_bytes(&comp_hdr));
        frame.extend_from_slice(&comp_payload);
        let _ = out_tx.send(frame).await;
    }
}

pub struct WorkerAgent {
    config: WorkerConfig,
    limiter: Arc<NodeConcurrencyLimiter>,
    execution_gate: Arc<Semaphore>,
    active_slots: Arc<AtomicU32>,
    active_stages: Arc<RwLock<Vec<String>>>,
    active_processes: Arc<RwLock<HashMap<StageKey, u32>>>,
    cancelled_stages: Arc<RwLock<HashMap<StageKey, String>>>,
    cancel_notifiers: Arc<RwLock<CancelNotifierMap>>,
}

impl WorkerAgent {
    pub fn new(config: WorkerConfig) -> Self {
        let total_slots = config.max_slots.max(1);
        let limiter = Arc::new(NodeConcurrencyLimiter::new(total_slots));
        let execution_gate = limiter.semaphore();
        Self {
            config,
            limiter,
            execution_gate,
            active_slots: Arc::new(AtomicU32::new(0)),
            active_stages: Arc::new(RwLock::new(Vec::new())),
            active_processes: Arc::new(RwLock::new(HashMap::new())),
            cancelled_stages: Arc::new(RwLock::new(HashMap::new())),
            cancel_notifiers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Records a [`StageCancelTask`] for either a queued or active stage, notifying any
    /// active execution watcher and returning the active OS process ID if running.
    pub async fn register_stage_cancel(&self, cancel_task: &StageCancelTask) -> Option<u32> {
        let key = (cancel_task.job_id.clone(), cancel_task.stage_id.clone());
        let reason = normalize_cancel_reason(&cancel_task.reason);
        {
            let notifiers = self.cancel_notifiers.read().await;
            let mut cancelled = self.cancelled_stages.write().await;
            if let Some(tx) = notifiers.get(&key) {
                let _ = tx.send(Some(reason.clone()));
            }
            cancelled.insert(key.clone(), reason);
        }
        self.active_processes.write().await.remove(&key)
    }

    #[allow(dead_code)]
    pub fn limiter(&self) -> Arc<NodeConcurrencyLimiter> {
        self.limiter.clone()
    }

    #[allow(dead_code)]
    pub fn execution_gate(&self) -> Arc<Semaphore> {
        self.execution_gate.clone()
    }

    #[allow(dead_code)]
    pub fn available_permits(&self) -> usize {
        self.execution_gate.available_permits()
    }

    #[allow(dead_code)]
    pub fn active_slots(&self) -> u32 {
        self.active_slots.load(Ordering::Relaxed)
    }

    #[allow(dead_code)]
    pub async fn acquire_permit(
        &self,
        exclusive: bool,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, tokio::sync::AcquireError> {
        let total_slots = self.config.max_slots.max(1);
        if exclusive {
            self.execution_gate
                .clone()
                .acquire_many_owned(total_slots)
                .await
        } else {
            self.execution_gate.clone().acquire_owned().await
        }
    }

    #[allow(dead_code)]
    pub fn try_acquire_permit(
        &self,
        exclusive: bool,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, tokio::sync::TryAcquireError> {
        let total_slots = self.config.max_slots.max(1);
        if exclusive {
            self.execution_gate
                .clone()
                .try_acquire_many_owned(total_slots)
        } else {
            self.execution_gate.clone().try_acquire_owned()
        }
    }

    pub async fn run_forever(self) {
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

        tokio::spawn(async move {
            wait_for_shutdown_signal().await;
            let _ = shutdown_tx.send(true);
        });

        let mut backoff = Duration::from_secs(1);

        loop {
            if *shutdown_rx.borrow() {
                info!("Worker shutting down per received signal; terminating main loop");
                break;
            }

            info!(
                controller = %self.config.controller_url,
                "Attempting connection to ActionRunner-controller cluster tunnel..."
            );

            let mut shutdown_watch = shutdown_rx.clone();
            tokio::select! {
                res = self.connect_and_process(&mut shutdown_watch) => {
                    match res {
                        Ok(()) => {
                            if *shutdown_rx.borrow() {
                                info!("Worker shutting down cleanly; exiting");
                                break;
                            }
                            info!("Cluster session concluded cleanly; reconnecting...");
                            backoff = Duration::from_secs(1);
                        }
                        Err(e) => {
                            if *shutdown_rx.borrow() {
                                info!("Worker shutting down; exiting");
                                break;
                            }
                            let nanos = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.subsec_nanos() as u64)
                                .unwrap_or(12345);
                            let mut state = nanos ^ 0x5deece66d;
                            state ^= state << 13;
                            state ^= state >> 7;
                            state ^= state << 17;
                            let factor = 0.8 + 0.4 * ((state % 10_000) as f64 / 10_000.0);
                            let retry_duration =
                                Duration::from_secs_f64(backoff.as_secs_f64() * factor);

                            warn!(
                                error = %e,
                                retry_in = ?retry_duration,
                                "Cluster connection error; backing off before retry"
                            );
                            tokio::select! {
                                _ = sleep(retry_duration) => {}
                                _ = shutdown_rx.changed() => {
                                    info!("Shutdown signaled during retry backoff; exiting");
                                    break;
                                }
                            }
                            backoff = (backoff * 2).min(Duration::from_secs(15));
                        }
                    }
                }
                _ = shutdown_rx.changed() => {
                    info!("Shutdown signaled during tunnel operation; exiting");
                    break;
                }
            }
        }
    }

    pub async fn connect_and_process(
        &self,
        shutdown_watch: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut req = self.config.controller_url.clone().into_client_request()?;
        req.headers_mut()
            .insert("x-cluster-token", self.config.cluster_token.parse()?);

        let (ws_stream, _) = connect_async(req).await?;
        info!("Successfully connected to cluster tunnel!");

        let (mut ws_sender, mut ws_receiver) = ws_stream.split();
        let fory = create_fory_runtime()?;

        let mut seq = 0u64;

        // 1. Send WorkerRegistration frame
        let (ram_total_mb, _) = detect_system_memory_mb();
        let reg = WorkerRegistration {
            node_id: self.config.node_id.clone(),
            arch: self.config.arch.clone(),
            tags: self.config.tags.clone(),
            max_slots: self.config.max_slots,
            ram_total_mb,
            btrfs_enabled: self.config.use_btrfs,
            toolchains: self.config.toolchains.clone(),
            rust_semver: self.config.rust_semver.clone(),
        };

        let reg_payload = fory.serialize(&reg)?;
        seq += 1;
        let reg_hdr = IpcHeader::new(
            MSG_TYPE_WORKER_REGISTRATION,
            reg_payload.len() as u32,
            0,
            seq,
        );

        let mut reg_frame = Vec::with_capacity(mu_ci_ipc::HEADER_SIZE + reg_payload.len());
        reg_frame.extend_from_slice(zerocopy::IntoBytes::as_bytes(&reg_hdr));
        reg_frame.extend_from_slice(&reg_payload);
        let write_timeout = crate::config::first_parsed_env::<u64>(&[
            "MU_CI_WORKER_WRITE_TIMEOUT_SECS",
            "MU_CI_WRITE_TIMEOUT_SECS",
            "ACTION_RUNNER_WRITE_TIMEOUT_SECS",
        ])
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(10));

        tokio::time::timeout(
            write_timeout,
            ws_sender.send(Message::Binary(reg_frame.into())),
        )
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Timed out sending WorkerRegistration frame to coordinator",
            )
        })??;
        info!(
            node_id = %self.config.node_id,
            rust_semver = ?self.config.rust_semver,
            "Registered with coordinator as '{}'",
            self.config.node_id
        );

        let (outbound_tx, mut outbound_rx) = mpsc::channel::<Vec<u8>>(128);

        // Heartbeat task loop
        let node_id = self.config.node_id.clone();
        let active_slots = self.active_slots.clone();
        let hb_tx = outbound_tx.clone();
        let hb_fory = create_fory_runtime()?;

        let ping_interval = crate::config::first_parsed_env::<u64>(&[
            "MU_CI_WORKER_PING_INTERVAL_SECS",
            "MU_CI_PING_INTERVAL_SECS",
            "ACTION_RUNNER_PING_INTERVAL_SECS",
        ])
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(5));

        let hb_handle = tokio::spawn(async move {
            let mut hb_seq = 1000u64;
            let num_cpus = std::thread::available_parallelism()
                .map(|n| n.get() as f32)
                .unwrap_or(4.0)
                .max(1.0);
            loop {
                sleep(ping_interval).await;
                let cpu_load_pct = std::fs::read_to_string("/proc/loadavg")
                    .ok()
                    .and_then(|s| s.split_whitespace().next()?.parse::<f32>().ok())
                    .map(|load1| ((load1 / num_cpus) * 100.0).clamp(0.0, 100.0))
                    .unwrap_or(10.0);
                let (_, ram_used_mb) = detect_system_memory_mb();
                let hb = WorkerHeartbeat {
                    node_id: node_id.clone(),
                    active_slots: active_slots.load(Ordering::Relaxed),
                    cpu_load_pct,
                    ram_used_mb,
                    timestamp_epoch_ms: Utc::now().timestamp_millis() as u64,
                };

                if let Ok(payload) = hb_fory.serialize(&hb) {
                    hb_seq += 1;
                    let hdr =
                        IpcHeader::new(MSG_TYPE_WORKER_HEARTBEAT, payload.len() as u32, 0, hb_seq);
                    let mut frame = Vec::with_capacity(mu_ci_ipc::HEADER_SIZE + payload.len());
                    frame.extend_from_slice(zerocopy::IntoBytes::as_bytes(&hdr));
                    frame.extend_from_slice(&payload);
                    if hb_tx.send(frame).await.is_err() {
                        break;
                    }
                }
            }
        });

        let stage_runner = Arc::new(StageRunner::new(self.config.clone()));
        let liveness_timeout = crate::config::first_parsed_env::<u64>(&[
            "MU_CI_WORKER_LIVENESS_TIMEOUT_SECS",
            "MU_CI_LIVENESS_TIMEOUT_SECS",
            "ACTION_RUNNER_LIVENESS_TIMEOUT_SECS",
        ])
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(15));
        let mut liveness_deadline = tokio::time::Instant::now() + liveness_timeout;

        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(liveness_deadline) => {
                    warn!(
                        node = %self.config.node_id,
                        timeout_secs = liveness_timeout.as_secs(),
                        "Coordinator tunnel read-liveness deadline expired without Pong or inbound frame; disconnecting to trigger reconnect"
                    );
                    hb_handle.abort();
                    return Err(Box::new(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "Coordinator tunnel read-liveness deadline expired without Pong or inbound frame",
                    )));
                }

                _ = shutdown_watch.changed() => {
                    if *shutdown_watch.borrow() {
                        info!("Received shutdown signal; notifying coordinator before disconnecting");
                        for pid in self.active_processes.read().await.values() {
                            info!(pid = pid, "Terminating active process group before worker disconnect");
                            #[cfg(unix)]
                            unsafe {
                                let _ = libc::kill(-(*pid as i32), libc::SIGTERM);
                                let _ = libc::kill(*pid as i32, libc::SIGTERM);
                            }
                        }
                        let interrupted = self.active_stages.read().await.clone();
                        let shutdown_payload = WorkerShutdown {
                            node_id: self.config.node_id.clone(),
                            reason: "Worker daemon restarting / shutting down (SIGTERM/SIGINT)".to_string(),
                            interrupted_stages: interrupted.clone(),
                        };
                        if let Ok(payload) = fory.serialize(&shutdown_payload) {
                            seq += 1;
                            let hdr = IpcHeader::new(
                                MSG_TYPE_WORKER_SHUTDOWN,
                                payload.len() as u32,
                                0,
                                seq,
                            );
                            let mut frame = Vec::with_capacity(mu_ci_ipc::HEADER_SIZE + payload.len());
                            frame.extend_from_slice(zerocopy::IntoBytes::as_bytes(&hdr));
                            frame.extend_from_slice(&payload);
                            let _ = tokio::time::timeout(
                                write_timeout,
                                ws_sender.send(Message::Binary(frame.into())),
                            )
                            .await;
                            let _ = tokio::time::timeout(
                                write_timeout,
                                ws_sender.flush(),
                            )
                            .await;
                            info!(node = %self.config.node_id, stages = ?interrupted, "Dispatched WorkerShutdown frame to coordinator");
                        }
                        break;
                    }
                }

                // Outbound messages to send to coordinator
                Some(frame) = outbound_rx.recv() => {
                    let is_hb = IpcHeader::parse(&frame)
                        .map(|(hdr, _)| hdr.msg_type == MSG_TYPE_WORKER_HEARTBEAT)
                        .unwrap_or(false);
                    tokio::time::timeout(
                        write_timeout,
                        ws_sender.send(Message::Binary(frame.into())),
                    )
                    .await
                    .map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "Timed out sending binary frame to coordinator",
                        )
                    })??;
                    if is_hb {
                        let ping_payload: tokio_tungstenite::tungstenite::Bytes =
                            Utc::now().timestamp_millis().to_be_bytes().to_vec().into();
                        tokio::time::timeout(
                            write_timeout,
                            ws_sender.send(Message::Ping(ping_payload)),
                        )
                        .await
                        .map_err(|_| {
                            std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "Timed out sending WebSocket Ping to coordinator",
                            )
                        })??;
                    }
                }

                // Inbound messages received from coordinator
                inbound = ws_receiver.next() => {
                    let msg = match inbound {
                        Some(Ok(m)) => m,
                        _ => break, // Disconnected
                    };
                    liveness_deadline = tokio::time::Instant::now() + liveness_timeout;

                    match &msg {
                        Message::Ping(payload) => {
                            let _ = tokio::time::timeout(
                                write_timeout,
                                ws_sender.send(Message::Pong(payload.clone())),
                            )
                            .await;
                            continue;
                        }
                        Message::Pong(_) => continue,
                        Message::Close(_) => break,
                        _ => {}
                    }

                    if let Message::Binary(bin) = msg {
                        match IpcHeader::parse(&bin) {
                            Ok((hdr, payload)) => {
                                info!(msg_type = hdr.msg_type, payload_len = payload.len(), "Worker received binary frame");
                                if hdr.msg_type == MSG_TYPE_STAGE_DISPATCH {
                                    match fory.deserialize::<StageDispatchTask>(payload) {
                                        Ok(task) => {
                                            info!(
                                                job_id = %task.job_id,
                                                stage = %task.stage_id,
                                                exclusive = task.exclusive,
                                                "Worker successfully deserialized StageDispatchTask"
                                            );
                                            let runner = stage_runner.clone();
                                            let limiter = self.limiter.clone();
                                            let out_tx = outbound_tx.clone();
                                            let active_counter = self.active_slots.clone();
                                            let active_stages = self.active_stages.clone();
                                            let active_processes = self.active_processes.clone();
                                            let cancelled_stages = self.cancelled_stages.clone();
                                            let cancel_notifiers = self.cancel_notifiers.clone();
                                            let total_slots = self.config.max_slots.max(1);
                                            let node_id = self.config.node_id.clone();
                                            let comp_fory = create_fory_runtime()?;

                                            tokio::spawn(async move {
                                                let permits_to_acquire = if task.exclusive {
                                                    total_slots
                                                } else {
                                                    1
                                                };

                                                info!(
                                                    job_id = %task.job_id,
                                                    stage = %task.stage_id,
                                                    exclusive = task.exclusive,
                                                    permits = permits_to_acquire,
                                                    total_slots = total_slots,
                                                    "Awaiting execution gate permit(s) for stage"
                                                );

                                                let permit_res = limiter.acquire_for_task(&task).await;

                                                let _permit = match permit_res {
                                                    Ok(p) => p,
                                                    Err(e) => {
                                                        tracing::error!(
                                                            job_id = %task.job_id,
                                                            stage = %task.stage_id,
                                                            error = %e,
                                                            "Execution gate closed; aborting stage dispatch"
                                                        );
                                                        return;
                                                    }
                                                };

                                                // Check if cancelled while waiting for permit and register active watcher atomically
                                                let cancel_rx = match prepare_stage_execution_state(
                                                    &cancel_notifiers,
                                                    &cancelled_stages,
                                                    &task.job_id,
                                                    &task.stage_id,
                                                )
                                                .await
                                                {
                                                    Ok(rx) => rx,
                                                    Err(reason) => {
                                                        info!(
                                                            job_id = %task.job_id,
                                                            stage = %task.stage_id,
                                                            reason = %reason,
                                                            "Stage was cancelled while queued in execution gate; releasing permit immediately"
                                                        );
                                                        let report = build_cancelled_completion_report(
                                                            &task,
                                                            &reason,
                                                            true,
                                                        );
                                                        send_completion_report(
                                                            &out_tx,
                                                            &comp_fory,
                                                            &report,
                                                        )
                                                        .await;
                                                        return;
                                                    }
                                                };

                                                info!(
                                                    job_id = %task.job_id,
                                                    stage = %task.stage_id,
                                                    exclusive = task.exclusive,
                                                    "Acquired execution gate permit(s); launching stage"
                                                );

                                                active_counter
                                                    .fetch_add(permits_to_acquire, Ordering::Relaxed);
                                                active_stages.write().await.push(task.stage_id.clone());

                                                let (started_tx, started_rx) =
                                                    tokio::sync::oneshot::channel::<u32>();
                                                let node_id_started = node_id.clone();
                                                let out_tx_started = out_tx.clone();
                                                let stage_id_started = task.stage_id.clone();
                                                let job_id_started = task.job_id.clone();
                                                let active_proc_reg = active_processes.clone();
                                                let started_traceparent = Some(
                                                    StageTraceSession::from_dispatch_task(
                                                        &task,
                                                        &node_id,
                                                        &HardwareProfile {
                                                            arch: String::new(),
                                                            cpu_cluster: "generic".to_string(),
                                                            vlen_bits: 0,
                                                        },
                                                    )
                                                    .echo_traceparent,
                                                );

                                                let started_handle = tokio::spawn(async move {
                                                    if let Ok(pid) = started_rx.await {
                                                        active_proc_reg.write().await.insert(
                                                            (
                                                                job_id_started.clone(),
                                                                stage_id_started.clone(),
                                                            ),
                                                            pid,
                                                        );
                                                        let started_event = StageStarted {
                                                            job_id: job_id_started,
                                                            stage_id: stage_id_started,
                                                            node_id: node_id_started,
                                                            pid,
                                                            started_at_epoch_ms: Utc::now()
                                                                .timestamp_millis()
                                                                as u64,
                                                            traceparent: started_traceparent,
                                                        };
                                                        if let Ok(sfory) = create_fory_runtime() {
                                                            if let Ok(spayload) =
                                                                sfory.serialize(&started_event)
                                                            {
                                                                let shdr = IpcHeader::new(
                                                                    MSG_TYPE_STAGE_STARTED,
                                                                    spayload.len() as u32,
                                                                    0,
                                                                    1001,
                                                                );
                                                                let mut sframe = Vec::with_capacity(
                                                                    mu_ci_ipc::HEADER_SIZE
                                                                        + spayload.len(),
                                                                );
                                                                sframe.extend_from_slice(
                                                                    zerocopy::IntoBytes::as_bytes(
                                                                        &shdr,
                                                                    ),
                                                                );
                                                                sframe.extend_from_slice(&spayload);
                                                                let _ = out_tx_started
                                                                    .send(sframe)
                                                                    .await;
                                                            }
                                                        }
                                                    }
                                                });

                                                let (log_batch_tx, mut log_batch_rx) =
                                                    mpsc::channel::<Vec<u8>>(32);

                                                // Forward log batches to tunnel outbound channel
                                                let log_forwarder = {
                                                    let out_tx = out_tx.clone();
                                                    tokio::spawn(async move {
                                                        let mut l_seq = 2000u64;
                                                        while let Some(batch_bytes) =
                                                            log_batch_rx.recv().await
                                                        {
                                                            l_seq += 1;
                                                            let log_hdr = IpcHeader::new(
                                                                MSG_TYPE_ARROW_LOG,
                                                                batch_bytes.len() as u32,
                                                                0,
                                                                l_seq,
                                                             );
                                                            let mut frame = Vec::with_capacity(
                                                                mu_ci_ipc::HEADER_SIZE
                                                                    + batch_bytes.len(),
                                                            );
                                                            frame.extend_from_slice(
                                                                zerocopy::IntoBytes::as_bytes(
                                                                    &log_hdr,
                                                                ),
                                                            );
                                                            frame.extend_from_slice(&batch_bytes);
                                                            let _ = out_tx.send(frame).await;
                                                        }
                                                    })
                                                };

                                                let res = runner
                                                    .run_stage_with_cancel(
                                                        &task,
                                                        log_batch_tx,
                                                        Some(started_tx),
                                                        Some(cancel_rx),
                                                    )
                                                    .await;
                                                let _ = started_handle.await;
                                                let _ = log_forwarder.await;

                                                active_counter
                                                    .fetch_sub(permits_to_acquire, Ordering::Relaxed);
                                                active_stages
                                                    .write()
                                                    .await
                                                    .retain(|s| s != &task.stage_id);
                                                active_processes
                                                    .write()
                                                    .await
                                                    .remove(&(task.job_id.clone(), task.stage_id.clone()));

                                                let cancel_reason = consume_stage_cancellation(
                                                    &cancel_notifiers,
                                                    &cancelled_stages,
                                                    &task.job_id,
                                                    &task.stage_id,
                                                )
                                                .await;

                                                let report = finalize_stage_completion_report(
                                                    &task,
                                                    res,
                                                    cancel_reason,
                                                );
                                                send_completion_report(&out_tx, &comp_fory, &report)
                                                    .await;
                                            });
                                        }
                                        Err(e) => {
                                            tracing::error!(
                                                error = %e,
                                                "Worker failed to deserialize StageDispatchTask with Fory"
                                            );
                                        }
                                    }
                                } else if hdr.msg_type == MSG_TYPE_STAGE_CANCEL {
                                    if let Ok(cancel_task) =
                                        fory.deserialize::<StageCancelTask>(payload)
                                    {
                                        info!(
                                            job_id = %cancel_task.job_id,
                                            stage = %cancel_task.stage_id,
                                            reason = %cancel_task.reason,
                                            "Worker received StageCancelTask; checking for active process"
                                        );
                                        let maybe_pid =
                                            self.register_stage_cancel(&cancel_task).await;
                                        if let Some(pid) = maybe_pid {
                                            info!(
                                                job_id = %cancel_task.job_id,
                                                stage = %cancel_task.stage_id,
                                                pid = pid,
                                                reason = %cancel_task.reason,
                                                "Terminating active process group via SIGTERM"
                                            );
                                            #[cfg(unix)]
                                            unsafe {
                                                let _ = libc::kill(-(pid as i32), libc::SIGTERM);
                                                let _ = libc::kill(pid as i32, libc::SIGTERM);
                                            }
                                        }
                                    }
                                }

                            }
                            Err(e) => {
                                tracing::error!(error = ?e, "Worker failed to parse IpcHeader");
                            }
                        }
                    }
                }
            }
        }

        hb_handle.abort();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_proc_meminfo_4gib_worker() {
        let sample = "\
MemTotal:        3884032 kB
MemFree:         2949120 kB
MemAvailable:    3369984 kB
Buffers:           40960 kB
Cached:           419840 kB
";
        let (total_mb, used_mb) = parse_proc_meminfo(sample).expect("valid 4 GiB meminfo");
        assert_eq!(total_mb, 3793);
        assert_eq!(used_mb, 502);
    }

    #[test]
    fn test_parse_proc_meminfo_16gib_and_32gib_workers() {
        let sample_16g = "\
MemTotal:       16384000 kB
MemFree:        10240000 kB
MemAvailable:   14336000 kB
";
        let (total_16, used_16) = parse_proc_meminfo(sample_16g).expect("valid 16 GiB meminfo");
        assert_eq!(total_16, 16000);
        assert_eq!(used_16, 2000);

        let sample_32g = "\
MemTotal:       33554432 kB
MemFree:        25165824 kB
MemAvailable:   30408704 kB
";
        let (total_32, used_32) = parse_proc_meminfo(sample_32g).expect("valid 32 GiB meminfo");
        assert_eq!(total_32, 32768);
        assert_eq!(used_32, 3072);
    }

    #[test]
    fn test_parse_proc_meminfo_fallback_without_mem_available() {
        let sample = "\
MemTotal:        4194304 kB
MemFree:         1048576 kB
Buffers:          524288 kB
Cached:          1572864 kB
";
        let (total_mb, used_mb) =
            parse_proc_meminfo(sample).expect("fallback without MemAvailable");
        assert_eq!(total_mb, 4096);
        assert_eq!(used_mb, 1024);
    }

    #[test]
    fn test_parse_proc_meminfo_invalid_returns_none() {
        assert_eq!(parse_proc_meminfo(""), None);
        assert_eq!(parse_proc_meminfo("MemTotal: 0 kB\n"), None);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn test_active_and_queued_stage_cancel_preserves_reason_and_consumes_state() {
        let mut config = WorkerConfig::load();
        config.node_id = "worker-test".to_string();
        let worker = WorkerAgent::new(config);

        let task = StageDispatchTask {
            job_id: "job-cancel-72".to_string(),
            stage_id: "test-check".to_string(),
            traceparent: Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into()),
            ..Default::default()
        };

        // 1. Active stage cancellation: prepare watcher, register running PID, receive StageCancelTask
        let mut cancel_rx = prepare_stage_execution_state(
            &worker.cancel_notifiers,
            &worker.cancelled_stages,
            &task.job_id,
            &task.stage_id,
        )
        .await
        .expect("stage not yet cancelled");

        worker
            .active_processes
            .write()
            .await
            .insert((task.job_id.clone(), task.stage_id.clone()), 424242);

        let cancel_task = StageCancelTask {
            job_id: task.job_id.clone(),
            stage_id: task.stage_id.clone(),
            reason: "Superseded by newer push to feat/branch".to_string(),
        };
        let killed_pid = worker.register_stage_cancel(&cancel_task).await;
        assert_eq!(killed_pid, Some(424242));
        assert_eq!(
            cancel_rx.borrow_and_update().as_deref(),
            Some("Superseded by newer push to feat/branch")
        );

        let consumed_reason = consume_stage_cancellation(
            &worker.cancel_notifiers,
            &worker.cancelled_stages,
            &task.job_id,
            &task.stage_id,
        )
        .await;
        assert_eq!(
            consumed_reason.as_deref(),
            Some("Superseded by newer push to feat/branch")
        );
        // State is cleanly consumed so subsequent lookups return None.
        assert_eq!(
            consume_stage_cancellation(
                &worker.cancel_notifiers,
                &worker.cancelled_stages,
                &task.job_id,
                &task.stage_id,
            )
            .await,
            None
        );

        // Simulate child killed by SIGTERM (143) before runner saw cancel_rx
        let raw_sigterm_result = StageExecutionResult {
            exit_code: 143,
            duration_secs: 4,
            pid: 424242,
            error_message: Some(
                "Tool 'cargo-test' terminated by signal 15 (SIGTERM) (exit code 143)".to_string(),
            ),
            security_grant_denied: None,
            traceparent: task.traceparent.clone(),
            child_span: None,
            test_case_runs: Vec::new(),
            benchmark_runs: Vec::new(),
        };
        let report = finalize_stage_completion_report(&task, raw_sigterm_result, consumed_reason);
        assert_eq!(report.exit_code, 130);
        assert_eq!(
            report.error_message.as_deref(),
            Some("Stage cancelled: Superseded by newer push to feat/branch")
        );

        // 2. Queued stage cancellation before permit acquisition
        let queued_cancel = StageCancelTask {
            job_id: "job-queued-72".to_string(),
            stage_id: "clippy-check".to_string(),
            reason: "Cancelled by user".to_string(),
        };
        assert_eq!(worker.register_stage_cancel(&queued_cancel).await, None);
        let queued_err = prepare_stage_execution_state(
            &worker.cancel_notifiers,
            &worker.cancelled_stages,
            "job-queued-72",
            "clippy-check",
        )
        .await
        .expect_err("queued cancellation must be observed before launch");
        assert_eq!(queued_err, "Cancelled by user");
        let queued_report = build_cancelled_completion_report(&task, &queued_err, true);
        assert_eq!(queued_report.exit_code, 130);
        assert_eq!(
            queued_report.error_message.as_deref(),
            Some("Stage cancelled before permit acquisition: Cancelled by user")
        );
    }

    #[test]
    fn test_finalize_stage_completion_report_preserves_zero_exit_and_normal_failures() {
        let task = StageDispatchTask {
            job_id: "job-normal".to_string(),
            stage_id: "test-check".to_string(),
            traceparent: Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into()),
            ..Default::default()
        };
        let ok_res = StageExecutionResult {
            exit_code: 0,
            duration_secs: 2,
            pid: 100,
            error_message: None,
            security_grant_denied: None,
            traceparent: None,
            child_span: None,
            test_case_runs: Vec::new(),
            benchmark_runs: Vec::new(),
        };
        let ok_report =
            finalize_stage_completion_report(&task, ok_res, Some("late cancel".to_string()));
        assert_eq!(ok_report.exit_code, 0);
        assert_eq!(ok_report.error_message, None);
        assert_eq!(ok_report.traceparent, task.traceparent);

        let crash_res = StageExecutionResult {
            exit_code: 137,
            duration_secs: 5,
            pid: 101,
            error_message: Some("Tool 'cargo-miri' terminated by signal 9 (SIGKILL)".to_string()),
            security_grant_denied: None,
            traceparent: None,
            child_span: None,
            test_case_runs: Vec::new(),
            benchmark_runs: Vec::new(),
        };
        let crash_report = finalize_stage_completion_report(&task, crash_res, None);
        assert_eq!(crash_report.exit_code, 137);
        assert!(crash_report
            .error_message
            .as_deref()
            .unwrap()
            .contains("SIGKILL"));
    }
}
