#!/bin/bash
# zs fork: cloud-init user-data for the on-demand Linux test box (buzz-ci-box).
#
# Reproduces the Blacksmith CI environment of `.github/workflows/_ci-desktop.yml`
# and `_ci-rust.yml` on one Ubuntu 24.04 x86_64 instance:
#   - the same apt packages, installed with the same retry loop CI uses
#   - the Playwright system dependencies for Chromium, installed as root
#   - a `ci` user holding the repo clone, hermit, the pnpm store, the cargo
#     caches and the Playwright browsers, all on the 200 GB gp3 root volume
#   - a root cron that stops the box once uptime passes 90 minutes
#
# Every step that fails leaves the completion marker unwritten, and
# provision.sh treats a missing marker as a failed box, so a half-built box is
# never adopted. The `ci` user has no sudo rights: branch code runs as `ci`,
# and root would let it disable the cost guard or poison the caches.
#
# Runs once, as root, at first boot; provision.sh also re-runs it over ssh to
# repair a box whose marker is missing. Re-running it is safe.
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
MARKER=/var/lib/buzz-ci-bootstrap-done
UPTIME_LIMIT_MINUTES=90

fatal() { echo "FATAL: $*"; exit 1; }

# The marker is written only at the very end. Remove any old one first, so a
# repair run that fails cannot leave a stale "this box is fine" marker behind.
rm -f "$MARKER"

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
apt_ok=0
for attempt in 1 2 3 4 5; do
  # A stop that lands mid-install (the provisioner's own trap, an idle alarm)
  # leaves dpkg interrupted, and every later apt call refuses until it is
  # configured; the rerun over ssh must heal that itself.
  dpkg --configure -a || true
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
      cron \
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
    apt_ok=1
    break
  fi
  echo "apt attempt $attempt failed; retrying in 30s"
  sleep 30
done
[ "$apt_ok" = 1 ] || fatal "apt install failed after 5 attempts"

# ── uptime guard: stop the box after 90 minutes whatever happens ─────────────
# The instance is launched with instance-initiated-shutdown-behavior=stop, so
# `shutdown -h now` stops it rather than terminating it. This is the belt to
# the CloudWatch idle alarm's braces, and bootstrap fails if it is not running:
# a hung high-CPU process never trips the idle alarm, so this is the only guard
# that still bounds the bill in that case.
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
systemctl enable --now cron || fatal "could not enable the cron service"
systemctl is-active --quiet cron \
  || fatal "the cron service is not active; the 90-minute uptime stop would not run"
[ -x /usr/local/sbin/buzz-ci-uptime-guard ] || fatal "the uptime guard is not executable"
echo "uptime guard active (limit ${UPTIME_LIMIT_MINUTES} minutes)"

# ── ci user, with no privilege ───────────────────────────────────────────────
if ! id -u "$CI_USER" >/dev/null 2>&1; then
  useradd --create-home --shell /bin/bash "$CI_USER" || fatal "useradd failed"
fi
# An earlier revision of this script granted `ci` passwordless sudo. Branch code
# runs as `ci`, so that made every tested branch root. Remove it on repair runs.
rm -f /etc/sudoers.d/buzz-ci
install -d -m 700 -o "$CI_USER" -g "$CI_USER" "${CI_HOME}/.ssh"
if [ -f /home/ubuntu/.ssh/authorized_keys ]; then
  install -m 600 -o "$CI_USER" -g "$CI_USER" \
    /home/ubuntu/.ssh/authorized_keys "${CI_HOME}/.ssh/authorized_keys"
fi

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
    || fatal "clone failed"
fi
as_ci "cd '${REPO_DIR}' && git config --local gc.auto 0" || fatal "git config failed"

# Hermit downloads its packages on first activation; do it now so the first
# real run does not pay for it.
as_ci "cd '${REPO_DIR}' && . ./bin/activate-hermit && just --version && node --version && pnpm --version && cargo --version" \
  || fatal "hermit bootstrap failed"

# ── toolchain extras CI installs through actions ─────────────────────────────
# cargo-nextest (taiki-e/install-action in _ci-rust.yml).
as_ci "mkdir -p \"\$HOME/.cargo/bin\" && curl -fsSL --retry 5 https://get.nexte.st/latest/linux | tar zxf - -C \"\$HOME/.cargo/bin\"" \
  || fatal "cargo-nextest install failed"

# ── pnpm store + node modules ────────────────────────────────────────────────
as_ci "cd '${REPO_DIR}' && . ./bin/activate-hermit && pnpm config set store-dir \"\$HOME/.pnpm-store\" --global" \
  || fatal "pnpm store-dir config failed"
as_ci "cd '${REPO_DIR}' && . ./bin/activate-hermit && just desktop-install" \
  || fatal "just desktop-install failed"

# ── Playwright browser (as ci) + system deps (as root) ───────────────────────
as_ci "cd '${REPO_DIR}/desktop' && . ../bin/activate-hermit && pnpm exec playwright install chromium" \
  || fatal "playwright install chromium failed"

# install-deps runs apt and needs root. Running it as root through hermit costs
# one extra toolchain download into /root, which is cheaper and far safer than
# giving the `ci` user sudo that branch code could then use.
deps_ok=0
for attempt in 1 2 3 4 5; do
  if ( cd "${REPO_DIR}" && . ./bin/activate-hermit && cd desktop \
       && pnpm exec playwright install-deps chromium ); then
    deps_ok=1
    break
  fi
  echo "install-deps attempt $attempt failed; retrying in 30s"
  sleep 30
done
[ "$deps_ok" = 1 ] || fatal "playwright install-deps failed after 5 attempts"

# ── warm build so the first real gate is not a cold compile ──────────────────
as_ci "cd '${REPO_DIR}' && . ./bin/activate-hermit && just desktop-build" \
  || fatal "warm desktop-build failed"
as_ci "cd '${REPO_DIR}' && . ./bin/activate-hermit && just desktop-tauri-check && cd desktop/src-tauri && cargo build --workspace --all-targets" \
  || fatal "warm cargo build failed"

install -d -m 755 "$(dirname "$MARKER")"
date -u +%FT%TZ > "$MARKER"
echo "=== buzz-ci-box bootstrap complete $(date -u +%FT%TZ) ==="
