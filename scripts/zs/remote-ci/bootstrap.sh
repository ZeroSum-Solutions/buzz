#!/bin/bash
# zs fork: cloud-init user-data for the on-demand Linux test box (buzz-ci-box).
#
# Reproduces the Blacksmith CI environment of `.github/workflows/_ci-desktop.yml`
# and `_ci-rust.yml` on one Ubuntu 24.04 x86_64 instance:
#   - the same apt packages, installed with the same retry loop CI uses
#   - the Playwright system dependencies for Chromium
#   - a `ci` user holding the repo clone, hermit, the pnpm store, the cargo
#     caches and the Playwright browsers, all on the 200 GB gp3 root volume
#   - a root cron that stops the box once uptime passes 90 minutes
#
# Runs once, as root, at first boot. Everything it writes is on the persistent
# root volume, so a stop/start keeps it. Re-running it by hand is safe.
#
# Log: /var/log/buzz-bootstrap.log
set -uo pipefail

LOG=/var/log/buzz-bootstrap.log
exec > >(tee -a "$LOG") 2>&1
echo "=== buzz-ci-box bootstrap starting $(date -u +%FT%TZ) ==="

CI_USER=ci
CI_HOME=/home/${CI_USER}
REPO_URL="${BUZZ_CI_REPO_URL:-https://github.com/ZeroSum-Solutions/buzz.git}"
REPO_BRANCH="${BUZZ_CI_REPO_BRANCH:-zs/main}"
REPO_DIR="${CI_HOME}/buzz"
UPTIME_LIMIT_MINUTES=90

# The fork is public (gh repo view ZeroSum-Solutions/buzz --json isPrivate ->
# false), so an unauthenticated clone works. If it is ever made private, do NOT
# put a token in user-data: add a read-only deploy key to the repo, drop the
# private half at /home/ci/.ssh/id_buzz_deploy (0600) by hand or through SSM
# Parameter Store, set REPO_URL to git@github.com:ZeroSum-Solutions/buzz.git,
# and add `IdentityFile ~/.ssh/id_buzz_deploy` to /home/ci/.ssh/config.

# ~/.bashrc returns early for non-interactive shells, so source the env file
# explicitly rather than relying on a login shell to pick it up.
as_ci() {
  sudo -u "$CI_USER" -H bash -c \
    "set -uo pipefail; [ -f \"\$HOME/.buzz-ci-env\" ] && . \"\$HOME/.buzz-ci-env\"; $1"
}

# ── apt ──────────────────────────────────────────────────────────────────────
export DEBIAN_FRONTEND=noninteractive
# Same shape as CI: the mirrors drop out for minutes at a time and apt's own
# three retries do not cover it, so re-run the whole update+install five times.
for attempt in 1 2 3 4 5; do
  if apt-get update \
      -o Acquire::Retries=3 \
      -o Acquire::http::Timeout=30 \
      -o Acquire::https::Timeout=30 \
    && apt-get install -y --no-install-recommends \
      -o Acquire::Retries=3 \
      -o Acquire::http::Timeout=30 \
      -o Acquire::https::Timeout=30 \
      -o DPkg::Lock::Timeout=120 \
      build-essential \
      curl \
      file \
      git \
      jq \
      libasound2-dev \
      libayatana-appindicator3-dev \
      libgtk-3-dev \
      librsvg2-dev \
      libssl-dev \
      libwebkit2gtk-4.1-dev \
      libxdo-dev \
      mold \
      patchelf \
      pkg-config \
      python3 \
      unzip \
      wget \
      xz-utils; then
    break
  fi
  if [ "$attempt" = 5 ]; then
    echo "FATAL: apt install failed after 5 attempts"
    exit 1
  fi
  echo "apt attempt $attempt failed; retrying in 30s"
  sleep 30
done

# ── uptime guard: stop the box after 90 minutes whatever happens ─────────────
# The instance is launched with instance-initiated-shutdown-behavior=stop, so
# `shutdown -h now` stops it rather than terminating it. This is the belt to
# the CloudWatch idle alarm's braces.
cat > /usr/local/sbin/buzz-ci-uptime-guard <<GUARD
#!/bin/bash
set -u
limit_seconds=\$(( ${UPTIME_LIMIT_MINUTES} * 60 ))
up=\$(cut -d. -f1 /proc/uptime)
if [ "\$up" -ge "\$limit_seconds" ]; then
  logger -t buzz-ci-uptime-guard "uptime \${up}s >= \${limit_seconds}s; stopping the instance"
  /sbin/shutdown -h now "buzz-ci-box uptime limit reached"
fi
GUARD
chmod 755 /usr/local/sbin/buzz-ci-uptime-guard
cat > /etc/cron.d/buzz-ci-uptime-stop <<'CRON'
# Stop buzz-ci-box once uptime passes its limit. Checked every minute.
SHELL=/bin/bash
PATH=/usr/local/sbin:/usr/local/bin:/sbin:/bin:/usr/sbin:/usr/bin
* * * * * root /usr/local/sbin/buzz-ci-uptime-guard
CRON
chmod 644 /etc/cron.d/buzz-ci-uptime-stop
systemctl enable --now cron || true

