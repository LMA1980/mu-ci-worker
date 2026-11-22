use std::path::{Path, PathBuf};
use tracing::{info, warn};

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub controller_url: String,
    pub cluster_token: String,
    pub node_id: String,
    pub arch: String,
    pub tags: Vec<String>,
    pub max_slots: u32,
    pub workspace_dir: PathBuf,
    pub toolchains: Vec<String>,
    pub use_btrfs: bool,
    pub isolation_mode: IsolationMode,
    pub rust_semver: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IsolationMode {
    None,
    SystemdRun,
}

/// Returns the first non-empty trimmed value among `vars` using a custom env lookup.
pub(crate) fn first_non_empty_in<F>(vars: &[&str], get_env: &F) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    for var in vars {
        if let Some(val) = get_env(var) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// Parses the first valid value of `T` among `vars` using a custom env lookup.
pub(crate) fn first_parsed_in<T, F>(vars: &[&str], get_env: &F) -> Option<T>
where
    T: std::str::FromStr,
    F: Fn(&str) -> Option<String>,
{
    for var in vars {
        if let Some(val) = get_env(var) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                if let Ok(parsed) = trimmed.parse::<T>() {
                    return Some(parsed);
                }
            }
        }
    }
    None
}

/// Returns the first non-empty trimmed value among `vars`.
pub(crate) fn first_non_empty_env(vars: &[&str]) -> Option<String> {
    first_non_empty_in(vars, &|k| std::env::var(k).ok())
}

/// Parses the first valid value of `T` among `vars`.
pub(crate) fn first_parsed_env<T: std::str::FromStr>(vars: &[&str]) -> Option<T> {
    first_parsed_in(vars, &|k| std::env::var(k).ok())
}

impl WorkerConfig {
    /// Resolve the default workspace directory, preferring the canonical `mu-ci-worker` root
    /// when present on disk, falling back to the legacy `action-runner` root if only it exists,
    /// and otherwise defaulting to `mu_root/workspace`.
    #[must_use]
    pub fn resolve_default_workspace_dir(mu_root: &Path, legacy_root: &Path) -> PathBuf {
        if mu_root.is_dir() {
            mu_root.join("workspace")
        } else if legacy_root.is_dir() {
            legacy_root.join("workspace")
        } else {
            mu_root.join("workspace")
        }
    }

