#!/usr/bin/env bash
set -euo pipefail

echo "================================================================================"
echo "  Mu-CI Worker Service Account & Security Isolation Provisioning"
echo "================================================================================"

if [ "$(id -u)" -ne 0 ]; then
    echo "ERROR: This script must be run as root (or via sudo)." >&2
    exit 1
fi

# 1. Create mu-ci-worker system user and group if not present
if ! id -u mu-ci-worker >/dev/null 2>&1; then
    echo "Creating system service account: mu-ci-worker"
    useradd -r -m -d /var/lib/mu-ci-worker -s /bin/bash -c "Mu-CI Worker Daemon" mu-ci-worker
else
    echo "System service account mu-ci-worker already exists."
fi

# 2. Prepare workspace and config directories with strict ownership (0750 / 0700)
echo "Setting up /var/lib/mu-ci-worker and /etc/mu-ci-worker"
mkdir -p /var/lib/mu-ci-worker/workspace/repos
mkdir -p /var/lib/mu-ci-worker/workspace/logs
mkdir -p /var/lib/mu-ci-worker/workspace/.cargo
mkdir -p /var/lib/mu-ci-worker/.cache
mkdir -p /etc/mu-ci-worker

# Migrate legacy worker configuration secrets/env if present and target does not exist
for secret_file in cluster_token sccache.env; do
    if [ -f "/etc/action-runner/${secret_file}" ] && [ ! -f "/etc/mu-ci-worker/${secret_file}" ]; then
        echo "Migrating /etc/action-runner/${secret_file} to /etc/mu-ci-worker/${secret_file}"
        cp "/etc/action-runner/${secret_file}" "/etc/mu-ci-worker/${secret_file}"
        chmod 600 "/etc/mu-ci-worker/${secret_file}"
    fi
done

chown -R mu-ci-worker:mu-ci-worker /var/lib/mu-ci-worker
chown -R mu-ci-worker:mu-ci-worker /etc/mu-ci-worker
chmod 750 /var/lib/mu-ci-worker
chmod 700 /etc/mu-ci-worker

# 3. Provision Rust toolchain (.rustup) and Cargo subcommands (cargo-audit, cargo-llvm-cov)
if [ ! -d "/var/lib/mu-ci-worker/.rustup/toolchains" ]; then
    if [ -d "/var/lib/action-runner/.rustup/toolchains" ]; then
        echo "Provisioning Rustup toolchain for mu-ci-worker from /var/lib/action-runner/.rustup"
        mkdir -p /var/lib/mu-ci-worker/.rustup
        cp -a /var/lib/action-runner/.rustup/. /var/lib/mu-ci-worker/.rustup/
    elif [ -d "/home/lma1980/.rustup/toolchains" ]; then
        echo "Provisioning Rustup toolchain for mu-ci-worker from /home/lma1980/.rustup"
        mkdir -p /var/lib/mu-ci-worker/.rustup
        cp -a /home/lma1980/.rustup/. /var/lib/mu-ci-worker/.rustup/
    fi
    if [ -d "/var/lib/mu-ci-worker/.rustup" ]; then
        chown -R mu-ci-worker:mu-ci-worker /var/lib/mu-ci-worker/.rustup
    fi
fi

CARGO_SRC_DIR=""
if [ -d "/var/lib/action-runner/.cargo/bin" ]; then
    CARGO_SRC_DIR="/var/lib/action-runner/.cargo"
elif [ -d "/home/lma1980/.cargo/bin" ]; then
    CARGO_SRC_DIR="/home/lma1980/.cargo"
fi

