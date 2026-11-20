#![deny(unsafe_code)]

use futures_util::{SinkExt, StreamExt};
use mu_ci_ipc::{
    create_fory_runtime, IpcHeader, WorkerHeartbeat, WorkerRegistration, WorkerShutdown,
    MSG_TYPE_WORKER_HEARTBEAT, MSG_TYPE_WORKER_REGISTRATION, MSG_TYPE_WORKER_SHUTDOWN,
};
use mu_ci_worker::concurrency::NodeConcurrencyLimiter;
use mu_ci_worker::config::{IsolationMode, WorkerConfig};
use mu_ci_worker::worker::{parse_proc_meminfo, WorkerAgent};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::Message;

static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Debug, Clone)]
struct SmokeConfig {
    target_url: Option<String>,
    bearer_token: Option<String>,
    tls_ca_pem: Option<Vec<u8>>,
    tls_cert_pem: Option<Vec<u8>>,
    tls_key_pem: Option<Vec<u8>>,
    timeout: Duration,
}

fn read_trimmed_secret_file(var_name: &str, path_raw: &str) -> Result<String, String> {
    let path = path_raw.trim();
    if path.is_empty() {
        return Err(format!("{var_name} was set to an empty path"));
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {var_name} at '{path}': {e}"))?;
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        return Err(format!("{var_name} at '{path}' is empty"));
    }
    Ok(trimmed)
}

fn read_non_empty_file_bytes(var_name: &str, path_raw: &str) -> Result<Vec<u8>, String> {
    let path = path_raw.trim();
    if path.is_empty() {
        return Err(format!("{var_name} was set to an empty path"));
    }
    let bytes =
        std::fs::read(path).map_err(|e| format!("Failed to read {var_name} at '{path}': {e}"))?;
    if bytes.is_empty() {
        return Err(format!("{var_name} at '{path}' is empty"));
    }
    Ok(bytes)
}

fn load_smoke_config_with<F>(get_env: F) -> Result<SmokeConfig, String>
where
    F: Fn(&str) -> Option<String>,
{
    let target_url = get_env("MU_SMOKE_TARGET_URL")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let bearer_token = match get_env("MU_SMOKE_BEARER_TOKEN_FILE")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        Some(path) => Some(read_trimmed_secret_file(
            "MU_SMOKE_BEARER_TOKEN_FILE",
            &path,
        )?),
        None => None,
    };

    let tls_ca_pem = match get_env("MU_SMOKE_TLS_CA_FILE")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        Some(path) => Some(read_non_empty_file_bytes("MU_SMOKE_TLS_CA_FILE", &path)?),
        None => None,
    };

    let cert_path = get_env("MU_SMOKE_TLS_CERT_FILE")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let key_path = get_env("MU_SMOKE_TLS_KEY_FILE")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let (tls_cert_pem, tls_key_pem) = match (cert_path, key_path) {
        (Some(cert), Some(key)) => (
            Some(read_non_empty_file_bytes("MU_SMOKE_TLS_CERT_FILE", &cert)?),
            Some(read_non_empty_file_bytes("MU_SMOKE_TLS_KEY_FILE", &key)?),
        ),
        (Some(_), None) | (None, Some(_)) => {
            return Err(
                "MU_SMOKE_TLS_CERT_FILE and MU_SMOKE_TLS_KEY_FILE must both be set together"
                    .to_string(),
            );
        }
        (None, None) => (None, None),
    };

    let timeout_secs = match get_env("MU_SMOKE_TIMEOUT_SECS")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        Some(raw) => {
            let secs = raw.parse::<u64>().map_err(|e| {
                format!("Invalid MU_SMOKE_TIMEOUT_SECS '{raw}': expected positive integer ({e})")
            })?;
            if secs == 0 {
                return Err("MU_SMOKE_TIMEOUT_SECS must be greater than zero".to_string());
            }
            secs
        }
        None => 10,
    };

    Ok(SmokeConfig {
        target_url,
        bearer_token,
        tls_ca_pem,
        tls_cert_pem,
        tls_key_pem,
        timeout: Duration::from_secs(timeout_secs),
    })
}

fn load_smoke_config() -> Result<SmokeConfig, String> {
    load_smoke_config_with(|k| std::env::var(k).ok())
}

