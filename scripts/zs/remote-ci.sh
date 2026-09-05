#!/usr/bin/env bash
# zs fork: run the heavy gates on the on-demand AWS Linux box instead of this Mac.
#
#   scripts/zs/remote-ci.sh <branch> [just targets...]
#
# Starts the stopped EC2 instance provisioned by scripts/zs/remote-ci/provision.sh,
# checks the branch out on it, refreshes its dependencies, runs `just <targets>`
# (default: `ci`), streams the log to this terminal and to ~/Inbox/notes/, copies
# desktop/playwright-report back to ~/Inbox/misc/, then stops the instance again.
# The exit code is the gate's own.
#
# One box, one run: the run holds an exclusive flock on the box and refuses to
# start when someone else already has it. Only the lock owner stops the box.
#
# This is the pre-push lane. Blacksmith stays the merge gate.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BOX_ENV="${REMOTE_CI_BOX_ENV:-${SCRIPT_DIR}/remote-ci/box.env}"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
INBOX_NOTES="${REMOTE_CI_NOTES_DIR:-$HOME/Inbox/notes}"
INBOX_MISC="${REMOTE_CI_MISC_DIR:-$HOME/Inbox/misc}"
REMOTE_REPO=/home/ci/buzz
REMOTE_LOCK=/home/ci/.remote-ci.lock
SSH_WAIT_SECONDS="${REMOTE_CI_SSH_WAIT:-300}"
STOPPING_WAIT_SECONDS="${REMOTE_CI_STOPPING_WAIT:-300}"
EXIT_MARKER='__REMOTE_CI_EXIT__'
LOCK_MARKER='__REMOTE_CI_LOCK__'

KEEP=0
STATUS_ONLY=0
PUSH_LOCAL=0
JOIN=0
BRANCH=""
TARGETS=()

usage() {
  cat <<'EOF'
usage: scripts/zs/remote-ci.sh [flags] <branch> [just targets...]

Runs the named just targets on the on-demand AWS Linux test box and stops the
box afterwards. Default target: ci.

  scripts/zs/remote-ci.sh zs/main
  scripts/zs/remote-ci.sh feat/my-branch desktop-test desktop-tauri-test
  scripts/zs/remote-ci.sh --push-local feat/not-pushed-yet desktop-tauri-test

Flags:
  --push-local     Send the local branch's COMMITTED tree over ssh as a git
                   bundle instead of fetching it from origin. Use for a branch
                   that is not pushed yet. Uncommitted work is never sent.
  --keep           Leave the instance running when the run finishes. Costs
                   about $1.20 per hour. The box still stops itself after 90
                   minutes of uptime, and the CloudWatch idle alarm stops it
                   after 30 minutes below 5% CPU.
  --join           Do not refuse just because the box is already running (for
                   example after someone used --keep). The run still takes the
                   exclusive lock on the box and gives up if another run holds
                   it, and it stops the box only if it owned that lock.
  --status         Print the instance state, its uptime and the current lock
                   holder, then exit. Does not start or stop anything.
  -h, --help       This text.

Environment:
  AWS_PROFILE            Profile used for the EC2 calls. Default
                         buzz-ci-runner. The scoped key also works straight
                         from ZS Vault: the two entries export
                         AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY, and this
                         script uses them when AWS_PROFILE is unset.
  REMOTE_CI_BOX_ENV      Path to box.env (default scripts/zs/remote-ci/box.env).
  REMOTE_CI_SSH_WAIT     Seconds to wait for SSH after start (default 300).
  REMOTE_CI_STOPPING_WAIT  Seconds to wait out a `stopping` box (default 300).
  REMOTE_CI_NOTES_DIR    Log directory (default ~/Inbox/notes).
  REMOTE_CI_MISC_DIR     Report directory (default ~/Inbox/misc).

box.env is written by scripts/zs/remote-ci/provision.sh and holds
BUZZ_CI_INSTANCE_ID, BUZZ_CI_REGION, BUZZ_CI_KEY_PATH and BUZZ_CI_SSH_USER.
EOF
}

die() { printf 'remote-ci: %s\n' "$*" >&2; exit 2; }
log() { printf '==> %s\n' "$*" >&2; }