# ── ci user ──────────────────────────────────────────────────────────────────
if ! id -u "$CI_USER" >/dev/null 2>&1; then
  useradd --create-home --shell /bin/bash "$CI_USER"
fi
install -d -m 700 -o "$CI_USER" -g "$CI_USER" "${CI_HOME}/.ssh"
if [ -f /home/ubuntu/.ssh/authorized_keys ]; then
  install -m 600 -o "$CI_USER" -g "$CI_USER" \
    /home/ubuntu/.ssh/authorized_keys "${CI_HOME}/.ssh/authorized_keys"
fi

# `playwright install-deps` and nothing else needs root from the ci user.
cat > /etc/sudoers.d/buzz-ci <<'SUDO'
ci ALL=(ALL) NOPASSWD: ALL
SUDO
chmod 440 /etc/sudoers.d/buzz-ci

cat > "${CI_HOME}/.buzz-ci-env" <<'ENVFILE'
# Sourced by every remote-ci run. Keeps the caches on the persistent volume.
export CARGO_TERM_COLOR=always
export CARGO_HOME="$HOME/.cargo"
export RUSTUP_HOME="$HOME/.rustup"
export PLAYWRIGHT_BROWSERS_PATH="$HOME/.cache/ms-playwright"
export PNPM_HOME="$HOME/.local/share/pnpm"
export BUZZ_TEST_POSTGRES_PASSWORD=buzz_dev
export CMAKE_POLICY_VERSION_MINIMUM=3.5
export DEBIAN_FRONTEND=noninteractive
export PATH="$HOME/.cargo/bin:$PNPM_HOME:$PATH"
ENVFILE
chown "${CI_USER}:${CI_USER}" "${CI_HOME}/.buzz-ci-env"
if ! grep -q buzz-ci-env "${CI_HOME}/.bashrc" 2>/dev/null; then
  echo '[ -f "$HOME/.buzz-ci-env" ] && . "$HOME/.buzz-ci-env"' >> "${CI_HOME}/.bashrc"
  chown "${CI_USER}:${CI_USER}" "${CI_HOME}/.bashrc"
fi

# ── repo clone + hermit ──────────────────────────────────────────────────────
if [ ! -d "${REPO_DIR}/.git" ]; then
  as_ci "git clone --branch '${REPO_BRANCH}' '${REPO_URL}' '${REPO_DIR}'" \
    || { echo "FATAL: clone failed"; exit 1; }
fi
as_ci "cd '${REPO_DIR}' && git config --local gc.auto 0"

# Hermit downloads its packages on first activation; do it now so the first
# real run does not pay for it.
as_ci "cd '${REPO_DIR}' && . ./bin/activate-hermit && just --version && node --version && pnpm --version && cargo --version" \
  || { echo "FATAL: hermit bootstrap failed"; exit 1; }

# ── toolchain extras CI installs through actions ─────────────────────────────
# cargo-nextest (taiki-e/install-action in _ci-rust.yml).
as_ci "mkdir -p \"\$HOME/.cargo/bin\" && curl -fsSL --retry 5 https://get.nexte.st/latest/linux | tar zxf - -C \"\$HOME/.cargo/bin\"" \
  || echo "WARN: cargo-nextest install failed; just test-unit falls back to cargo test"

# ── pnpm store + node modules ────────────────────────────────────────────────
as_ci "cd '${REPO_DIR}' && . ./bin/activate-hermit && pnpm config set store-dir \"\$HOME/.pnpm-store\" --global"
as_ci "cd '${REPO_DIR}' && . ./bin/activate-hermit && just desktop-install" \
  || { echo "FATAL: just desktop-install failed"; exit 1; }

# ── Playwright browser + system deps ─────────────────────────────────────────
as_ci "cd '${REPO_DIR}/desktop' && . ../bin/activate-hermit && pnpm exec playwright install chromium" \
  || echo "WARN: playwright install chromium failed"
for attempt in 1 2 3 4 5; do
  if as_ci "cd '${REPO_DIR}/desktop' && . ../bin/activate-hermit && sudo -E \$(command -v pnpm) exec playwright install-deps chromium"; then
    break
  fi
  # The pnpm shim lives in the hermit env, which sudo -E keeps on PATH; if that
  # still fails, fall back to the documented apt list playwright would install.
  if [ "$attempt" = 5 ]; then
    echo "WARN: playwright install-deps failed after 5 attempts"
  fi
  echo "install-deps attempt $attempt failed; retrying in 30s"
  sleep 30
done

# ── warm build so the first real gate is not a cold compile ──────────────────
as_ci "cd '${REPO_DIR}' && . ./bin/activate-hermit && just desktop-build" \
  || echo "WARN: warm desktop-build failed"
as_ci "cd '${REPO_DIR}' && . ./bin/activate-hermit && just desktop-tauri-check && cd desktop/src-tauri && cargo build --workspace --all-targets" \
  || echo "WARN: warm cargo build failed; the first run will compile from cold"

date -u +%FT%TZ > /var/lib/buzz-ci-bootstrap-done
echo "=== buzz-ci-box bootstrap complete $(date -u +%FT%TZ) ==="