if [ -n "${CARGO_SRC_DIR}" ]; then
    mkdir -p /var/lib/mu-ci-worker/.cargo/bin /var/lib/mu-ci-worker/workspace/.cargo/bin
    cp -a "${CARGO_SRC_DIR}"/bin/* /var/lib/mu-ci-worker/.cargo/bin/ 2>/dev/null || true
    cp -a "${CARGO_SRC_DIR}"/bin/* /var/lib/mu-ci-worker/workspace/.cargo/bin/ 2>/dev/null || true
    if [ -d "${CARGO_SRC_DIR}/advisory-db" ] && [ ! -d "/var/lib/mu-ci-worker/workspace/.cargo/advisory-db" ]; then
        cp -a "${CARGO_SRC_DIR}/advisory-db" /var/lib/mu-ci-worker/workspace/.cargo/advisory-db
    fi
    if [ -d "${CARGO_SRC_DIR}/advisory-db" ] && [ ! -d "/var/lib/mu-ci-worker/.cargo/advisory-db" ]; then
        cp -a "${CARGO_SRC_DIR}/advisory-db" /var/lib/mu-ci-worker/.cargo/advisory-db
    fi
    chown -R mu-ci-worker:mu-ci-worker /var/lib/mu-ci-worker/.cargo /var/lib/mu-ci-worker/workspace/.cargo
fi

# 3b. Provision Python user site-packages (.local) such as mdformat-gfm
if [ ! -d "/var/lib/mu-ci-worker/.local/lib" ]; then
    if [ -d "/var/lib/action-runner/.local/lib" ]; then
        echo "Provisioning Python .local packages (e.g., mdformat-gfm) for mu-ci-worker from /var/lib/action-runner/.local"
        mkdir -p /var/lib/mu-ci-worker/.local
        cp -a /var/lib/action-runner/.local/. /var/lib/mu-ci-worker/.local/
    elif [ -d "/home/lma1980/.local" ]; then
        echo "Provisioning Python .local packages (e.g., mdformat-gfm) for mu-ci-worker from /home/lma1980/.local"
        mkdir -p /var/lib/mu-ci-worker/.local
        cp -a /home/lma1980/.local/. /var/lib/mu-ci-worker/.local/
    fi
    if [ -d "/var/lib/mu-ci-worker/.local" ]; then
        chown -R mu-ci-worker:mu-ci-worker /var/lib/mu-ci-worker/.local
    fi
fi

# 4. Provision CI SSH Deploy Key if present
SSH_KEY_SRC=""
if [ -f "/var/lib/action-runner/.ssh/id_k3_ci" ]; then
    SSH_KEY_SRC="/var/lib/action-runner/.ssh/id_k3_ci"
elif [ -f "/home/lma1980/.ssh/id_k3_ci" ]; then
    SSH_KEY_SRC="/home/lma1980/.ssh/id_k3_ci"
fi

if [ -n "${SSH_KEY_SRC}" ] && [ ! -f "/var/lib/mu-ci-worker/.ssh/id_k3_ci" ]; then
    echo "Provisioning CI deploy key for mu-ci-worker"
    mkdir -p /var/lib/mu-ci-worker/.ssh
    cp "${SSH_KEY_SRC}" /var/lib/mu-ci-worker/.ssh/id_k3_ci
    if [ -f "${SSH_KEY_SRC}.pub" ]; then
        cp "${SSH_KEY_SRC}.pub" /var/lib/mu-ci-worker/.ssh/id_k3_ci.pub
    fi
    cat << 'EOF' > /var/lib/mu-ci-worker/.ssh/config
Host github.com
    HostName github.com
    User git
    IdentityFile /var/lib/mu-ci-worker/.ssh/id_k3_ci
    IdentitiesOnly yes
    StrictHostKeyChecking accept-new
EOF
    chown -R mu-ci-worker:mu-ci-worker /var/lib/mu-ci-worker/.ssh
    chmod 700 /var/lib/mu-ci-worker/.ssh
    chmod 600 /var/lib/mu-ci-worker/.ssh/id_k3_ci
    chmod 600 /var/lib/mu-ci-worker/.ssh/config
fi

# 5. Install Sudoers configuration for user lma1980 diagnostics
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
echo "Installing sudoers diagnostics policy in /etc/sudoers.d/mu-ci-worker-diag"
cp "${SCRIPT_DIR}/sudoers-mu-ci-worker" /etc/sudoers.d/mu-ci-worker-diag
chmod 440 /etc/sudoers.d/mu-ci-worker-diag
visudo -c -f /etc/sudoers.d/mu-ci-worker-diag

# 6. Install Systemd Service Unit
echo "Installing /etc/systemd/system/mu-ci-worker.service"
cp "${SCRIPT_DIR}/mu-ci-worker.service" /etc/systemd/system/mu-ci-worker.service
chmod 644 /etc/systemd/system/mu-ci-worker.service

if [ ! -f "/etc/mu-ci-worker/worker.env" ]; then
    echo "Installing template /etc/mu-ci-worker/worker.env (customize before starting)"
    cp "${SCRIPT_DIR}/mu-ci-worker.env.example" /etc/mu-ci-worker/worker.env
    chown mu-ci-worker:mu-ci-worker /etc/mu-ci-worker/worker.env
    chmod 600 /etc/mu-ci-worker/worker.env
fi

systemctl daemon-reload

echo "================================================================================"
echo "  Provisioning complete!"
echo "  To test sudo diagnostics as lma1980:"
echo "    sudo -u mu-ci-worker -i"
echo "  To enable and start the service:"
echo "    sudo systemctl enable --now mu-ci-worker"
echo "================================================================================"