while [ $# -gt 0 ]; do
  case "$1" in
    --keep) KEEP=1; shift ;;
    --status) STATUS_ONLY=1; shift ;;
    --push-local) PUSH_LOCAL=1; shift ;;
    --join) JOIN=1; shift ;;
    -h|--help) usage; exit 0 ;;
    --) shift; break ;;
    -*) die "unknown flag: $1 (try --help)" ;;
    *) break ;;
  esac
done
if [ $# -gt 0 ]; then
  BRANCH="$1"; shift
  TARGETS=("$@")
fi
[ "${#TARGETS[@]}" -gt 0 ] || TARGETS=(ci)

# ── box.env ──────────────────────────────────────────────────────────────────
# Parsed key by key rather than sourced: box.env is generated, but a generated
# file is still an external input and must not be able to run code.
BUZZ_CI_INSTANCE_ID=""; BUZZ_CI_REGION=""; BUZZ_CI_KEY_PATH=""; BUZZ_CI_SSH_USER=""
[ -f "$BOX_ENV" ] || die "no box.env at ${BOX_ENV}. Run scripts/zs/remote-ci/provision.sh first."
while IFS= read -r line || [ -n "$line" ]; do
  case "$line" in
    ''|'#'*) continue ;;
  esac
  key="${line%%=*}"
  value="${line#*=}"
  case "$key" in
    BUZZ_CI_INSTANCE_ID) BUZZ_CI_INSTANCE_ID="$value" ;;
    BUZZ_CI_REGION) BUZZ_CI_REGION="$value" ;;
    BUZZ_CI_KEY_PATH) BUZZ_CI_KEY_PATH="$value" ;;
    BUZZ_CI_SSH_USER) BUZZ_CI_SSH_USER="$value" ;;
    *) die "unexpected key in ${BOX_ENV}: ${key}" ;;
  esac
done < "$BOX_ENV"

case "$BUZZ_CI_INSTANCE_ID" in
  i-[0-9a-f]*) ;;
  *) die "BUZZ_CI_INSTANCE_ID in ${BOX_ENV} is not an instance id: '${BUZZ_CI_INSTANCE_ID}'" ;;
esac
case "$BUZZ_CI_REGION" in
  [a-z]*-[a-z]*-[0-9]) ;;
  *) die "BUZZ_CI_REGION in ${BOX_ENV} is not a region: '${BUZZ_CI_REGION}'" ;;
esac
[ -n "$BUZZ_CI_KEY_PATH" ] || die "BUZZ_CI_KEY_PATH missing from ${BOX_ENV}"
case "$BUZZ_CI_SSH_USER" in
  [a-z_][a-z0-9_-]*) ;;
  *) die "BUZZ_CI_SSH_USER in ${BOX_ENV} is not a user name: '${BUZZ_CI_SSH_USER}'" ;;
esac
KEY_PATH="${BUZZ_CI_KEY_PATH/#\~/$HOME}"
KNOWN_HOSTS="${REMOTE_CI_KNOWN_HOSTS:-$HOME/.ssh/known_hosts.buzz-ci-box}"

AWS=(aws --region "$BUZZ_CI_REGION" --output text)
if [ -n "${AWS_PROFILE-}" ]; then
  AWS=(aws --profile "$AWS_PROFILE" --region "$BUZZ_CI_REGION" --output text)
elif [ -z "${AWS_ACCESS_KEY_ID-}" ]; then
  AWS=(aws --profile buzz-ci-runner --region "$BUZZ_CI_REGION" --output text)
fi
command -v aws >/dev/null 2>&1 || die "aws CLI not on PATH"

ec2_field() { # ec2_field <jmespath>
  "${AWS[@]}" ec2 describe-instances --instance-ids "$BUZZ_CI_INSTANCE_ID" --query "$1" \
    || die "describe-instances failed (is the AWS credential set up?)"
}
instance_state() { ec2_field 'Reservations[0].Instances[0].State.Name'; }
instance_ip() { ec2_field 'Reservations[0].Instances[0].PublicIpAddress'; }

SSH_OPTS=(-o StrictHostKeyChecking=accept-new -o UserKnownHostsFile="$KNOWN_HOSTS"
          -o ConnectTimeout=10 -o ServerAliveInterval=30 -o BatchMode=yes -i "$KEY_PATH")