    /// Resolve the `sccache` environment file path, honoring an explicit override first,
    /// then `mu_path`, then `legacy_path`, defaulting to `mu_path` when neither exists on disk.
    #[must_use]
    pub fn resolve_sccache_env_file_from(
        explicit: Option<&str>,
        mu_path: &Path,
        legacy_path: &Path,
    ) -> PathBuf {
        if let Some(raw) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
            return PathBuf::from(raw);
        }
        if mu_path.is_file() {
            return mu_path.to_path_buf();
        }
        if legacy_path.is_file() {
            return legacy_path.to_path_buf();
        }
        mu_path.to_path_buf()
    }

    pub fn load() -> Self {
        Self::load_with_env(|k| std::env::var(k).ok())
    }

    pub fn load_with_env<F>(get_env: F) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        let controller_url = first_non_empty_in(
            &[
                "MU_CI_WORKER_CONTROLLER_URL",
                "ACTION_RUNNER_CONTROLLER_URL",
            ],
            &get_env,
        )
        .unwrap_or_else(|| "ws://127.0.0.1:8000/api/cluster/tunnel".to_string());

        let cluster_token = Self::resolve_secret_first_with_env(
            &["MU_CI_WORKER_CLUSTER_TOKEN", "ACTION_RUNNER_CLUSTER_TOKEN"],
            &get_env,
        )
        .unwrap_or_else(|| "action-runner-default-cluster-token".to_string());

        let node_id =
            first_non_empty_in(&["MU_CI_WORKER_NODE_ID", "ACTION_RUNNER_NODE_ID"], &get_env)
                .unwrap_or_else(hostname);

        let arch = first_non_empty_in(&["MU_CI_WORKER_ARCH", "ACTION_RUNNER_ARCH"], &get_env)
            .unwrap_or_else(|| std::env::consts::ARCH.to_string());

        let default_tags = default_tags_for_arch(&arch);

        let mut tags: Vec<String> =
            first_non_empty_in(&["MU_CI_WORKER_TAGS", "ACTION_RUNNER_TAGS"], &get_env)
                .unwrap_or_else(|| default_tags.to_string())
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();

        if Self::detect_sccache_bin_with_env(&get_env).is_some()
            && !tags.iter().any(|t| t == "sccache")
        {
            tags.push("sccache".to_string());
        }

        let max_slots = first_parsed_in::<u32, _>(
            &[
                "MU_CI_WORKER_MAX_SLOTS",
                "MU_CI_WORKER_SLOTS",
                "ACTION_RUNNER_MAX_SLOTS",
                "ACTION_RUNNER_SLOTS",
            ],
            &get_env,
        )
        .unwrap_or(1)
        .max(1);

        let workspace_dir = first_non_empty_in(
            &["MU_CI_WORKER_WORKSPACE", "ACTION_RUNNER_WORKSPACE"],
            &get_env,
        )
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Self::resolve_default_workspace_dir(
                Path::new("/var/lib/mu-ci-worker"),
                Path::new("/var/lib/action-runner"),
            )
        });

        let toolchains = Self::detect_toolchains(&arch);

        let use_btrfs = first_non_empty_in(
            &["MU_CI_WORKER_USE_BTRFS", "ACTION_RUNNER_USE_BTRFS"],
            &get_env,
        )
        .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
        .unwrap_or_else(|| Self::detect_btrfs(&workspace_dir));

        let isolation_mode = match first_non_empty_in(
            &["MU_CI_WORKER_ISOLATION", "ACTION_RUNNER_ISOLATION"],
            &get_env,
        )
        .unwrap_or_else(|| "none".to_string())
        .to_ascii_lowercase()
        .as_str()
        {
            "systemd-run" => IsolationMode::SystemdRun,
            _ => IsolationMode::None,
        };

        let rust_semver = Self::probe_rustc_semver_with_env(&get_env);

        let cfg = Self {
            controller_url,
            cluster_token,
            node_id,
            arch,
            tags,
            max_slots,
            workspace_dir,
            toolchains,
            use_btrfs,
            isolation_mode,
            rust_semver,
        };

        info!(
            node_id = %cfg.node_id,
            arch = %cfg.arch,
            max_slots = cfg.max_slots,
            max_threads = cfg.max_threads_with_env(&get_env),
            tags = ?cfg.tags,
            toolchains = ?cfg.toolchains,
            rust_semver = ?cfg.rust_semver,
            btrfs = cfg.use_btrfs,
            "Loaded mu-ci-worker configuration"
        );

        cfg
    }

    /// Probes installed LLVM-24 and GCC-17 toolchains (native, RVA23, SpacemiT X100/A100,
    /// `x86_64`, and `aarch64`) for worker capability advertisement.
    #[must_use]
    pub fn detect_toolchains(arch: &str) -> Vec<String> {
        Self::detect_toolchains_with_probe(arch, |p| Path::new(p).exists())
    }

    #[must_use]
    pub fn detect_toolchains_with_probe<F>(arch: &str, mut exists: F) -> Vec<String>
    where
        F: FnMut(&str) -> bool,
    {
        let mut toolchains = vec!["system".to_string()];
        Self::detect_llvm_toolchains(arch, &mut exists, &mut toolchains);
        Self::detect_gcc_toolchains(arch, &mut exists, &mut toolchains);
        toolchains
    }

    fn push_unique_toolchain(toolchains: &mut Vec<String>, name: &str) {
        if !toolchains.iter().any(|t| t == name) {
            toolchains.push(name.to_string());
        }
    }

    fn detect_llvm_toolchains<F>(arch: &str, exists: &mut F, toolchains: &mut Vec<String>)
    where
        F: FnMut(&str) -> bool,
    {
        const LLVM_DIRS: [&str; 5] = [
            "/opt/llvm-24/bin",
            "/opt/llvm-24-spacemit-x100/bin",
            "/opt/llvm-24-spacemit-a100/bin",
            "/home/lma1980/opt/llvm-24/bin",
            "/opt/cross-llvm-24/bin",
        ];
        if !LLVM_DIRS.iter().any(|d| exists(&format!("{d}/clang"))) {
            return;
        }
        Self::push_unique_toolchain(toolchains, "llvm-24");
        if exists("/opt/llvm-24/bin/clang") || exists("/home/lma1980/opt/llvm-24/bin/clang") {
            Self::push_unique_toolchain(toolchains, "llvm-24-native");
        }
        if LLVM_DIRS.iter().any(|d| {
            exists(&format!("{d}/riscv64-rva23-linux-gnu-clang"))
                || exists(&format!("{d}/riscv64-linux-gnu-clang"))
                || exists(&format!("{d}/riscv64-unknown-linux-gnu-clang"))
        }) {
            Self::push_unique_toolchain(toolchains, "llvm-24-rva23");
        }
        if exists("/opt/llvm-24-spacemit-x100/bin/clang")
            || LLVM_DIRS
                .iter()
                .any(|d| exists(&format!("{d}/riscv64-spacemit-x100-linux-gnu-clang")))
        {
            Self::push_unique_toolchain(toolchains, "llvm-24-spacemit-x100");
        }
        if exists("/opt/llvm-24-spacemit-a100/bin/clang")
            || LLVM_DIRS
                .iter()
                .any(|d| exists(&format!("{d}/riscv64-spacemit-a100-linux-gnu-clang")))
        {
            Self::push_unique_toolchain(toolchains, "llvm-24-spacemit-a100");
        }
        if (arch == "x86_64" && exists("/opt/llvm-24/bin/clang"))
            || LLVM_DIRS.iter().any(|d| {
                exists(&format!("{d}/x86_64-linux-gnu-clang"))
                    || exists(&format!("{d}/x86_64-unknown-linux-gnu-clang"))
            })
        {
            Self::push_unique_toolchain(toolchains, "llvm-24-x86_64");
        }
        if (is_arm_arch(arch) && exists("/opt/llvm-24/bin/clang"))
            || LLVM_DIRS.iter().any(|d| {
                exists(&format!("{d}/aarch64-linux-gnu-clang"))
                    || exists(&format!("{d}/aarch64-unknown-linux-gnu-clang"))
            })
        {
            Self::push_unique_toolchain(toolchains, "llvm-24-aarch64");
        }
    }

    fn detect_gcc_toolchains<F>(arch: &str, exists: &mut F, toolchains: &mut Vec<String>)
    where
        F: FnMut(&str) -> bool,
    {
        const GCC_DIRS: [&str; 6] = [
            "/opt/gcc-17/bin",
            "/opt/gcc-17-spacemit-x100/bin",
            "/opt/gcc-17-spacemit-a100/bin",
            "/home/lma1980/opt/gcc-17-spacemit-x100/bin",
            "/home/lma1980/opt/gcc-17/bin",
            "/opt/cross-gcc-17/bin",
        ];
        let has_native =
            exists("/opt/gcc-17/bin/gcc") || exists("/home/lma1980/opt/gcc-17/bin/gcc");
        let has_rva23 = GCC_DIRS.iter().any(|d| {
            exists(&format!("{d}/riscv64-rva23-linux-gnu-gcc"))
                || exists(&format!("{d}/riscv64-linux-gnu-gcc"))
                || exists(&format!("{d}/riscv64-unknown-linux-gnu-gcc"))
        });
        let has_x100 = exists("/opt/gcc-17-spacemit-x100/bin/gcc")
            || exists("/opt/gcc-17-spacemit-x100/bin/riscv64-unknown-linux-gnu-gcc")
            || exists("/home/lma1980/opt/gcc-17-spacemit-x100/bin/gcc")
            || exists("/home/lma1980/opt/gcc-17-spacemit-x100/bin/riscv64-unknown-linux-gnu-gcc")
            || exists("/opt/cross-gcc-17/bin/riscv64-unknown-linux-gnu-gcc")
            || GCC_DIRS
                .iter()
                .any(|d| exists(&format!("{d}/riscv64-spacemit-x100-linux-gnu-gcc")));
        let has_a100 = exists("/opt/gcc-17-spacemit-a100/bin/gcc")
            || GCC_DIRS
                .iter()
                .any(|d| exists(&format!("{d}/riscv64-spacemit-a100-linux-gnu-gcc")));
        let has_x86_64 = (arch == "x86_64" && has_native)
            || GCC_DIRS.iter().any(|d| {
                exists(&format!("{d}/x86_64-linux-gnu-gcc"))
                    || exists(&format!("{d}/x86_64-unknown-linux-gnu-gcc"))
            });
        let has_aarch64 = (is_arm_arch(arch) && has_native)
            || GCC_DIRS.iter().any(|d| {
                exists(&format!("{d}/aarch64-linux-gnu-gcc"))
                    || exists(&format!("{d}/aarch64-unknown-linux-gnu-gcc"))
            });

        if has_native || has_rva23 || has_x100 || has_a100 || has_x86_64 || has_aarch64 {
            Self::push_unique_toolchain(toolchains, "gcc-17");
        }
        for (present, token) in [
            (has_native, "gcc-17-native"),
            (has_rva23, "gcc-17-rva23"),
            (has_x100, "gcc-17-spacemit-x100"),
            (has_a100, "gcc-17-spacemit-a100"),
            (has_x86_64, "gcc-17-x86_64"),
            (has_aarch64, "gcc-17-aarch64"),
        ] {
            if present {
                Self::push_unique_toolchain(toolchains, token);
            }
        }
    }

    pub fn max_threads(&self) -> u32 {
        self.max_threads_with_env(|k| std::env::var(k).ok())
    }

    pub fn max_threads_with_env<F>(&self, get_env: F) -> u32
    where
        F: Fn(&str) -> Option<String>,
    {
        let default_threads = if self.node_id.eq_ignore_ascii_case("node-a")
            || self.tags.iter().any(|t| t.eq_ignore_ascii_case("n100"))
        {
            2
        } else {
            6
        };

        first_parsed_in::<u32, _>(
            &[
                "MU_CI_WORKER_THREADS",
                "ACTION_RUNNER_THREADS",
                "CARGO_BUILD_JOBS",
            ],
            &get_env,
        )
        .unwrap_or(default_threads)
        .max(1)
    }

    /// Pure evaluation of whether `tool_name` is disabled for a worker `arch` and optional
    /// `MU_CI_WORKER_DISABLED_TOOLS` / `ACTION_RUNNER_DISABLED_TOOLS` override.
    ///
    /// By default (`disabled_env == None`), `cargo-miri` is disabled on ARM workers
    /// (`aarch64` / `arm*`). Setting the override to `"none"` / `"off"` / `"false"`
    /// clears the default denylist; setting a comma-separated list disables each listed tool.
    #[must_use]
    pub fn is_tool_disabled_for(arch: &str, tool_name: &str, disabled_env: Option<&str>) -> bool {
        let tool = tool_name.trim();
        if tool.is_empty() {
            return false;
        }
        match disabled_env.map(str::trim).filter(|s| !s.is_empty()) {
            Some(raw)
                if matches!(
                    raw.to_ascii_lowercase().as_str(),
                    "none" | "off" | "false" | "0"
                ) =>
            {
                false
            }
            Some(raw) => raw
                .split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .any(|t| t.eq_ignore_ascii_case(tool)),
            None => is_arm_arch(arch) && tool.eq_ignore_ascii_case("cargo-miri"),
        }
    }

    #[must_use]
    pub fn is_tool_disabled(&self, tool_name: &str) -> bool {
        self.is_tool_disabled_with_env(tool_name, |k| std::env::var(k).ok())
    }

    #[must_use]
    pub fn is_tool_disabled_with_env<F>(&self, tool_name: &str, get_env: F) -> bool
    where
        F: Fn(&str) -> Option<String>,
    {
        let env_val = first_non_empty_in(
            &[
                "MU_CI_WORKER_DISABLED_TOOLS",
                "ACTION_RUNNER_DISABLED_TOOLS",
            ],
            &get_env,
        );
        Self::is_tool_disabled_for(&self.arch, tool_name, env_val.as_deref())
    }

    fn detect_btrfs(path: &Path) -> bool {
        // Quick check via statvfs/df if path or parent is btrfs
        if let Ok(output) = std::process::Command::new("findmnt")
            .arg("-n")
            .arg("-o")
            .arg("FSTYPE")
            .arg("-T")
            .arg(path)
            .output()
        {
            if let Ok(fs) = std::str::from_utf8(&output.stdout) {
                return fs.trim().eq_ignore_ascii_case("btrfs");
            }
        }
        false
    }

    pub fn detect_sccache_bin() -> Option<PathBuf> {
        Self::detect_sccache_bin_with_env(|k| std::env::var(k).ok())
    }

    pub fn detect_sccache_bin_with_env<F>(get_env: F) -> Option<PathBuf>
    where
        F: Fn(&str) -> Option<String>,
    {
        if let Some(trimmed) = first_non_empty_in(
            &["MU_CI_WORKER_SCCACHE_BIN", "ACTION_RUNNER_SCCACHE_BIN"],
            &get_env,
        ) {
            if trimmed.eq_ignore_ascii_case("false")
                || trimmed.eq_ignore_ascii_case("off")
                || trimmed.eq_ignore_ascii_case("none")
                || trimmed == "0"
            {
                return None;
            }
            let p = PathBuf::from(trimmed);
            if p.is_file() {
                return Some(p);
            }
            return None;
        }

        for candidate in ["/usr/local/bin/sccache", "/usr/bin/sccache"] {
            let p = Path::new(candidate);
            if p.is_file() {
                return Some(p.to_path_buf());
            }
        }

        if let Some(home) = get_env("CARGO_HOME") {
            let p = Path::new(&home).join("bin/sccache");
            if p.is_file() {
                return Some(p);
            }
        }
        if let Some(home) = get_env("HOME") {
            let p = Path::new(&home).join(".cargo/bin/sccache");
            if p.is_file() {
                return Some(p);
            }
        }
        None
    }

    pub fn load_sccache_env() -> Vec<(String, String)> {
        Self::load_sccache_env_with_env(|k| std::env::var(k).ok())
    }

    pub fn load_sccache_env_with_env<F>(get_env: F) -> Vec<(String, String)>
    where
        F: Fn(&str) -> Option<String>,
    {
        let mut envs = Vec::new();
        let explicit = first_non_empty_in(
            &[
                "MU_CI_WORKER_SCCACHE_ENV_FILE",
                "ACTION_RUNNER_SCCACHE_ENV_FILE",
            ],
            &get_env,
        );
        let env_file = Self::resolve_sccache_env_file_from(
            explicit.as_deref(),
            Path::new("/etc/mu-ci-worker/sccache.env"),
            Path::new("/etc/action-runner/sccache.env"),
        );
        if let Ok(content) = std::fs::read_to_string(&env_file) {
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                if let Some((k, v)) = trimmed.split_once('=') {
                    let key = k.trim().trim_start_matches("export ").trim().to_string();
                    let val = v.trim().trim_matches('"').trim_matches('\'').to_string();
                    if !key.is_empty()
                        && !key.eq_ignore_ascii_case("RUSTC_WRAPPER")
                        && !key.eq_ignore_ascii_case("CARGO_BUILD_RUSTC_WRAPPER")
                    {
                        envs.push((key, val));
                    }
                }
            }
        }
        for key in [
            "SCCACHE_BUCKET",
            "SCCACHE_REGION",
            "SCCACHE_ENDPOINT",
            "SCCACHE_S3_USE_SSL",
            "SCCACHE_S3_KEY_PREFIX",
            "SCCACHE_DIR",
            "SCCACHE_CACHE_SIZE",
            "SCCACHE_SERVER_PORT",
            "SCCACHE_SERVER_UDS",
            "SCCACHE_IDLE_TIMEOUT",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
        ] {
            if let Some(val) = get_env(key) {
                if !val.trim().is_empty() {
                    if let Some(pos) = envs.iter().position(|(k, _)| k == key) {
                        envs[pos].1 = val;
                    } else {
                        envs.push((key.to_string(), val));
                    }
                }
            }
        }
        envs
    }

    pub fn resolve_secret(env_var: &str) -> Option<String> {
        let val = std::env::var(env_var).ok()?;
        Self::resolve_secret_value(&val)
    }

    /// Resolve a secret from the first non-empty environment variable in `vars`.
    /// Fails closed (`None`) if the selected variable points to an unreadable or empty path.
    pub fn resolve_secret_first(vars: &[&str]) -> Option<String> {
        Self::resolve_secret_first_with_env(vars, |k| std::env::var(k).ok())
    }

    pub fn resolve_secret_first_with_env<F>(vars: &[&str], get_env: F) -> Option<String>
    where
        F: Fn(&str) -> Option<String>,
    {
        for var in vars {
            if let Some(val) = get_env(var) {
                if !val.trim().is_empty() {
                    return Self::resolve_secret_value(&val);
                }
            }
        }
        None
    }

    pub fn resolve_secret_value(val: &str) -> Option<String> {
        let val = val.trim();
        if val.is_empty() {
            return None;
        }

        let is_path = val.starts_with('/')
            || val.starts_with("./")
            || val.starts_with("../")
            || val.contains('/')
            || val.contains('\\');

        let path = Path::new(val);
        if path.is_file() {
            match std::fs::read_to_string(path) {
                Ok(content) => {
                    let secret = content.trim().to_string();
                    if secret.is_empty() {
                        warn!("Secret file at {} is empty", path.display());
                        None
                    } else {
                        info!("Loaded secret from {}", path.display());
                        Some(secret)
                    }
                }
                Err(err) => {
                    warn!("Failed to read secret file at {}: {}", path.display(), err);
                    None
                }
            }
        } else if is_path {
            warn!(
                "Secret path '{}' does not exist or is not a readable file; failing closed",
                val
            );
            None
        } else {
            Some(val.to_string())
        }
    }

    /// Resolve the `rustc` binary path while honoring `MU_CI_WORKER_RUSTC_BIN` /
    /// `ACTION_RUNNER_RUSTC_BIN`, `CARGO_HOME`, `PATH`, `RUSTUP_HOME`, `HOME`,
    /// and service-account fallbacks (`/var/lib/mu-ci-worker`, `/var/lib/action-runner`).
    pub fn resolve_rustc_bin_from(
        explicit_bin: Option<&Path>,
        cargo_home: Option<&Path>,
        rustup_home: Option<&Path>,
        path_env: Option<&std::ffi::OsStr>,
        home: Option<&Path>,
    ) -> PathBuf {
        Self::resolve_rustc_bin_from_roots(
            explicit_bin,
            cargo_home,
            rustup_home,
            path_env,
            home,
            &[
                Path::new("/var/lib/mu-ci-worker"),
                Path::new("/var/lib/action-runner"),
            ],
        )
    }

    pub(crate) fn resolve_rustc_bin_from_roots(
        explicit_bin: Option<&Path>,
        cargo_home: Option<&Path>,
        rustup_home: Option<&Path>,
        path_env: Option<&std::ffi::OsStr>,
        home: Option<&Path>,
        service_roots: &[&Path],
    ) -> PathBuf {
        if let Some(explicit) = explicit_bin.filter(|p| !p.as_os_str().is_empty()) {
            return explicit.to_path_buf();
        }

        if let Some(ch) = cargo_home.filter(|p| !p.as_os_str().is_empty()) {
            let candidate = ch.join("bin").join("rustc");
            if candidate.is_file() {
                return candidate;
            }
        }

        if let Some(pe) = path_env {
            for dir in std::env::split_paths(pe) {
                let candidate = dir.join("rustc");
                if candidate.is_file() {
                    return candidate;
                }
            }
        }

        if let Some(rh) = rustup_home.filter(|p| !p.as_os_str().is_empty()) {
            if let Some(parent) = rh.parent() {
                let candidate = parent.join(".cargo").join("bin").join("rustc");
                if candidate.is_file() {
                    return candidate;
                }
            }
        }

        if let Some(h) = home.filter(|p| !p.as_os_str().is_empty()) {
            let candidate = h.join(".cargo").join("bin").join("rustc");
            if candidate.is_file() {
                return candidate;
            }
        }

        for root in service_roots {
            let service_candidate = root.join(".cargo").join("bin").join("rustc");
            if service_candidate.is_file() {
                return service_candidate;
            }
        }

        PathBuf::from("rustc")
    }

    pub(crate) fn infer_service_toolchain_homes(
        rustc_bin: &Path,
        rustup_home: Option<PathBuf>,
        cargo_home: Option<PathBuf>,
        service_roots: &[&Path],
    ) -> (Option<PathBuf>, Option<PathBuf>) {
        let effective_rustup_home = rustup_home.or_else(|| {
            service_roots.iter().find_map(|root| {
                let rustup = root.join(".rustup");
                if rustc_bin.starts_with(root) && rustup.is_dir() {
                    Some(rustup)
                } else {
                    None
                }
            })
        });
        let effective_cargo_home = cargo_home.or_else(|| {
            service_roots.iter().find_map(|root| {
                let cargo = root.join(".cargo");
                if rustc_bin.starts_with(root) && cargo.is_dir() {
                    Some(cargo)
                } else {
                    None
                }
            })
        });
        (effective_rustup_home, effective_cargo_home)
    }

    /// Detect the `rustc` binary using the current process environment.
    pub fn detect_rustc_bin() -> PathBuf {
        Self::detect_rustc_bin_with_env(|k| std::env::var(k).ok())
    }

    pub fn detect_rustc_bin_with_env<F>(get_env: F) -> PathBuf
    where
        F: Fn(&str) -> Option<String>,
    {
        let explicit = first_non_empty_in(
            &["MU_CI_WORKER_RUSTC_BIN", "ACTION_RUNNER_RUSTC_BIN"],
            &get_env,
        )
        .map(PathBuf::from);
        let cargo_home = get_env("CARGO_HOME").map(PathBuf::from);
        let rustup_home = get_env("RUSTUP_HOME").map(PathBuf::from);
        let path_env = get_env("PATH").map(std::ffi::OsString::from);
        let home = get_env("HOME").map(PathBuf::from);
        Self::resolve_rustc_bin_from(
            explicit.as_deref(),
            cargo_home.as_deref(),
            rustup_home.as_deref(),
            path_env.as_deref(),
            home.as_deref(),
        )
    }

    /// Construct the `rustc --version` probe command with optional `RUSTUP_HOME` and `CARGO_HOME`.
    pub fn build_rustc_probe_command(
        rustc_bin: &Path,
        rustup_home: Option<&Path>,
        cargo_home: Option<&Path>,
    ) -> std::process::Command {
        let mut cmd = std::process::Command::new(rustc_bin);
        cmd.arg("--version");
        if let Some(home) = rustup_home.filter(|p| !p.as_os_str().is_empty()) {
            cmd.env("RUSTUP_HOME", home);
        }
        if let Some(home) = cargo_home.filter(|p| !p.as_os_str().is_empty()) {
            cmd.env("CARGO_HOME", home);
        }
        cmd
    }

    /// Probe the active `rustc --version` under the current process or service-account environment.
    ///
    /// Fails soft (`None`) with a warning log if `rustc` cannot be executed or parsed.
    pub fn probe_rustc_semver() -> Option<String> {
        Self::probe_rustc_semver_with_env(|k| std::env::var(k).ok())
    }

    pub fn probe_rustc_semver_with_env<F>(get_env: F) -> Option<String>
    where
        F: Fn(&str) -> Option<String>,
    {
        let explicit = first_non_empty_in(
            &["MU_CI_WORKER_RUSTC_BIN", "ACTION_RUNNER_RUSTC_BIN"],
            &get_env,
        )
        .map(PathBuf::from);
        let cargo_home = get_env("CARGO_HOME")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty());
        let rustup_home = get_env("RUSTUP_HOME")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty());
        let path_env = get_env("PATH").map(std::ffi::OsString::from);
        let home = get_env("HOME").map(PathBuf::from);

        let rustc_bin = Self::resolve_rustc_bin_from(
            explicit.as_deref(),
            cargo_home.as_deref(),
            rustup_home.as_deref(),
            path_env.as_deref(),
            home.as_deref(),
        );

        let (effective_rustup_home, effective_cargo_home) = Self::infer_service_toolchain_homes(
            &rustc_bin,
            rustup_home,
            cargo_home,
            &[
                Path::new("/var/lib/mu-ci-worker"),
                Path::new("/var/lib/action-runner"),
            ],
        );

        Self::probe_rustc_semver_for(
            &rustc_bin,
            effective_rustup_home.as_deref(),
            effective_cargo_home.as_deref(),
        )
    }

    /// Probe `rustc --version` for a specific binary path and optional `RUSTUP_HOME` / `CARGO_HOME`.
    pub fn probe_rustc_semver_for(
        rustc_bin: &Path,
        rustup_home: Option<&Path>,
        cargo_home: Option<&Path>,
    ) -> Option<String> {
        let mut cmd = Self::build_rustc_probe_command(rustc_bin, rustup_home, cargo_home);
        Self::probe_rustc_semver_with(rustc_bin, || cmd.output())
    }

    fn probe_rustc_semver_with<F>(rustc_bin: &Path, run: F) -> Option<String>
    where
        F: FnOnce() -> std::io::Result<std::process::Output>,
    {
        match run() {
            Ok(output) => {
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    warn!(
                        bin = %rustc_bin.display(),
                        status = %output.status,
                        stderr = %stderr.trim(),
                        "rustc --version exited unsuccessfully during startup probe; continuing with rust_semver = None"
                    );
                    return None;
                }
                match std::str::from_utf8(&output.stdout) {
                    Ok(stdout) => match parse_rustc_semver(stdout) {
                        Some(ver) => Some(ver),
                        None => {
                            warn!(
                                bin = %rustc_bin.display(),
                                stdout = %stdout.trim(),
                                "Failed to parse semver from rustc --version output; continuing with rust_semver = None"
                            );
                            None
                        }
                    },
                    Err(err) => {
                        warn!(
                            bin = %rustc_bin.display(),
                            error = %err,
                            "rustc --version emitted non-UTF-8 stdout; continuing with rust_semver = None"
                        );
                        None
                    }
                }
            }
            Err(err) => {
                warn!(
                    bin = %rustc_bin.display(),
                    error = %err,
                    "Unable to invoke rustc --version during startup probe; continuing with rust_semver = None"
                );
                None
            }
        }
    }
}

