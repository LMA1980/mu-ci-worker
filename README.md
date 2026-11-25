# mu-ci-worker

<p align="center">
  <a href="https://hook.lma1980.duckdns.org/cards/LMA1980/mu-ci-worker/card.svg">
    <img src="https://hook.lma1980.duckdns.org/cards/LMA1980/mu-ci-worker/card.svg" alt="mu-ci-worker CI Factory Scorecard" width="520"/>
  </a>
</p>

<p align="center">
  <a href="https://hook.lma1980.duckdns.org/jobs"><img src="https://hook.lma1980.duckdns.org/badges/LMA1980/mu-ci-worker/status.svg" alt="CI Status"/></a>
  <a href="https://hook.lma1980.duckdns.org/badges/LMA1980/mu-ci-worker/score.svg"><img src="https://hook.lma1980.duckdns.org/badges/LMA1980/mu-ci-worker/score.svg" alt="Quality Score"/></a>
  <a href="https://hook.lma1980.duckdns.org/badges/LMA1980/mu-ci-worker/coverage.svg"><img src="https://hook.lma1980.duckdns.org/badges/LMA1980/mu-ci-worker/coverage.svg" alt="Coverage"/></a>
  <a href="https://hook.lma1980.duckdns.org/badges/LMA1980/mu-ci-worker/benchmark.svg"><img src="https://hook.lma1980.duckdns.org/badges/LMA1980/mu-ci-worker/benchmark.svg" alt="Benchmark"/></a>
</p>

______________________________________________________________________