read_remote_lock() { # read_remote_lock <ip>; best-effort diagnostic only
  ssh "${SSH_OPTS[@]}" -o ConnectTimeout=5 "${BUZZ_CI_SSH_USER}@${1}" \
    "cat ${REMOTE_LOCK} 2>/dev/null" 2>/dev/null || true
}

uptime_minutes() { # uptime_minutes <launch-time>
  local launched="$1" epoch
  epoch="$(date -j -f '%Y-%m-%dT%H:%M:%S' "${launched%%+*}" +%s 2>/dev/null \
    || date -d "$launched" +%s 2>/dev/null || echo '')"
  [ -n "$epoch" ] || return 1
  printf '%d\n' $(( ( $(date +%s) - epoch ) / 60 ))
}

if [ "$STATUS_ONLY" = 1 ]; then
  state="$(instance_state)"
  printf 'instance %s (%s): %s\n' "$BUZZ_CI_INSTANCE_ID" "$BUZZ_CI_REGION" "$state"
  if [ "$state" = running ]; then
    launched="$(ec2_field 'Reservations[0].Instances[0].LaunchTime')"
    printf 'launched: %s\n' "$launched"
    if mins="$(uptime_minutes "$launched")"; then
      printf 'uptime: %s minutes (box stops itself at 90)\n' "$mins"
    fi
    ip="$(instance_ip)"
    printf 'public ip: %s\n' "$ip"
    if [ -f "$KEY_PATH" ] && [ "$ip" != None ]; then
      holder="$(read_remote_lock "$ip")"
      if [ -n "$holder" ]; then
        printf 'lock holder:\n%s\n' "$holder"
      else
        printf 'lock holder: none recorded\n'
      fi
    fi
  fi
  exit 0
fi

[ -n "$BRANCH" ] || { usage >&2; die "no branch given"; }
[ -f "$KEY_PATH" ] || die "private key ${KEY_PATH} is missing; re-run provision.sh"

SAFE_BRANCH="$(printf '%s' "$BRANCH" | tr '/ ' '--')"
TS="$(date -u +%Y%m%dT%H%M%SZ)"
LOG_FILE="${INBOX_NOTES}/remote-ci-${SAFE_BRANCH}-${TS}.log"
REPORT_DIR="${INBOX_MISC}/remote-ci-report-${SAFE_BRANCH}-${TS}"
mkdir -p "$INBOX_NOTES" "$INBOX_MISC"

WORKDIR="$(mktemp -d)" || die "mktemp failed"
[ -d "$WORKDIR" ] || die "mktemp produced no directory"
WE_STARTED=0
LOCK_OWNED=0
STOPPED=0
cleanup() {
  local rc=$?
  rm -rf "$WORKDIR"
  if [ "$KEEP" = 1 ]; then
    log "--keep: leaving ${BUZZ_CI_INSTANCE_ID} running"
  elif [ "$STOPPED" = 1 ]; then
    :
  elif [ "$LOCK_OWNED" = 1 ] || [ "$WE_STARTED" = 1 ]; then
    STOPPED=1
    log "stopping ${BUZZ_CI_INSTANCE_ID}"
    "${AWS[@]}" ec2 stop-instances --instance-ids "$BUZZ_CI_INSTANCE_ID" >/dev/null \
      || log "WARNING: stop-instances failed. Stop it by hand; the box also stops itself at 90 minutes of uptime."
  else
    log "another run owns the box; leaving it alone"
  fi
  exit "$rc"
}
trap cleanup EXIT INT TERM

# ── start and wait ───────────────────────────────────────────────────────────
state="$(instance_state)"
if [ "$state" = stopping ]; then
  # stop-instances is asynchronous, so a run started right after the previous
  # one commonly lands here. StartInstances is rejected while stopping.
  log "instance is stopping; waiting up to ${STOPPING_WAIT_SECONDS}s for it to stop"
  deadline=$(( $(date +%s) + STOPPING_WAIT_SECONDS ))
  while [ "$state" = stopping ]; do
    [ "$(date +%s)" -lt "$deadline" ] \
      || die "the instance was still stopping after ${STOPPING_WAIT_SECONDS}s"
    sleep 10
    state="$(instance_state)"
  done