/// Parses the semantic version token from `rustc --version` output.
///
/// Supports stable (`rustc 1.86.0 (...)`), beta (`rustc 1.87.0-beta.2 (...)`),
/// nightly (`rustc 1.88.0-nightly (...)`), and dev (`rustc 1.88.0-dev`) outputs.
/// Returns `None` for empty, non-`rustc`, or malformed version strings.
pub fn parse_rustc_semver(output: &str) -> Option<String> {
    let line = output.lines().map(str::trim).find(|l| !l.is_empty())?;
    let mut parts = line.split_whitespace();
    if parts.next()? != "rustc" {
        return None;
    }
    let version_token = parts.next()?;
    if !is_valid_semver(version_token) {
        return None;
    }
    Some(version_token.to_string())
}

fn is_valid_semver_ident(ident: &str, allow_leading_zero: bool) -> bool {
    if ident.is_empty() {
        return false;
    }
    if !allow_leading_zero
        && ident.len() > 1
        && ident.starts_with('0')
        && ident.bytes().all(|b| b.is_ascii_digit())
    {
        return false;
    }
    ident
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn is_valid_numeric_component(part: &str) -> bool {
    if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    if part.len() > 1 && part.starts_with('0') {
        return false;
    }
    part.parse::<u64>().is_ok()
}

fn is_valid_semver(version: &str) -> bool {
    let (core_and_pre, build_opt) = match version.split_once('+') {
        Some((before, build)) => (before, Some(build)),
        None => (version, None),
    };
    if let Some(build) = build_opt {
        if build.is_empty() || !build.split('.').all(|id| is_valid_semver_ident(id, true)) {
            return false;
        }
    }

    let (core, pre_opt) = match core_and_pre.split_once('-') {
        Some((before, pre)) => (before, Some(pre)),
        None => (core_and_pre, None),
    };
    if let Some(pre) = pre_opt {
        if pre.is_empty()
            || !pre.split('.').all(|id| {
                let numeric = id.bytes().all(|b| b.is_ascii_digit());
                is_valid_semver_ident(id, !numeric)
            })
        {
            return false;
        }
    }

    let mut core_parts = core.split('.');
    let (Some(major), Some(minor), Some(patch), None) = (
        core_parts.next(),
        core_parts.next(),
        core_parts.next(),
        core_parts.next(),
    ) else {
        return false;
    };
    is_valid_numeric_component(major)
        && is_valid_numeric_component(minor)
        && is_valid_numeric_component(patch)
}

/// Returns `true` when a worker architecture string identifies an ARM (`aarch64` / `arm*`) host.
#[must_use]
pub fn is_arm_arch(arch: &str) -> bool {
    let lower = arch.trim().to_ascii_lowercase();
    matches!(lower.as_str(), "aarch64" | "arm64" | "arm")
        || lower.starts_with("aarch64-")
        || lower.starts_with("arm-")
        || lower.starts_with("armv")
}

/// Returns the default `ACTION_RUNNER_TAGS` string for a worker architecture when
/// `ACTION_RUNNER_TAGS` is unset.
#[must_use]
pub fn default_tags_for_arch(arch: &str) -> &'static str {
    match arch.trim() {
        "riscv64" | "riscv64gc" => "riscv64,native,k3,rvv,ime,bench",
        a if is_arm_arch(a) => "aarch64,arm64",
        _ => "x86_64,fmt,clippy,audit,cross,heavy",
    }
}