Stateless distributed CI stage execution agent in the [`mu-ci`](https://github.com/LMA1980/mu-ci) platform, running natively on `x86_64` hosts and **SpacemiT Key Stone K3** (`riscv64gc`: 8× X100 @ `2.15 GHz` + 8× A100 @ `1.80 GHz`) hardware.

______________________________________________________________________

## Architecture & Capabilities

`mu-ci-worker` is a pure cluster stage runner without an HTTP ingress server; webhook ingestion, queue management, DAG scheduling, and SVG badge/card rendering live in [`mu-ci-controller`](https://github.com/LMA1980/mu-ci-controller).

- **WebSocket Cluster Tunnel (`mu-ci-ipc`)**: Connects outbound to [`mu-ci-controller`](https://github.com/LMA1980/mu-ci-controller) (`/api/cluster/tunnel`), registering node metadata (`WorkerRegistration` including `rustc --version` semver probed into `rust_semver`), capability `tags`, `toolchains`, and bounded `max_slots = 1` over [`mu-ci-ipc`](https://github.com/LMA1980/mu-ci-ipc) 24-byte `IpcHeader` frames (`b"K3CI"` magic) with Apache Fory control envelopes and Apache Arrow columnar payloads.
- **Typed Tool Dispatch (`mu-ci-tools`)**: Executes the 12 compile-time registered `ToolAdapter` implementations from [`mu-ci-tools`](https://github.com/LMA1980/mu-ci-tools) via direct `execve`-style argument vectors—never invoking arbitrary shell scripts.
- **3-Layer Monotonic Sandboxing (`mu-ci-isolation`)**: Resolves Layer 1 host ceilings (`MU_ISOLATION_CEILING_PATH`, defaulting to `/etc/mu-isolation/ceiling.yml`), Layer 2 repository profiles (`.muci/security.yml` with `.actionrunner/security.yml` fallback), and Layer 3 dispatched stage/task constraints into `KernelSandbox` (Landlock LSM, Linux namespaces, seccomp-BPF, bounded `RLIMIT_MEMLOCK`, conditional `/proc/set_ai_thread`) or `HypervisorSandbox` (KVM), failing closed with `SecurityGrantDenied` on privilege escalation attempts.
- **Single-Pass Line Streaming & OTEL Telemetry (`mu-observable-ipc`)**: Streams stdout/stderr lines once in real-time `MSG_TYPE_ARROW_LOG` batches over the controller tunnel (see [`specs/single-pass-line-streaming.md`](specs/single-pass-line-streaming.md)) and emits `MUOB`-framed OTEL spans, logs, test case runs, and benchmark runs to [`mu-observable-collector`](https://github.com/LMA1980/mu-observable-collector) (`MUOBSERVE_COLLECTOR_ADDR`, default `127.0.0.1:4319`) via [`mu-observable-ipc`](https://github.com/LMA1980/mu-observable-ipc).
- **Continuous Delivery (`mu-cd`)**: Managed as `mu-ci-worker.service` (`/usr/local/bin/mu-ci-worker`) via centralized [`mu-cd`](https://github.com/LMA1980/mu-cd) deployment policy.

______________________________________________________________________

## Configuration

Environment variables configured in `/etc/action-runner/worker.env` (loaded by `mu-ci-worker.service`):

| Variable                                                              | Default                                  | Purpose                                                                                                                                          |
| --------------------------------------------------------------------- | ---------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------ |
| `ACTION_RUNNER_CONTROLLER_URL`                                        | `ws://127.0.0.1:8000/api/cluster/tunnel` | WebSocket tunnel endpoint on `mu-ci-controller`                                                                                                  |
| `ACTION_RUNNER_CLUSTER_TOKEN`                                         | Built-in dev fallback                    | Shared cluster authentication token (file path or literal; fails closed if path unreadable)                                                      |
| `ACTION_RUNNER_NODE_ID`                                               | Hostname                                 | Unique worker node identifier (e.g. `node-a`, `node-b`, `worker-riscv64`)                                                                        |
| `ACTION_RUNNER_ARCH`                                                  | Host `std::env::consts::ARCH`            | Architecture reported in `WorkerRegistration`                                                                                                    |
| `ACTION_RUNNER_TAGS`                                                  | Arch-dependent defaults                  | Comma-separated worker capability tags (`sccache` appended automatically when `sccache` binary is detected)                                      |
| `ACTION_RUNNER_MAX_SLOTS`                                             | `1`                                      | Maximum concurrent stage slots per worker node (`1` recommended; scale intra-stage parallelism via `ACTION_RUNNER_THREADS` / `CARGO_BUILD_JOBS`) |
| `ACTION_RUNNER_THREADS`                                               | Arch-dependent                           | Intra-stage build/test thread budget (`CARGO_BUILD_JOBS`, `RAYON_NUM_THREADS`, `RUST_TEST_THREADS`)                                              |
| `ACTION_RUNNER_WORKSPACE`                                             | `/var/lib/action-runner/workspace`       | Base directory for stage workspaces and git mirrors                                                                                              |
| `ACTION_RUNNER_USE_BTRFS`                                             | Auto-detected                            | Enable Btrfs snapshot workspace optimizations (`true`/`1`)                                                                                       |
| `ACTION_RUNNER_ISOLATION`                                             | `none`                                   | Optional `systemd-run` wrapper mode (`none` \| `systemd-run`)                                                                                    |
| `ACTION_RUNNER_RUSTC_BIN`                                             | `rustc` on `PATH`                        | Optional explicit `rustc` binary path probed for `rust_semver`                                                                                   |
| `ACTION_RUNNER_SCCACHE_BIN`                                           | Auto-detected                            | Optional explicit path to `sccache` binary, or `off`/`false`/`0`/`none` to disable `sccache` on the worker                                       |
| `ACTION_RUNNER_SCCACHE_ENV_FILE`                                      | `/etc/action-runner/sccache.env`         | Path to optional S3/storage environment file injected into `sccache`                                                                             |
| `ACTION_RUNNER_RAM_TOTAL_MB`                                          | `/proc/meminfo`                          | Optional RAM total override in MiB                                                                                                               |
| `MU_CI_WRITE_TIMEOUT_SECS` / `ACTION_RUNNER_WRITE_TIMEOUT_SECS`       | Default tunnel timeout                   | WebSocket frame write timeout override                                                                                                           |
| `MU_CI_PING_INTERVAL_SECS` / `ACTION_RUNNER_PING_INTERVAL_SECS`       | Default heartbeat interval               | WebSocket heartbeat/ping interval override                                                                                                       |
| `MU_CI_LIVENESS_TIMEOUT_SECS` / `ACTION_RUNNER_LIVENESS_TIMEOUT_SECS` | Default liveness timeout                 | Controller tunnel liveness timeout override                                                                                                      |
| `MU_ISOLATION_CEILING_PATH`                                           | `/etc/mu-isolation/ceiling.yml`          | Layer 1 operator host privilege ceiling for `mu-ci-isolation`                                                                                    |
| `MUOBSERVE_COLLECTOR_ADDR`                                            | `127.0.0.1:4319`                         | `mu-observable-collector` `MUOB` TCP endpoint for OTEL spans/logs/test/bench batches                                                             |

______________________________________________________________________

## Operator Guide: Shared `sccache` Compiler Cache Integration

`mu-ci-worker` natively integrates with [`sccache`](https://github.com/mozilla/sccache) (local disk or S3-compatible object storage such as [Garage](https://garagehq.deuxfleurs.fr/), MinIO, or AWS S3) so distributed workers share compiled Rust and WebAssembly artifacts across CI runs.

> **Complete Operator & Workflow Portal Guide**: See [**Operator & Workflow Guide: Distributed `sccache` Compilation Caching**](https://github.com/LMA1980/lma1980.github.io/blob/main/content/docs/sccache-guide.md) for the full architectural and operator walkthrough.

### 0. Default `try` Mode & Cache Semantics

1. **Default-On `try` Mode (`SccacheMode::Try`)**:
   - In `mu-ci`, `sccache` defaults to **`try`** across all workflows, stages, and tasks (`tasks.<id>.sccache -> stages[i].sccache -> workflow.sccache -> try`).
   - **Without `sccache` installed**: `try` mode sees no `sccache` binary and transparently runs standard uncached `cargo` builds.
   - **With `/usr/local/bin/sccache` installed**: Upon restarting `mu-ci-worker.service`, the worker advertises `"sccache"` in its capability `tags` and automatically caches eligible compilation tasks across repositories without modifying `.muci/mu-ci.yml`. (To opt out on a worker, set `ACTION_RUNNER_SCCACHE_BIN=off` in `/etc/action-runner/worker.env`; to opt out in a workflow/stage/task, set `sccache: false`.)
1. **Cold Cache (Build #1) vs. Warm Cache (Build #2+)**:
   - **Build #1 (Cold Cache)**: Compiles every crate from scratch, hashes inputs, and uploads `.rlib`/`.rmeta` artifacts to the cache backend, taking roughly 5%–10% longer than an uncached build.
   - **Build #2+ (Warm Cache)**: Subsequent runs fetch unchanged crates directly from `sccache`, reducing `cargo-check`, `cargo-clippy`, `cargo-test`, and `cargo-build` times by ~60%–85%.
1. **Automatic `CARGO_INCREMENTAL=0`**:
   - `sccache` cannot cache Rust's local incremental compilation state (`-C incremental=...`). Whenever `StageRunner` activates `sccache` for a task, it sets `CARGO_INCREMENTAL=0` so each crate is cached once as a hermetic unit and reused across branches and worker nodes without `target/*/incremental/` disk bloat.
1. **Cross-Compilation & Cache Key Alignment (`x86_64` ↔ `riscv64` / `aarch64`)**:
   - **`x86_64` Cross-Compilers vs. Native `riscv64` / `aarch64` Workers**: `sccache` hashes the **SHA-256 digest of the host `rustc` binary** along with `--target` and flags. Multiple `x86_64` workers cross-compiling for `riscv64gc-unknown-linux-gnu`, `riscv64a23-unknown-linux-gnu`, or `aarch64-unknown-linux-gnu` share cache hits in S3, while native `riscv64` and `aarch64` workers maintain separate host hash spaces (all architectures safely share the same `SCCACHE_BUCKET` without collisions).
   - **Target-Scoped `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` vs. Global `RUSTFLAGS`**: Setting global `RUSTFLAGS="-C target-cpu=spacemit-x100"` during cross-builds breaks host `build.rs` scripts and proc-macros. `StageRunner::compute_toolchain_env` scopes flags to `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` (`riscv64gc-unknown-linux-gnu`, `riscv64a23-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-gnu`) and passes portable `--sysroot=/opt/sysroots/<triple>` when present (falling back to `/`), supporting `llvm-24*` and `gcc-17*` (`native`, `rva23`, `spacemit-x100`, `spacemit-a100`, `x86_64`, `aarch64`) while leaving host `RUSTFLAGS` clean so `sccache` caches both host proc-macros and target crates.
   - **Toolchain & Path Alignment**: Workers sharing cross-compile or native caches align `rustc` versions (`/var/lib/action-runner/.rustup`), unified toolchain roots (`/opt/gcc-17`, `/opt/llvm-24`, `/opt/sysroots`, `/opt/toolchain-env`, plus compatibility symlinks `/opt/cross-llvm-24`, `/opt/cross-gcc-17`, `/opt/llvm-24-spacemit-{x100,a100}`, `/opt/gcc-17-spacemit-{x100,a100}`), RVA23 / SpacemiT X100 / SpacemiT A100 driver wrappers, and `ACTION_RUNNER_WORKSPACE` paths (`/var/lib/action-runner/workspace`).
   - **Low-RAM (`4 GiB`) Edge Workers**: Cross-compile `sccache` on an `x86_64` host and install the stripped binary to `/usr/local/bin/sccache` instead of running `cargo install sccache` on constrained SBCs.

### 1. Install the `sccache` Binary on Each Worker Node

> **Service Account Rule**: Never install `sccache` only inside `/root/.cargo/bin` or a personal user directory (`ProtectHome=read-only` and `mu-ci-isolation` Landlock rules block access). Install to **`/usr/local/bin/sccache`** (`0755 root:root`).

`WorkerConfig::detect_sccache_bin()` probes:

1. `ACTION_RUNNER_SCCACHE_BIN` (explicit path, or `off`/`false`/`0`/`none` to disable `sccache` on that worker)
1. `/usr/local/bin/sccache` *(recommended system-wide location)*
1. `/usr/bin/sccache`
1. `$CARGO_HOME/bin/sccache` or `$HOME/.cargo/bin/sccache`

#### Option A: `x86_64` or `aarch64` Linux Worker (Official Static Binary)

```bash
SCCACHE_VERSION="v0.8.2"
ARCH="$(uname -m)" # x86_64 or aarch64
curl -fL "https://github.com/mozilla/sccache/releases/download/${SCCACHE_VERSION}/sccache-${SCCACHE_VERSION}-${ARCH}-unknown-linux-musl.tar.gz" -o /tmp/sccache.tar.gz
tar -xzf /tmp/sccache.tar.gz -C /tmp
sudo install -m 755 "/tmp/sccache-${SCCACHE_VERSION}-${ARCH}-unknown-linux-musl/sccache" /usr/local/bin/sccache
rm -rf /tmp/sccache.tar.gz "/tmp/sccache-${SCCACHE_VERSION}-${ARCH}-unknown-linux-musl"
sudo -u action-runner /usr/local/bin/sccache --version
```

#### Option B: `riscv64` (SpacemiT K3) or Custom Linux Worker (Build from Source)

```bash
sudo apt-get update && sudo apt-get install -y pkg-config libssl-dev build-essential
sudo -u action-runner env \
  RUSTUP_HOME=/var/lib/action-runner/.rustup \
  CARGO_HOME=/var/lib/action-runner/.cargo \
  PATH="/var/lib/action-runner/.cargo/bin:/usr/local/bin:/usr/bin:/bin" \
  cargo install sccache --locked
sudo install -m 755 /var/lib/action-runner/.cargo/bin/sccache /usr/local/bin/sccache
sudo -u action-runner /usr/local/bin/sccache --version
```

When `detect_sccache_bin()` locates a valid binary at startup, the worker automatically appends `"sccache"` to its advertised `tags` during `WorkerRegistration`, making the node eligible for stages configured with `sccache: mandatory`.

### 2. Configure the Cache Backend (`/etc/action-runner/sccache.env`)

Create `/etc/action-runner/sccache.env` owned by the `action-runner` service account (`0700` on `/etc/action-runner` owned by `action-runner:action-runner`, **not** `root:root`):

```bash
sudo install -d -m 700 -o action-runner -g action-runner /etc/action-runner
sudo tee /etc/action-runner/sccache.env >/dev/null <<'EOF'
# Backend Option A: Shared S3 / Garage / MinIO Cache (multi-node cluster)
SCCACHE_BUCKET=sccache
SCCACHE_REGION=us-east-1
SCCACHE_ENDPOINT=http://192.0.2.10:3900
SCCACHE_S3_USE_SSL=false
SCCACHE_S3_KEY_PREFIX=v1/
SCCACHE_SERVER_PORT=4226
SCCACHE_IDLE_TIMEOUT=0
AWS_ACCESS_KEY_ID=GK0123456789abcdef01234567
AWS_SECRET_ACCESS_KEY=replace-with-your-s3-secret-key

# Backend Option B: Per-Node Local Disk Cache (if not using S3, comment out Option A and uncomment below)
# SCCACHE_DIR=/var/lib/action-runner/.cache/sccache
# SCCACHE_CACHE_SIZE=20G
# SCCACHE_SERVER_PORT=4226
# SCCACHE_IDLE_TIMEOUT=0
EOF
sudo chown action-runner:action-runner /etc/action-runner/sccache.env
sudo chmod 600 /etc/action-runner/sccache.env
```

`WorkerConfig::load_sccache_env()` reads `/etc/action-runner/sccache.env` (or `ACTION_RUNNER_SCCACHE_ENV_FILE`) and merges overrides for:

- `SCCACHE_BUCKET`, `SCCACHE_REGION`, `SCCACHE_ENDPOINT`, `SCCACHE_S3_USE_SSL`, `SCCACHE_S3_KEY_PREFIX`
- `SCCACHE_DIR`, `SCCACHE_CACHE_SIZE`, `SCCACHE_SERVER_PORT`, `SCCACHE_SERVER_UDS`, `SCCACHE_IDLE_TIMEOUT`
- `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`

> **Note**: Do **not** hardcode `RUSTC_WRAPPER` inside `/etc/action-runner/sccache.env` or `/etc/action-runner/worker.env`; `StageRunner` automatically filters out `RUSTC_WRAPPER` from `sccache.env` and injects it selectively per task so incompatible tools (`cargo-miri`, `cargo-llvm-cov`) remain unaffected.

### 3. Ensure `mu-ci-isolation` (`security.yml`) Permits `sccache` Execution

When repositories run under [`mu-ci-isolation`](https://github.com/LMA1980/mu-ci-isolation) (`backend: Kernel` with Landlock LSM), ensure the directory containing `sccache` (`/usr/local/bin` or `/usr/bin`) is included in `exec_paths` and `/usr` in `read_paths` within `.muci/security.yml` (or legacy `.actionrunner/security.yml`):

```yaml
default_profile: "default_ci"
profiles:
  default_ci:
    backend: Kernel
    read_paths: ["/usr", "/bin", "/lib", "/etc/ssl", "/var/lib/action-runner"]
    write_paths: ["/var/lib/action-runner", "/tmp"]
    exec_paths:
      - /usr/bin
      - /usr/local/bin
      - /bin
      - /var/lib/action-runner/.cargo/bin
      - /var/lib/action-runner/.rustup
      - /var/lib/action-runner/.local/bin
```

### 4. Runtime Behavior, Tool Filtering & Self-Healing

For each dispatched task in a stage, `StageRunner` resolves the effective `SccacheMode` (`Off`, `Try` [default], or `Mandatory`) and enforces:

- **Tool Eligibility Filtering**:
  - **Eligible compiler tools** (`cargo-build`, `cargo-check`, `cargo-test`, `cargo-clippy`, `cargo-bench`, `trunk`) receive `RUSTC_WRAPPER=<sccache_bin>`, `CARGO_INCREMENTAL=0`, `SCCACHE_IDLE_TIMEOUT=0`, and all S3/cache variables from `sccache.env`.
  - **Non-compiler & incompatible tools** (`cargo-fmt`, `cargo-audit`, `mdformat`, `cargo-llvm-cov`, `cargo-miri`) automatically have `RUSTC_WRAPPER` and `CARGO_BUILD_RUSTC_WRAPPER` removed from their process environment.
- **Pre-Flight Health Check & Local-Disk Hijack Recovery (`ensure_sccache_server_healthy`)**:
  - Before spawning an eligible compiler tool, the worker runs `sccache --show-stats` under a bounded **5-second timeout** (auto-starting the local `sccache` daemon with `sccache.env` credentials if not yet running).
  - If `SCCACHE_BUCKET` is configured but the local `sccache` daemon reports `Cache location: Local disk`, `StageRunner` stops the stale daemon (`sccache --stop-server`) and restarts it with the S3 environment.
  - If the daemon is transiently wedged, `StageRunner` executes a single `--stop-server` + `--show-stats` recovery cycle.
- **Failure Policy (`Try` vs. `Mandatory`)**:
  - **`SccacheMode::Try` (default)**: If `sccache` is missing or fails the health check, the worker logs a warning, strips `RUSTC_WRAPPER`, and falls back to uncached `rustc` compilation.
  - **`SccacheMode::Mandatory`**: If `sccache` is missing or fails the health check after retry, the stage fails closed immediately (`exit_code = 126`).

### 5. Verifying Worker `sccache` Status

Run under the `action-runner` service account to verify S3 connectivity and cache hit rates:

```bash
sudo -u action-runner bash -c 'set -a && source /etc/action-runner/sccache.env && set +a && /usr/local/bin/sccache --show-stats'
```

______________________________________________________________________

## Service Account Provisioning

Automated provisioning script in [`deploy/setup-service-account.sh`](deploy/setup-service-account.sh):

```bash
sudo ./deploy/setup-service-account.sh
```

Service account shell inspection:

```bash
sudo -u action-runner -i
```

______________________________________________________________________

## License

Licensed under the GNU Affero General Public License v3.0 (AGPL-3.0-only). See [LICENSE](LICENSE) for details.