fi

case "$state" in
  running|pending)
    if [ "$JOIN" = 0 ]; then
      holder=""
      if [ "$state" = running ]; then
        ip="$(instance_ip)"
        [ "$ip" != None ] && holder="$(read_remote_lock "$ip")"
      fi
      msg="the box is already ${state}; another run is probably using it."
      [ -n "$holder" ] && msg="${msg}
lock holder:
${holder}"
      die "${msg}
Wait for it, or pass --join to share the box (the run still takes the
exclusive lock and gives up if that run still holds it)."
    fi
    log "--join: the box is already ${state}"
    ;;
  stopped)
    log "starting ${BUZZ_CI_INSTANCE_ID}"
    "${AWS[@]}" ec2 start-instances --instance-ids "$BUZZ_CI_INSTANCE_ID" >/dev/null \
      || die "start-instances failed"
    WE_STARTED=1
    ;;
  *)
    die "the instance is '${state}'; this script only handles running, pending, stopping and stopped"
    ;;
esac

log "waiting for the instance to run"
"${AWS[@]}" ec2 wait instance-running --instance-ids "$BUZZ_CI_INSTANCE_ID" \
  || die "instance did not reach running"
IP="$(instance_ip)"
[ -n "$IP" ] && [ "$IP" != None ] || die "instance has no public IP"
REMOTE="${BUZZ_CI_SSH_USER}@${IP}"

log "waiting up to ${SSH_WAIT_SECONDS}s for ssh on ${REMOTE}"
deadline=$(( $(date +%s) + SSH_WAIT_SECONDS ))
until ssh "${SSH_OPTS[@]}" "$REMOTE" true 2>/dev/null; do
  [ "$(date +%s)" -lt "$deadline" ] || die "ssh did not come up within ${SSH_WAIT_SECONDS}s"
  sleep 5
done

# ── send the tree ────────────────────────────────────────────────────────────
# Per-run remote paths: two runs must never overwrite each other's inputs, even
# in the window before one of them takes the lock.
RUN_ID="${TS}-$$"
REMOTE_RUNNER="/tmp/remote-ci-run-${RUN_ID}.sh"
REMOTE_BUNDLE=""
if [ "$PUSH_LOCAL" = 1 ]; then
  rev="$(git -C "$REPO_ROOT" rev-parse --verify "$BRANCH" 2>/dev/null)" \
    || die "--push-local: '${BRANCH}' is not a ref in ${REPO_ROOT}"
  bundle="${WORKDIR}/local.bundle"
  base="$(git -C "$REPO_ROOT" merge-base origin/zs/main "$rev" 2>/dev/null || true)"
  if [ -n "$base" ] && git -C "$REPO_ROOT" bundle create "$bundle" "${base}..${BRANCH}" 2>/dev/null; then
    log "bundled ${base}..${BRANCH} (incremental)"
  else
    git -C "$REPO_ROOT" bundle create "$bundle" "$BRANCH" \
      || die "--push-local: git bundle create failed"
    log "bundled the full history of ${BRANCH}"
  fi
  REMOTE_BUNDLE="/tmp/remote-ci-${RUN_ID}.bundle"
  scp "${SSH_OPTS[@]}" "$bundle" "${REMOTE}:${REMOTE_BUNDLE}" >/dev/null \
    || die "--push-local: scp of the bundle failed"
fi