async fn probe_live_target(cfg: &SmokeConfig, target_url: &str) -> Result<(), String> {
    let trimmed = target_url.trim();
    if trimmed.starts_with("ws://") {
        // Non-intrusive unauthenticated tunnel handshake check: must fail closed with 401/403
        // rather than permitting anonymous worker registration.
        let res = tokio::time::timeout(cfg.timeout, tokio_tungstenite::connect_async(trimmed))
            .await
            .map_err(|_| format!("Timed out probing WebSocket endpoint '{trimmed}'"))?;
        match res {
            Err(tokio_tungstenite::tungstenite::Error::Http(resp))
                if matches!(
                    resp.status(),
                    StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
                ) =>
            {
                Ok(())
            }
            Err(other) => Err(format!(
                "Unexpected WebSocket handshake result probing '{trimmed}': {other}"
            )),
            Ok(_) => Err(format!(
                "WebSocket endpoint '{trimmed}' unexpectedly allowed unauthenticated worker registration"
            )),
        }
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        let (authority, path_raw) = match rest.find('/') {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, "/health"),
        };
        let path = if path_raw.is_empty() || path_raw == "/" {
            "/health"
        } else {
            path_raw
        };
        let mut stream = tokio::time::timeout(cfg.timeout, TcpStream::connect(authority))
            .await
            .map_err(|_| format!("Timed out connecting to '{authority}'"))?
            .map_err(|e| format!("Failed connecting to '{authority}': {e}"))?;

        let mut req = format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n");
        if let Some(ref tok) = cfg.bearer_token {
            req.push_str(&format!("Authorization: Bearer {tok}\r\n"));
        }
        req.push_str("\r\n");

        tokio::time::timeout(cfg.timeout, stream.write_all(req.as_bytes()))
            .await
            .map_err(|_| "Timed out writing HTTP probe".to_string())?
            .map_err(|e| format!("Failed writing HTTP probe: {e}"))?;

        let mut buf = Vec::new();
        tokio::time::timeout(cfg.timeout, stream.read_to_end(&mut buf))
            .await
            .map_err(|_| "Timed out reading HTTP probe response".to_string())?
            .map_err(|e| format!("Failed reading HTTP probe response: {e}"))?;

        let head = String::from_utf8_lossy(&buf);
        let status = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or_else(|| format!("Malformed HTTP probe response from '{trimmed}'"))?;
        if !(200..300).contains(&status) {
            return Err(format!(
                "Read-only probe '{trimmed}' returned HTTP {status}"
            ));
        }
        Ok(())
    } else {
        Err(format!(
            "Unsupported live probe URL scheme '{trimmed}' (mu-ci-worker supports ws:// or http://)"
        ))
    }
}

#[allow(clippy::result_large_err)]
fn check_ws_cluster_token(
    req: &Request,
    resp: Response,
    expected_token: &str,
) -> Result<Response, ErrorResponse> {
    let got = req
        .headers()
        .get("x-cluster-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if got != expected_token {
        let mut err = ErrorResponse::new(Some("unauthorized".to_string()));
        *err.status_mut() = StatusCode::UNAUTHORIZED;
        return Err(err);
    }
    Ok(resp)
}

#[allow(clippy::result_large_err)]
async fn accept_checked_ws(
    stream: TcpStream,
    expected_token: Arc<String>,
) -> Result<tokio_tungstenite::WebSocketStream<TcpStream>, tokio_tungstenite::tungstenite::Error> {
    tokio_tungstenite::accept_hdr_async(stream, move |req: &Request, resp: Response| {
        check_ws_cluster_token(req, resp, expected_token.as_str())
    })
    .await
}