pub fn is_safe_repo_name(repo: &str) -> bool {
    let parts: Vec<&str> = repo.split('/').collect();
    if parts.len() != 2 {
        return false;
    }
    let owner = parts[0];
    let name = parts[1];
    if owner.is_empty() || name.is_empty() {
        return false;
    }
    if owner.contains("..") || name.contains("..") {
        return false;
    }
    let is_valid_char = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.';
    owner.chars().all(is_valid_char) && name.chars().all(is_valid_char)
}

fn hostname() -> String {
    if let Ok(name) = std::env::var("HOSTNAME") {
        if !name.is_empty() {
            return name;
        }
    }
    if let Ok(output) = std::process::Command::new("hostname").output() {
        if let Ok(name) = std::str::from_utf8(&output.stdout) {
            let trimmed = name.trim().to_string();
            if !trimmed.is_empty() {
                return trimmed;
            }
        }
    }
    "worker-node".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_secret_value_literal() {
        let literal = "my_literal_token_12345";
        assert_eq!(
            WorkerConfig::resolve_secret_value(literal),
            Some(literal.to_string())
        );
    }

    #[test]
    fn test_resolve_secret_value_from_valid_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("secret.key");
        std::fs::write(&file_path, "  my-secret-token \n\t").unwrap();

        assert_eq!(
            WorkerConfig::resolve_secret_value(file_path.to_str().unwrap()),
            Some("my-secret-token".to_string())
        );
    }

    #[test]
    fn test_resolve_secret_value_missing_path_fail_closed() {
        assert_eq!(
            WorkerConfig::resolve_secret_value("/nonexistent/cluster.token"),
            None,
            "Missing absolute secret file must fail closed to None"
        );
        assert_eq!(
            WorkerConfig::resolve_secret_value("./missing_cluster.token"),
            None,
            "Missing relative secret file must fail closed to None"
        );
        assert_eq!(
            WorkerConfig::resolve_secret_value("../missing_cluster.token"),
            None,
            "Parent traversal secret path must fail closed to None"
        );
    }

    #[test]
    fn test_resolve_secret_value_empty_file_fail_closed() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("empty.key");
        std::fs::write(&file_path, "   \n\t").unwrap();

        assert_eq!(
            WorkerConfig::resolve_secret_value(file_path.to_str().unwrap()),
            None,
            "Empty secret file must fail closed to None"
        );
    }

    #[test]
    fn test_resolve_secret_env_var_fail_closed() {
        let var_name = "TEST_WORKER_SECRET_FAIL_CLOSED_VAR";
        std::env::set_var(var_name, "/nonexistent/path/to/token.secret");
        assert_eq!(
            WorkerConfig::resolve_secret(var_name),
            None,
            "Environment variable pointing to nonexistent path must fail closed"
        );
        std::env::remove_var(var_name);
    }

    #[test]
    fn test_is_safe_repo_name() {
        assert!(is_safe_repo_name("LMA1980/model-runner"));
        assert!(is_safe_repo_name("owner-123/repo_456.test"));
        assert!(!is_safe_repo_name(""));
        assert!(!is_safe_repo_name("/etc/passwd"));
        assert!(!is_safe_repo_name("../etc/passwd"));
        assert!(!is_safe_repo_name("LMA1980/../passwd"));
        assert!(!is_safe_repo_name("LMA1980"));
        assert!(!is_safe_repo_name("LMA1980/repo/extra"));
        assert!(!is_safe_repo_name("LMA1980/repo;rm -rf /"));
    }

    #[test]
    fn test_parse_rustc_semver_stable() {
        assert_eq!(
            parse_rustc_semver("rustc 1.86.0 (05f9846f8 2025-03-31)\n"),
            Some("1.86.0".to_string())
        );
        assert_eq!(
            parse_rustc_semver("  rustc 1.99.1 (58cf8f9c3 2026-10-13)  \r\n"),
            Some("1.99.1".to_string())
        );
        assert_eq!(
            parse_rustc_semver("rustc 1.86.0"),
            Some("1.86.0".to_string())
        );
    }

    #[test]
    fn test_parse_rustc_semver_nightly_and_beta() {
        assert_eq!(
            parse_rustc_semver("rustc 1.88.0-nightly (123456789 2025-05-01)\n"),
            Some("1.88.0-nightly".to_string())
        );
        assert_eq!(
            parse_rustc_semver("rustc 1.87.0-beta.2 (abcdef123 2025-04-15)"),
            Some("1.87.0-beta.2".to_string())
        );
        assert_eq!(
            parse_rustc_semver("rustc 1.87.0-beta (abcdef123 2025-04-15)"),
            Some("1.87.0-beta".to_string())
        );
        assert_eq!(
            parse_rustc_semver("rustc 1.85.0-dev"),
            Some("1.85.0-dev".to_string())
        );
        assert_eq!(
            parse_rustc_semver("rustc 1.86.0+custom.1 (abcdef 2025-01-01)"),
            Some("1.86.0+custom.1".to_string())
        );
    }

    #[test]
    fn test_parse_rustc_semver_malformed() {
        for invalid in [
            "",
            "   \n\t ",
            "cargo 1.86.0 (05f9846f8 2025-03-31)",
            "rustc",
            "rustc 1.86",
            "rustc 1.86.0.1",
            "rustc v1.86.0 (05f9846f8 2025-03-31)",
            "rustc 1.86.x (05f9846f8 2025-03-31)",
            "rustc 01.86.0",
            "rustc 1.86.0-",
            "rustc 1.86.0-nightly.",
            "rustc 1.86.0+",
            "not a rustc banner",
        ] {
            assert_eq!(
                parse_rustc_semver(invalid),
                None,
                "Expected None for malformed output: {invalid:?}"
            );
        }
    }

    #[test]
    fn test_resolve_rustc_bin_and_probe_command_respect_service_account_env() {
        let dir = tempfile::tempdir().unwrap();
        let cargo_home = dir.path().join("cargo");
        let rustup_home = dir.path().join("rustup");
        let path_dir = dir.path().join("path-bin");
        let home_dir = dir.path().join("home");
        let sibling_cargo_bin = dir.path().join(".cargo/bin");

        std::fs::create_dir_all(cargo_home.join("bin")).unwrap();
        std::fs::create_dir_all(&rustup_home).unwrap();
        std::fs::create_dir_all(&path_dir).unwrap();
        std::fs::create_dir_all(home_dir.join(".cargo/bin")).unwrap();
        std::fs::create_dir_all(&sibling_cargo_bin).unwrap();

        let cargo_rustc = cargo_home.join("bin/rustc");
        let path_rustc = path_dir.join("rustc");
        let home_rustc = home_dir.join(".cargo/bin/rustc");
        let sibling_rustc = sibling_cargo_bin.join("rustc");
        std::fs::write(&cargo_rustc, b"").unwrap();
        std::fs::write(&path_rustc, b"").unwrap();
        std::fs::write(&home_rustc, b"").unwrap();
        std::fs::write(&sibling_rustc, b"").unwrap();

        let explicit = Path::new("/custom/bin/rustc");
        assert_eq!(
            WorkerConfig::resolve_rustc_bin_from(
                Some(explicit),
                Some(&cargo_home),
                Some(&rustup_home),
                Some(path_dir.as_os_str()),
                Some(&home_dir),
            ),
            explicit
        );

        assert_eq!(
            WorkerConfig::resolve_rustc_bin_from(
                None,
                Some(&cargo_home),
                Some(&rustup_home),
                Some(path_dir.as_os_str()),
                Some(&home_dir),
            ),
            cargo_rustc
        );

        assert_eq!(
            WorkerConfig::resolve_rustc_bin_from(
                None,
                Some(&dir.path().join("empty-cargo")),
                Some(&rustup_home),
                Some(path_dir.as_os_str()),
                Some(&home_dir),
            ),
            path_rustc
        );

        assert_eq!(
            WorkerConfig::resolve_rustc_bin_from(
                None,
                None,
                Some(&rustup_home),
                None,
                Some(&home_dir),
            ),
            sibling_rustc
        );

        let empty_rustup = dir.path().join("isolated/rustup");
        std::fs::create_dir_all(&empty_rustup).unwrap();
        assert_eq!(
            WorkerConfig::resolve_rustc_bin_from(
                None,
                None,
                Some(&empty_rustup),
                None,
                Some(&home_dir),
            ),
            home_rustc
        );

        let cmd = WorkerConfig::build_rustc_probe_command(
            &cargo_rustc,
            Some(&rustup_home),
            Some(&cargo_home),
        );
        assert_eq!(cmd.get_program(), cargo_rustc.as_os_str());
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, vec![std::ffi::OsStr::new("--version")]);
        let envs: std::collections::HashMap<_, _> = cmd.get_envs().collect();
        assert_eq!(
            envs.get(std::ffi::OsStr::new("RUSTUP_HOME"))
                .copied()
                .flatten(),
            Some(rustup_home.as_os_str())
        );
        assert_eq!(
            envs.get(std::ffi::OsStr::new("CARGO_HOME"))
                .copied()
                .flatten(),
            Some(cargo_home.as_os_str())
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_probe_rustc_semver_with_mock_outputs_and_fail_soft() {
        use std::os::unix::process::ExitStatusExt;

        let bin = Path::new("/var/lib/action-runner/.cargo/bin/rustc");

        let stable = WorkerConfig::probe_rustc_semver_with(bin, || {
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: b"rustc 1.86.0 (05f9846f8 2025-03-31)\n".to_vec(),
                stderr: Vec::new(),
            })
        });
        assert_eq!(stable, Some("1.86.0".to_string()));

        let nightly = WorkerConfig::probe_rustc_semver_with(bin, || {
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: b"rustc 1.88.0-nightly (123456789 2025-05-01)\n".to_vec(),
                stderr: Vec::new(),
            })
        });
        assert_eq!(nightly, Some("1.88.0-nightly".to_string()));

        let failed_exit = WorkerConfig::probe_rustc_semver_with(bin, || {
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(1 << 8),
                stdout: Vec::new(),
                stderr: b"error: no default toolchain configured".to_vec(),
            })
        });
        assert_eq!(failed_exit, None);

        let malformed_stdout = WorkerConfig::probe_rustc_semver_with(bin, || {
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: b"garbage output\n".to_vec(),
                stderr: Vec::new(),
            })
        });
        assert_eq!(malformed_stdout, None);

        let invalid_utf8 = WorkerConfig::probe_rustc_semver_with(bin, || {
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: vec![0xff, 0xfe],
                stderr: Vec::new(),
            })
        });
        assert_eq!(invalid_utf8, None);

        let spawn_err = WorkerConfig::probe_rustc_semver_with(bin, || {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "rustc missing",
            ))
        });
        assert_eq!(spawn_err, None);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn test_probe_rustc_semver_live_and_isolated_fail_soft() {
        // Missing binary fails soft cleanly without panicking.
        assert_eq!(
            WorkerConfig::probe_rustc_semver_for(
                Path::new("/nonexistent/action-runner-missing-rustc"),
                None,
                None,
            ),
            None
        );

        if let Some(semver) = WorkerConfig::probe_rustc_semver() {
            assert!(
                is_valid_semver(&semver),
                "Detected rustc semver must be valid semver: {semver}"
            );
        }
    }

    #[test]
    fn test_arm_default_tags_and_tool_denylist_under_miri() {
        assert!(is_arm_arch("aarch64"));
        assert!(is_arm_arch("arm64"));
        assert!(is_arm_arch("armv7l"));
        assert!(is_arm_arch("aarch64-unknown-linux-gnu"));
        assert!(!is_arm_arch("x86_64"));
        assert!(!is_arm_arch("riscv64"));

        assert_eq!(default_tags_for_arch("aarch64"), "aarch64,arm64");
        assert_eq!(default_tags_for_arch("arm64"), "aarch64,arm64");
        assert_eq!(
            default_tags_for_arch("riscv64"),
            "riscv64,native,k3,rvv,ime,bench"
        );
        assert_eq!(
            default_tags_for_arch("x86_64"),
            "x86_64,fmt,clippy,audit,cross,heavy"
        );

        // Default denylist disables cargo-miri on ARM workers only.
        assert!(WorkerConfig::is_tool_disabled_for(
            "aarch64",
            "cargo-miri",
            None
        ));
        assert!(WorkerConfig::is_tool_disabled_for(
            "arm64",
            "CARGO-MIRI",
            None
        ));
        assert!(!WorkerConfig::is_tool_disabled_for(
            "aarch64",
            "cargo-test",
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

        // Explicit ACTION_RUNNER_DISABLED_TOOLS list overrides default behavior.
        assert!(WorkerConfig::is_tool_disabled_for(
            "x86_64",
            "cargo-miri",
            Some("cargo-miri,cargo-bench")
        ));
        assert!(WorkerConfig::is_tool_disabled_for(
            "x86_64",
            "cargo-bench",
            Some("cargo-miri, cargo-bench")
        ));
        assert!(!WorkerConfig::is_tool_disabled_for(
            "aarch64",
            "cargo-miri",
            Some("cargo-bench")
        ));

        // Explicit opt-out clears the default denylist.
        for opt_out in ["none", "off", "false", "0"] {
            assert!(!WorkerConfig::is_tool_disabled_for(
                "aarch64",
                "cargo-miri",
                Some(opt_out)
            ));
        }
    }

    static WORKER_ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvGuard {
        saved: Vec<(String, Option<std::ffi::OsString>)>,
    }

    impl EnvGuard {
        fn capture(keys: &[&str]) -> Self {
            let saved = keys
                .iter()
                .map(|k| ((*k).to_string(), std::env::var_os(k)))
                .collect();
            Self { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    #[test]
    fn test_resolve_default_workspace_dir_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let mu_root = dir.path().join("mu-ci-worker");
        let legacy_root = dir.path().join("action-runner");

        // Neither exists -> canonical mu_root/workspace
        assert_eq!(
            WorkerConfig::resolve_default_workspace_dir(&mu_root, &legacy_root),
            mu_root.join("workspace")
        );

        // Only legacy exists -> legacy_root/workspace
        std::fs::create_dir_all(&legacy_root).unwrap();
        assert_eq!(
            WorkerConfig::resolve_default_workspace_dir(&mu_root, &legacy_root),
            legacy_root.join("workspace")
        );

        // Both exist -> mu_root/workspace wins
        std::fs::create_dir_all(&mu_root).unwrap();
        assert_eq!(
            WorkerConfig::resolve_default_workspace_dir(&mu_root, &legacy_root),
            mu_root.join("workspace")
        );
    }

    #[test]
    fn test_resolve_sccache_env_file_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let mu_file = dir.path().join("mu-ci-worker/sccache.env");
        let legacy_file = dir.path().join("action-runner/sccache.env");
        std::fs::create_dir_all(mu_file.parent().unwrap()).unwrap();
        std::fs::create_dir_all(legacy_file.parent().unwrap()).unwrap();

        // Neither exists -> defaults to mu_file
        assert_eq!(
            WorkerConfig::resolve_sccache_env_file_from(None, &mu_file, &legacy_file),
            mu_file
        );

        // Only legacy exists -> legacy_file
        std::fs::write(&legacy_file, "SCCACHE_BUCKET=legacy\n").unwrap();
        assert_eq!(
            WorkerConfig::resolve_sccache_env_file_from(None, &mu_file, &legacy_file),
            legacy_file
        );

        // Both exist -> mu_file wins
        std::fs::write(&mu_file, "SCCACHE_BUCKET=canonical\n").unwrap();
        assert_eq!(
            WorkerConfig::resolve_sccache_env_file_from(None, &mu_file, &legacy_file),
            mu_file
        );

        // Explicit override wins over disk files
        let explicit = dir.path().join("custom.env");
        assert_eq!(
            WorkerConfig::resolve_sccache_env_file_from(
                Some(explicit.to_str().unwrap()),
                &mu_file,
                &legacy_file
            ),
            explicit
        );
    }

    #[test]
    fn test_resolve_rustc_bin_and_infer_service_roots_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let mu_root = dir.path().join("var/lib/mu-ci-worker");
        let legacy_root = dir.path().join("var/lib/action-runner");

        std::fs::create_dir_all(legacy_root.join(".cargo/bin")).unwrap();
        std::fs::create_dir_all(legacy_root.join(".rustup")).unwrap();
        let legacy_rustc = legacy_root.join(".cargo/bin/rustc");
        std::fs::write(&legacy_rustc, b"").unwrap();

        // When only legacy service root has rustc, it is selected
        let resolved_legacy = WorkerConfig::resolve_rustc_bin_from_roots(
            None,
            None,
            None,
            None,
            None,
            &[mu_root.as_path(), legacy_root.as_path()],
        );
        assert_eq!(resolved_legacy, legacy_rustc);
        let (legacy_rustup_home, legacy_cargo_home) = WorkerConfig::infer_service_toolchain_homes(
            &resolved_legacy,
            None,
            None,
            &[mu_root.as_path(), legacy_root.as_path()],
        );
        assert_eq!(legacy_rustup_home, Some(legacy_root.join(".rustup")));
        assert_eq!(legacy_cargo_home, Some(legacy_root.join(".cargo")));

        // When canonical mu-ci-worker service root also has rustc, canonical wins
        std::fs::create_dir_all(mu_root.join(".cargo/bin")).unwrap();
        std::fs::create_dir_all(mu_root.join(".rustup")).unwrap();
        let mu_rustc = mu_root.join(".cargo/bin/rustc");
        std::fs::write(&mu_rustc, b"").unwrap();

        let resolved_mu = WorkerConfig::resolve_rustc_bin_from_roots(
            None,
            None,
            None,
            None,
            None,
            &[mu_root.as_path(), legacy_root.as_path()],
        );
        assert_eq!(resolved_mu, mu_rustc);
        let (mu_rustup_home, mu_cargo_home) = WorkerConfig::infer_service_toolchain_homes(
            &resolved_mu,
            None,
            None,
            &[mu_root.as_path(), legacy_root.as_path()],
        );
        assert_eq!(mu_rustup_home, Some(mu_root.join(".rustup")));
        assert_eq!(mu_cargo_home, Some(mu_root.join(".cargo")));
    }

    #[test]
    fn test_dual_env_helpers_and_fail_closed_secret_precedence() {
        let mut env_map: std::collections::HashMap<&str, String> = Default::default();

        // Legacy fallback when canonical unset
        env_map.insert("ACTION_RUNNER_CLUSTER_TOKEN", "legacy-token".to_string());
        assert_eq!(
            WorkerConfig::resolve_secret_first_with_env(
                &["MU_CI_WORKER_CLUSTER_TOKEN", "ACTION_RUNNER_CLUSTER_TOKEN"],
                |k| env_map.get(k).cloned()
            ),
            Some("legacy-token".to_string())
        );

        // Canonical overrides legacy
        env_map.insert("MU_CI_WORKER_CLUSTER_TOKEN", "canonical-token".to_string());
        assert_eq!(
            WorkerConfig::resolve_secret_first_with_env(
                &["MU_CI_WORKER_CLUSTER_TOKEN", "ACTION_RUNNER_CLUSTER_TOKEN"],
                |k| env_map.get(k).cloned()
            ),
            Some("canonical-token".to_string())
        );

        // Missing canonical secret path fails closed instead of falling back to legacy token
        env_map.insert(
            "MU_CI_WORKER_CLUSTER_TOKEN",
            "/nonexistent/cluster_token".to_string(),
        );
        assert_eq!(
            WorkerConfig::resolve_secret_first_with_env(
                &["MU_CI_WORKER_CLUSTER_TOKEN", "ACTION_RUNNER_CLUSTER_TOKEN"],
                |k| env_map.get(k).cloned()
            ),
            None
        );

        // Slots precedence: MU_CI_WORKER_MAX_SLOTS -> MU_CI_WORKER_SLOTS -> ACTION_RUNNER_MAX_SLOTS
        env_map.insert("ACTION_RUNNER_MAX_SLOTS", "4".to_string());
        assert_eq!(
            first_parsed_in::<u32, _>(
                &[
                    "MU_CI_WORKER_MAX_SLOTS",
                    "MU_CI_WORKER_SLOTS",
                    "ACTION_RUNNER_MAX_SLOTS",
                ],
                &|k| env_map.get(k).cloned()
            ),
            Some(4)
        );
        env_map.insert("MU_CI_WORKER_SLOTS", "2".to_string());
        assert_eq!(
            first_parsed_in::<u32, _>(
                &[
                    "MU_CI_WORKER_MAX_SLOTS",
                    "MU_CI_WORKER_SLOTS",
                    "ACTION_RUNNER_MAX_SLOTS",
                ],
                &|k| env_map.get(k).cloned()
            ),
            Some(2)
        );
        env_map.insert("MU_CI_WORKER_MAX_SLOTS", "3".to_string());
        assert_eq!(
            first_parsed_in::<u32, _>(
                &[
                    "MU_CI_WORKER_MAX_SLOTS",
                    "MU_CI_WORKER_SLOTS",
                    "ACTION_RUNNER_MAX_SLOTS",
                ],
                &|k| env_map.get(k).cloned()
            ),
            Some(3)
        );

        // sccache env file dual-env precedence
        let dir = tempfile::tempdir().unwrap();
        let legacy_sccache = dir.path().join("legacy-sccache.env");
        let canonical_sccache = dir.path().join("mu-sccache.env");
        std::fs::write(&legacy_sccache, "SCCACHE_BUCKET=legacy-bucket\n").unwrap();
        std::fs::write(&canonical_sccache, "SCCACHE_BUCKET=canonical-bucket\n").unwrap();

        env_map.insert(
            "ACTION_RUNNER_SCCACHE_ENV_FILE",
            legacy_sccache.to_str().unwrap().to_string(),
        );
        let envs_legacy = WorkerConfig::load_sccache_env_with_env(|k| env_map.get(k).cloned());
        assert!(envs_legacy
            .iter()
            .any(|(k, v)| k == "SCCACHE_BUCKET" && v == "legacy-bucket"));

        env_map.insert(
            "MU_CI_WORKER_SCCACHE_ENV_FILE",
            canonical_sccache.to_str().unwrap().to_string(),
        );
        let envs_canonical = WorkerConfig::load_sccache_env_with_env(|k| env_map.get(k).cloned());
        assert!(envs_canonical
            .iter()
            .any(|(k, v)| k == "SCCACHE_BUCKET" && v == "canonical-bucket"));

        // Verify process env helpers with unique test-only keys
        let _lock = WORKER_ENV_TEST_LOCK.lock().unwrap();
        let test_keys = [
            "TEST_MU_CI_WORKER_ENV_PRIMARY",
            "TEST_ACTION_RUNNER_ENV_FALLBACK",
        ];
        let _guard = EnvGuard::capture(&test_keys);
        std::env::remove_var(test_keys[0]);
        std::env::set_var(test_keys[1], "42");
        assert_eq!(first_non_empty_env(&test_keys).as_deref(), Some("42"));
        assert_eq!(first_parsed_env::<u32>(&test_keys), Some(42));
        std::env::set_var(test_keys[0], "99");
        assert_eq!(first_non_empty_env(&test_keys).as_deref(), Some("99"));
        assert_eq!(first_parsed_env::<u32>(&test_keys), Some(99));
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn test_worker_config_load_dual_env_precedence_and_fallback() {
        let mut env_map: std::collections::HashMap<&str, String> = Default::default();
        // Prevent probing host rustc on PATH in this isolated lookup test
        env_map.insert(
            "ACTION_RUNNER_RUSTC_BIN",
            "/nonexistent/rustc-test".to_string(),
        );
        env_map.insert("ACTION_RUNNER_SCCACHE_BIN", "off".to_string());

        // 1. Legacy ACTION_RUNNER_* values are honored when MU_CI_WORKER_* are unset.
        env_map.insert("ACTION_RUNNER_NODE_ID", "legacy-node".to_string());
        env_map.insert(
            "ACTION_RUNNER_CONTROLLER_URL",
            "ws://192.0.2.1:8000/tunnel".to_string(),
        );
        env_map.insert("ACTION_RUNNER_CLUSTER_TOKEN", "legacy-secret".to_string());
        env_map.insert("ACTION_RUNNER_ARCH", "riscv64".to_string());
        env_map.insert("ACTION_RUNNER_SLOTS", "2".to_string());
        env_map.insert("ACTION_RUNNER_TAGS", "legacy,tag".to_string());
        env_map.insert("ACTION_RUNNER_WORKSPACE", "/tmp/legacy-ws".to_string());
        env_map.insert("ACTION_RUNNER_USE_BTRFS", "false".to_string());
        env_map.insert("ACTION_RUNNER_ISOLATION", "systemd-run".to_string());
        env_map.insert("ACTION_RUNNER_THREADS", "3".to_string());
        env_map.insert("ACTION_RUNNER_DISABLED_TOOLS", "cargo-audit".to_string());

        let legacy_cfg = WorkerConfig::load_with_env(|k| env_map.get(k).cloned());
        assert_eq!(legacy_cfg.node_id, "legacy-node");
        assert_eq!(legacy_cfg.controller_url, "ws://192.0.2.1:8000/tunnel");
        assert_eq!(legacy_cfg.cluster_token, "legacy-secret");
        assert_eq!(legacy_cfg.arch, "riscv64");
        assert_eq!(legacy_cfg.max_slots, 2);
        assert_eq!(legacy_cfg.tags, vec!["legacy", "tag"]);
        assert_eq!(legacy_cfg.workspace_dir, PathBuf::from("/tmp/legacy-ws"));
        assert!(!legacy_cfg.use_btrfs);
        assert_eq!(legacy_cfg.isolation_mode, IsolationMode::SystemdRun);
        assert_eq!(
            legacy_cfg.max_threads_with_env(|k| env_map.get(k).cloned()),
            3
        );
        assert!(legacy_cfg.is_tool_disabled_with_env("cargo-audit", |k| env_map.get(k).cloned()));
        assert_eq!(
            WorkerConfig::detect_rustc_bin_with_env(|k| env_map.get(k).cloned()),
            PathBuf::from("/nonexistent/rustc-test")
        );

        // 2. Canonical MU_CI_WORKER_* values take precedence over ACTION_RUNNER_*.
        env_map.insert("MU_CI_WORKER_NODE_ID", "canonical-node".to_string());
        env_map.insert(
            "MU_CI_WORKER_CONTROLLER_URL",
            "ws://192.0.2.2:8000/tunnel".to_string(),
        );
        env_map.insert("MU_CI_WORKER_CLUSTER_TOKEN", "canonical-secret".to_string());
        env_map.insert("MU_CI_WORKER_ARCH", "x86_64".to_string());
        env_map.insert("MU_CI_WORKER_SLOTS", "4".to_string());
        env_map.insert("MU_CI_WORKER_TAGS", "canonical,tag".to_string());
        env_map.insert("MU_CI_WORKER_WORKSPACE", "/tmp/canonical-ws".to_string());
        env_map.insert("MU_CI_WORKER_USE_BTRFS", "true".to_string());
        env_map.insert("MU_CI_WORKER_ISOLATION", "none".to_string());
        env_map.insert("MU_CI_WORKER_THREADS", "5".to_string());
        env_map.insert("MU_CI_WORKER_DISABLED_TOOLS", "cargo-bench".to_string());
        env_map.insert("MU_CI_WORKER_RUSTC_BIN", "/custom/mu/rustc".to_string());

        let mu_cfg = WorkerConfig::load_with_env(|k| env_map.get(k).cloned());
        assert_eq!(mu_cfg.node_id, "canonical-node");
        assert_eq!(mu_cfg.controller_url, "ws://192.0.2.2:8000/tunnel");
        assert_eq!(mu_cfg.cluster_token, "canonical-secret");
        assert_eq!(mu_cfg.arch, "x86_64");
        assert_eq!(mu_cfg.max_slots, 4);
        assert_eq!(mu_cfg.tags, vec!["canonical", "tag"]);
        assert_eq!(mu_cfg.workspace_dir, PathBuf::from("/tmp/canonical-ws"));
        assert!(mu_cfg.use_btrfs);
        assert_eq!(mu_cfg.isolation_mode, IsolationMode::None);
        assert_eq!(mu_cfg.max_threads_with_env(|k| env_map.get(k).cloned()), 5);
        assert!(mu_cfg.is_tool_disabled_with_env("cargo-bench", |k| env_map.get(k).cloned()));
        assert!(!mu_cfg.is_tool_disabled_with_env("cargo-audit", |k| env_map.get(k).cloned()));
        assert_eq!(
            WorkerConfig::detect_rustc_bin_with_env(|k| env_map.get(k).cloned()),
            PathBuf::from("/custom/mu/rustc")
        );
    }
}

#[cfg(test)]
mod toolchain_discovery_tests {
    use super::WorkerConfig;
    use std::collections::HashSet;

    #[test]
    fn test_detect_toolchains_unified_multi_arch() {
        let paths: HashSet<&str> = [
            "/opt/llvm-24/bin/clang",
            "/opt/llvm-24/bin/riscv64-rva23-linux-gnu-clang",
            "/opt/llvm-24/bin/riscv64-spacemit-x100-linux-gnu-clang",
            "/opt/llvm-24/bin/riscv64-spacemit-a100-linux-gnu-clang",
            "/opt/llvm-24/bin/x86_64-linux-gnu-clang",
            "/opt/llvm-24/bin/aarch64-linux-gnu-clang",
            "/opt/gcc-17/bin/gcc",
            "/opt/gcc-17/bin/riscv64-rva23-linux-gnu-gcc",
            "/opt/gcc-17/bin/riscv64-spacemit-x100-linux-gnu-gcc",
            "/opt/gcc-17/bin/riscv64-spacemit-a100-linux-gnu-gcc",
            "/opt/gcc-17/bin/x86_64-linux-gnu-gcc",
            "/opt/gcc-17/bin/aarch64-linux-gnu-gcc",
        ]
        .into_iter()
        .collect();

        let detected = WorkerConfig::detect_toolchains_with_probe("riscv64", |p| paths.contains(p));
        for expected in [
            "system",
            "llvm-24",
            "llvm-24-native",
            "llvm-24-rva23",
            "llvm-24-spacemit-x100",
            "llvm-24-spacemit-a100",
            "llvm-24-x86_64",
            "llvm-24-aarch64",
            "gcc-17",
            "gcc-17-native",
            "gcc-17-rva23",
            "gcc-17-spacemit-x100",
            "gcc-17-spacemit-a100",
            "gcc-17-x86_64",
            "gcc-17-aarch64",
        ] {
            assert!(
                detected.iter().any(|t| t == expected),
                "Missing expected toolchain '{expected}' in {detected:?}"
            );
        }
    }

    #[test]
    fn test_detect_toolchains_legacy_x100_still_advertises_gcc17_and_x100() {
        let paths: HashSet<&str> = [
            "/opt/cross-llvm-24/bin/clang",
            "/opt/cross-gcc-17/bin/riscv64-unknown-linux-gnu-gcc",
        ]
        .into_iter()
        .collect();
        let detected = WorkerConfig::detect_toolchains_with_probe("x86_64", |p| paths.contains(p));
        assert!(detected.contains(&"llvm-24".to_string()));
        assert!(detected.contains(&"gcc-17".to_string()));
        assert!(detected.contains(&"gcc-17-rva23".to_string()));
        assert!(detected.contains(&"gcc-17-spacemit-x100".to_string()));
    }

    #[test]
    fn test_detect_toolchains_empty_and_arch_native() {
        let empty = WorkerConfig::detect_toolchains_with_probe("aarch64", |_| false);
        assert_eq!(empty, vec!["system"]);

        let arm_native: HashSet<&str> = ["/opt/llvm-24/bin/clang", "/opt/gcc-17/bin/gcc"]
            .into_iter()
            .collect();
        let detected =
            WorkerConfig::detect_toolchains_with_probe("aarch64", |p| arm_native.contains(p));
        assert!(detected.contains(&"llvm-24-native".to_string()));
        assert!(detected.contains(&"llvm-24-aarch64".to_string()));
        assert!(!detected.contains(&"llvm-24-x86_64".to_string()));
        assert!(detected.contains(&"gcc-17-native".to_string()));
        assert!(detected.contains(&"gcc-17-aarch64".to_string()));
        assert!(!detected.contains(&"gcc-17-x86_64".to_string()));
    }
}