# ── remote runner ────────────────────────────────────────────────────────────
# Written locally and copied over, so no quoting of the just targets survives a
# round trip through two shells.
{
  printf '#!/bin/bash\nset -uo pipefail\n'
  printf '[ -f "$HOME/.buzz-ci-env" ] && . "$HOME/.buzz-ci-env"\n'
  # One run at a time on this box. The lock is held for the whole run and is
  # released by the kernel when this shell exits, however it exits.
  printf 'exec 9>>%q || exit 88\n' "$REMOTE_LOCK"
  printf 'if ! flock -n 9; then echo "%s=busy"; echo "--- lock holder ---"; cat %q 2>/dev/null; exit 89; fi\n' \
    "$LOCK_MARKER" "$REMOTE_LOCK"
  printf 'echo "%s=acquired"\n' "$LOCK_MARKER"
  printf 'printf "host=%%s\\npid=%%s\\nbranch=%%s\\nstarted=%%s\\n" %q %q %q "$(date -u +%%FT%%TZ)" > %q\n' \
    "$(hostname)" "$$" "$BRANCH" "$REMOTE_LOCK"
  printf 'cd %q || exit 90\n' "$REMOTE_REPO"
  printf 'git fetch origin --prune --tags || exit 91\n'
  if [ -n "$REMOTE_BUNDLE" ]; then
    printf 'git fetch %q %q || exit 92\n' "$REMOTE_BUNDLE" "$BRANCH"
    printf 'git checkout --detach FETCH_HEAD || exit 93\n'
  else
    printf 'git checkout --detach %q || exit 93\n' "origin/${BRANCH}"
  fi
  printf 'git --no-pager log -1 --oneline\n'
  printf 'rm -rf desktop/playwright-report desktop/playwright-report.json\n'
  printf '. ./bin/activate-hermit || exit 94\n'
  # The box was bootstrapped from zs/main. A branch that changes
  # pnpm-lock.yaml, package.json or the Playwright version would otherwise be
  # gated against the wrong dependencies, and `just ci` installs nothing.
  printf 'just desktop-install-ci || exit 95\n'
  printf '( cd desktop && pnpm exec playwright install chromium ) || exit 96\n'
  printf 'just'
  printf ' %q' "${TARGETS[@]}"
  printf '\nrc=$?\n'
  printf 'echo "%s=$rc"\n' "$EXIT_MARKER"
} > "${WORKDIR}/run.sh"
scp "${SSH_OPTS[@]}" "${WORKDIR}/run.sh" "${REMOTE}:${REMOTE_RUNNER}" >/dev/null \
  || die "scp of the runner failed"

log "running: just ${TARGETS[*]} on ${BRANCH}"
log "log: ${LOG_FILE}"
REMOTE_CMD="bash ${REMOTE_RUNNER}; rm -f ${REMOTE_RUNNER} ${REMOTE_BUNDLE}"
if [ "$(uname -s)" = Darwin ]; then
  script -q "$LOG_FILE" ssh -tt "${SSH_OPTS[@]}" "$REMOTE" "$REMOTE_CMD"
else
  # script -c takes one string, so every argument is quoted individually: a key
  # path with a space must survive being split again by script's own shell.
  linux_cmd="$(printf '%q ' ssh -tt "${SSH_OPTS[@]}" "$REMOTE" "$REMOTE_CMD")"
  script -q -e -c "$linux_cmd" "$LOG_FILE"
fi

# `script` does not report the remote command's status portably, so the runner
# prints its own exit code as the last line and we read it back.
if grep -aq "${LOCK_MARKER}=acquired" "$LOG_FILE"; then
  LOCK_OWNED=1
elif grep -aq "${LOCK_MARKER}=busy" "$LOG_FILE"; then
  die "another run holds the lock on the box; nothing was run and the box was left alone.
$(sed -n '/--- lock holder ---/,$p' "$LOG_FILE")"
else
  die "the run never reported whether it took the lock on the box; treating that as a failure"
fi

GATE_RC="$(tr -d '\r' < "$LOG_FILE" | grep -a "^${EXIT_MARKER}=" | tail -1 | cut -d= -f2)"
case "$GATE_RC" in
  ''|*[!0-9]*)
    log "the run did not report an exit code; treating it as a failure"
    GATE_RC=1
    ;;
esac

# ── bring the report home ────────────────────────────────────────────────────
if ssh "${SSH_OPTS[@]}" "$REMOTE" "test -d ${REMOTE_REPO}/desktop/playwright-report" 2>/dev/null; then
  mkdir -p "$REPORT_DIR"
  if scp -r "${SSH_OPTS[@]}" "${REMOTE}:${REMOTE_REPO}/desktop/playwright-report" "$REPORT_DIR/" >/dev/null; then
    log "playwright report: ${REPORT_DIR}"
  else
    log "WARNING: could not copy the playwright report back"
  fi
fi

log "gate exit code: ${GATE_RC}"
exit "$GATE_RC"