#[tokio::test]
async fn smoke_mu_ci_worker_tunnel_registration_and_heartbeat() {
    let cfg = load_smoke_config().expect("Smoke environment config must be valid");
    if let Some(ref target_url) = cfg.target_url {
        probe_live_target(&cfg, target_url)
            .await
            .expect("Live read-only worker target probe must succeed");
        return;
    }

    let _env_guard = ENV_LOCK.lock().await;
    let prev_ping = std::env::var("MU_CI_WORKER_PING_INTERVAL_SECS").ok();
    let prev_liveness = std::env::var("MU_CI_WORKER_LIVENESS_TIMEOUT_SECS").ok();
    std::env::set_var("MU_CI_WORKER_PING_INTERVAL_SECS", "1");
    std::env::set_var("MU_CI_WORKER_LIVENESS_TIMEOUT_SECS", "6");

    let expected_token = Arc::new("ephemeral-smoke-cluster-token".to_string());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let ws_url = format!("ws://{addr}/api/cluster/tunnel");

    let tok_srv = Arc::clone(&expected_token);
    let server_handle = tokio::spawn(async move {
        // Connection 1: reject unauthenticated upgrade with 401 Unauthorized.
        let (unauth_stream, _) = listener.accept().await.unwrap();
        let unauth_res = accept_checked_ws(unauth_stream, Arc::clone(&tok_srv)).await;
        assert!(unauth_res.is_err());

        // Connection 2: accept authenticated worker tunnel and verify registration -> heartbeat -> ping/pong -> shutdown.
        let (auth_stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_checked_ws(auth_stream, Arc::clone(&tok_srv))
            .await
            .expect("Authenticated worker handshake must succeed");

        let fory = create_fory_runtime().unwrap();

        let reg_msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("Timed out waiting for WorkerRegistration")
            .unwrap()
            .unwrap();
        let reg_bytes = match reg_msg {
            Message::Binary(b) => b,
            other => panic!("Expected binary registration frame, got {other:?}"),
        };
        let (reg_hdr, reg_payload) = IpcHeader::parse(&reg_bytes).unwrap();
        assert_eq!(reg_hdr.msg_type, MSG_TYPE_WORKER_REGISTRATION);
        let reg: WorkerRegistration = fory.deserialize(reg_payload).unwrap();
        assert_eq!(reg.node_id, "worker-smoke-ephemeral");
        assert_eq!(reg.arch, "riscv64");
        assert_eq!(reg.max_slots, 1);
        assert!(reg.ram_total_mb > 0);
        assert!(reg.tags.iter().any(|t| t == "riscv64"));

        let mut saw_heartbeat = false;
        let mut saw_ping = false;
        while !(saw_heartbeat && saw_ping) {
            let frame = tokio::time::timeout(Duration::from_secs(5), ws.next())
                .await
                .expect("Timed out waiting for WorkerHeartbeat / Ping")
                .unwrap()
                .unwrap();
            match frame {
                Message::Binary(bin) => {
                    let (hdr, payload) = IpcHeader::parse(&bin).unwrap();
                    if hdr.msg_type == MSG_TYPE_WORKER_HEARTBEAT {
                        let hb: WorkerHeartbeat = fory.deserialize(payload).unwrap();
                        assert_eq!(hb.node_id, "worker-smoke-ephemeral");
                        assert_eq!(hb.active_slots, 0);
                        saw_heartbeat = true;
                    }
                }
                Message::Ping(ping_data) => {
                    ws.send(Message::Pong(ping_data)).await.unwrap();
                    saw_ping = true;
                }
                _ => {}
            }
        }

        ws
    });

    // 1. Verify unauthenticated WS client fails with 401
    let bad_req = tokio_tungstenite::connect_async(&ws_url).await;
    assert!(bad_req.is_err(), "Unauthenticated tunnel upgrade must fail");

    // 2. Load WorkerConfig via canonical env loader and connect WorkerAgent
    let temp_ws = tempfile::tempdir().unwrap();
    let mut env_map = HashMap::new();
    env_map.insert("MU_CI_WORKER_CONTROLLER_URL", ws_url.clone());
    env_map.insert("MU_CI_WORKER_CLUSTER_TOKEN", expected_token.to_string());
    env_map.insert("MU_CI_WORKER_NODE_ID", "worker-smoke-ephemeral".to_string());
    env_map.insert("MU_CI_WORKER_ARCH", "riscv64".to_string());
    env_map.insert("MU_CI_WORKER_TAGS", "riscv64,rvv,smoke".to_string());
    env_map.insert("MU_CI_WORKER_MAX_SLOTS", "1".to_string());
    env_map.insert(
        "MU_CI_WORKER_WORKSPACE",
        temp_ws.path().to_string_lossy().into_owned(),
    );
    env_map.insert("MU_CI_WORKER_USE_BTRFS", "false".to_string());
    env_map.insert("MU_CI_WORKER_ISOLATION", "none".to_string());

    let loaded_cfg = WorkerConfig::load_with_env(|k| env_map.get(k).cloned());
    assert_eq!(loaded_cfg.controller_url, ws_url);
    assert_eq!(loaded_cfg.max_slots, 1);
    assert_eq!(loaded_cfg.isolation_mode, IsolationMode::None);
    assert!(!loaded_cfg.is_tool_disabled_with_env("cargo-fmt", |_| None));

    let worker = WorkerAgent::new(loaded_cfg);
    assert_eq!(worker.active_slots(), 0);
    assert_eq!(worker.available_permits(), 1);

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let worker_task =
        tokio::spawn(async move { worker.connect_and_process(&mut shutdown_rx).await });

    let mut srv_ws = server_handle.await.unwrap();
    shutdown_tx.send(true).unwrap();

    // Verify clean WorkerShutdown frame before exit
    let mut saw_shutdown = false;
    let fory = create_fory_runtime().unwrap();
    for _ in 0..8 {
        if let Ok(Some(Ok(msg))) = tokio::time::timeout(Duration::from_secs(3), srv_ws.next()).await
        {
            if let Message::Binary(bin) = msg {
                if let Ok((hdr, payload)) = IpcHeader::parse(&bin) {
                    if hdr.msg_type == MSG_TYPE_WORKER_SHUTDOWN {
                        let sd: WorkerShutdown = fory.deserialize(payload).unwrap();
                        assert_eq!(sd.node_id, "worker-smoke-ephemeral");
                        assert!(sd.interrupted_stages.is_empty());
                        saw_shutdown = true;
                        break;
                    }
                }
            }
        } else {
            break;
        }
    }
    assert!(
        saw_shutdown,
        "Worker must emit WorkerShutdown on graceful exit"
    );

    let worker_res = tokio::time::timeout(Duration::from_secs(3), worker_task)
        .await
        .expect("Worker connect_and_process must finish after shutdown")
        .unwrap();
    assert!(worker_res.is_ok());

    match prev_ping {
        Some(v) => std::env::set_var("MU_CI_WORKER_PING_INTERVAL_SECS", v),
        None => std::env::remove_var("MU_CI_WORKER_PING_INTERVAL_SECS"),
    }
    match prev_liveness {
        Some(v) => std::env::set_var("MU_CI_WORKER_LIVENESS_TIMEOUT_SECS", v),
        None => std::env::remove_var("MU_CI_WORKER_LIVENESS_TIMEOUT_SECS"),
    }
}

#[tokio::test]
async fn smoke_mu_ci_worker_config_denylist_and_fail_closed_files() {
    // 1. ARM worker cargo-miri denylist readiness & override behavior
    assert!(WorkerConfig::is_tool_disabled_for(
        "aarch64",
        "cargo-miri",
        None
    ));
    assert!(WorkerConfig::is_tool_disabled_for(
        "arm64",
        "cargo-miri",
        None
    ));
    assert!(!WorkerConfig::is_tool_disabled_for(
        "x86_64",
        "cargo-miri",
        None
    ));
    assert!(!WorkerConfig::is_tool_disabled_for(
        "riscv64",
        "cargo-miri",
        None
    ));
    assert!(!WorkerConfig::is_tool_disabled_for(
        "aarch64",
        "cargo-miri",
        Some("none")
    ));
    assert!(WorkerConfig::is_tool_disabled_for(
        "x86_64",
        "cargo-audit",
        Some("cargo-fmt, cargo-audit")
    ));

    // 2. Path resolution, memory parser & concurrency limiter readiness
    let tmp = tempfile::tempdir().unwrap();
    let legacy_root = tmp.path().join("legacy");
    let mu_root = tmp.path().join("mu");
    std::fs::create_dir_all(&legacy_root).unwrap();
    assert_eq!(
        WorkerConfig::resolve_default_workspace_dir(&mu_root, &legacy_root),
        legacy_root.join("workspace")
    );
    std::fs::create_dir_all(&mu_root).unwrap();
    assert_eq!(
        WorkerConfig::resolve_default_workspace_dir(&mu_root, &legacy_root),
        mu_root.join("workspace")
    );

    let (tot_mb, used_mb) =
        parse_proc_meminfo("MemTotal: 8388608 kB\nMemAvailable: 6291456 kB\n").unwrap();
    assert_eq!(tot_mb, 8192);
    assert_eq!(used_mb, 2048);

    let limiter = NodeConcurrencyLimiter::new(1);
    let guard = limiter.try_acquire(true, 1).unwrap();
    assert!(limiter.try_acquire(false, 1).is_err());
    drop(guard);

    // 3. Fail-closed validation of MU_SMOKE_* file parameters
    let missing_tok = load_smoke_config_with(|k| match k {
        "MU_SMOKE_BEARER_TOKEN_FILE" => Some("/nonexistent/worker-smoke-token".to_string()),
        _ => None,
    })
    .unwrap_err();
    assert!(missing_tok.contains("MU_SMOKE_BEARER_TOKEN_FILE"));

    let missing_ca = load_smoke_config_with(|k| match k {
        "MU_SMOKE_TLS_CA_FILE" => Some("/nonexistent/worker-smoke-ca.crt".to_string()),
        _ => None,
    })
    .unwrap_err();
    assert!(missing_ca.contains("MU_SMOKE_TLS_CA_FILE"));

    let partial_mtls = load_smoke_config_with(|k| match k {
        "MU_SMOKE_TLS_KEY_FILE" => Some("/tmp/only-key.pem".to_string()),
        _ => None,
    })
    .unwrap_err();
    assert!(partial_mtls.contains("MU_SMOKE_TLS_CERT_FILE"));

    let token_file = tmp.path().join("bearer.token");
    std::fs::write(&token_file, "  worker-smoke-token \n").unwrap();
    let ok = load_smoke_config_with(|k| match k {
        "MU_SMOKE_BEARER_TOKEN_FILE" => Some(token_file.to_string_lossy().into_owned()),
        _ => None,
    })
    .unwrap();
    assert_eq!(ok.bearer_token.as_deref(), Some("worker-smoke-token"));
    assert!(ok.tls_ca_pem.is_none());
    assert!(ok.tls_cert_pem.is_none());
    assert!(ok.tls_key_pem.is_none());
    assert_eq!(ok.timeout, Duration::from_secs(10));
}
