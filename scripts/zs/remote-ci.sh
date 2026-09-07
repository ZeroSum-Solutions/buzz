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
# ── one box, one owner ───────────────────────────────────────────────────────
# The box is a single mutable working tree and a single billable instance, so a
# run must own it before it touches it. Ownership is a local lease: an exclusive
# `flock` on ~/.cache/zs/buzz-remote-ci.lock, taken before the first EC2 call and
# held for the whole run, exactly as scripts/zs/with-gate-lock.sh serialises the
# local gates. Only the lease holder starts or stops the box. The on-box flock at
# /home/ci/.remote-ci.lock stays as a second guard.
#
# The lease is local, so THE BOX IS OWNED BY THIS ONE MACHINE. Every caller runs
# on this Mac. A second machine sharing the same box would not see this lease and
# could stop the box under a running gate; give it its own box (a second
# provision.sh run in another region or with BUZZ_CI_* overridden) instead.
#
# This is the pre-push lane. Blacksmith stays the merge gate.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BOX_ENV="${REMOTE_CI_BOX_ENV:-${SCRIPT_DIR}/remote-ci/box.env}"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
INBOX_NOTES="${REMOTE_CI_NOTES_DIR:-$HOME/Inbox/notes}"
INBOX_MISC="${REMOTE_CI_MISC_DIR:-$HOME/Inbox/misc}"
LEASE_FILE="${REMOTE_CI_LEASE:-${XDG_CACHE_HOME:-$HOME/.cache}/zs/buzz-remote-ci.lock}"
LEASE_HOLDER="${LEASE_FILE}.holder"
REMOTE_REPO=/home/ci/buzz
REMOTE_LOCK=/home/ci/.remote-ci.lock
SSH_WAIT_SECONDS="${REMOTE_CI_SSH_WAIT:-300}"
STOPPING_WAIT_SECONDS="${REMOTE_CI_STOPPING_WAIT:-300}"
STOP_BACKOFF="${REMOTE_CI_STOP_BACKOFF:-5}"
# The box stops itself at 90 minutes of uptime, so a gate that outlives that
# would be killed mid-run by the shutdown. Bound it at the same figure.
GATE_TIMEOUT="${REMOTE_CI_GATE_TIMEOUT:-5400}"
LOG_CAP_BYTES="${REMOTE_CI_LOG_CAP:-209715200}"     # 200 MB, tail kept
REPORT_CAP_KB="${REMOTE_CI_REPORT_CAP_KB:-512000}"  # 500 MB
EXIT_MARKER='__REMOTE_CI_EXIT__'
LOCK_MARKER='__REMOTE_CI_LOCK__'

KEEP=0
STATUS_ONLY=0
PUSH_LOCAL=0
JOIN=0
PRINT_RUNNER=0
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
                   example after someone used --keep). The run still needs the
                   local lease and the on-box lock, and it stops the box only
                   if it holds the lease.
  --status         Print the instance state, its uptime, the local lease holder
                   and the on-box lock holder, then exit. Starts and stops
                   nothing, and does not need the lease.
  --print-runner   Print the script this run would execute on the box, then
                   exit. A testing aid; touches no AWS resource.
  -h, --help       This text.

Environment:
  ZS_AWS_BUZZ_CI_RUNNER_KEY_ID / ZS_AWS_BUZZ_CI_RUNNER_SECRET
                         The scoped runner key from ZS Vault (entries
                         aws_buzz_ci_runner_key_id and aws_buzz_ci_runner_secret,
                         written by provision.sh). Preferred. They are mapped
                         into the aws child environment only, never exported.
  AWS_PROFILE            Profile used for the EC2 calls when the vault pair is
                         unset (default buzz-ci-runner, which must exist).
  REMOTE_CI_BOX_ENV      Path to box.env (default scripts/zs/remote-ci/box.env).
  REMOTE_CI_LEASE        Lease file (default ~/.cache/zs/buzz-remote-ci.lock).
  REMOTE_CI_SSH_WAIT     Seconds to wait for SSH after start (default 300).
  REMOTE_CI_STOPPING_WAIT  Seconds to wait out a `stopping` box (default 300).
  REMOTE_CI_GATE_TIMEOUT Seconds the gate may run on the box (default 5400,
                         the box's own 90-minute uptime limit).
  REMOTE_CI_LOG_CAP      Maximum local log bytes, tail kept (default 200 MB).
  REMOTE_CI_REPORT_CAP_KB  Largest report tree copied back (default 500 MB).
  REMOTE_CI_NOTES_DIR    Log directory (default ~/Inbox/notes).
  REMOTE_CI_MISC_DIR     Report directory (default ~/Inbox/misc).

box.env is written by scripts/zs/remote-ci/provision.sh and holds
BUZZ_CI_INSTANCE_ID, BUZZ_CI_REGION, BUZZ_CI_KEY_PATH and BUZZ_CI_SSH_USER.
EOF
}

die() { printf 'remote-ci: %s\n' "$*" >&2; exit 2; }
log() { printf '==> %s\n' "$*" >&2; }

# A containment limit that can be set to 0 is not a limit: GNU timeout treats 0
# as "no timeout", and a 0-byte log cap keeps the whole log. Every numeric
# override is checked before the first AWS call.
require_int() { # require_int <name> <value> <min> <max>
  case "$2" in
    ''|*[!0-9]*) die "${1} must be a whole number, not '${2}'" ;;
  esac
  [ "$2" -ge "$3" ] && [ "$2" -le "$4" ] \
    || die "${1}=${2} is outside ${3}..${4}; refusing to weaken the limit it sets"
}

while [ $# -gt 0 ]; do
  case "$1" in
    --keep) KEEP=1; shift ;;
    --status) STATUS_ONLY=1; shift ;;
    --push-local) PUSH_LOCAL=1; shift ;;
    --join) JOIN=1; shift ;;
    --print-runner) PRINT_RUNNER=1; shift ;;
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

require_int REMOTE_CI_GATE_TIMEOUT "$GATE_TIMEOUT" 60 86400
require_int REMOTE_CI_LOG_CAP "$LOG_CAP_BYTES" 1048576 10737418240
require_int REMOTE_CI_REPORT_CAP_KB "$REPORT_CAP_KB" 1024 10485760
require_int REMOTE_CI_SSH_WAIT "$SSH_WAIT_SECONDS" 1 3600
require_int REMOTE_CI_STOPPING_WAIT "$STOPPING_WAIT_SECONDS" 1 3600
require_int REMOTE_CI_STOP_BACKOFF "$STOP_BACKOFF" 0 300

# ── the script this run executes on the box ──────────────────────────────────
# Written here and copied over, so no quoting of the just targets survives a
# round trip through two shells. REMOTE_BUNDLE is set later; when it is empty
# the runner fetches from origin instead.
REMOTE_BUNDLE=""
write_runner() { # write_runner <path>
  {
    printf '#!/bin/bash\nset -uo pipefail\n'
    printf '[ -f "$HOME/.buzz-ci-env" ] && . "$HOME/.buzz-ci-env"\n'
    printf 'repo="${REMOTE_CI_REPO_DIR:-%s}"\n' "$REMOTE_REPO"
    printf 'lock="${REMOTE_CI_LOCK_FILE:-%s}"\n' "$REMOTE_LOCK"
    # Second guard behind the caller's local lease: one run at a time on the box.
    printf 'exec 9>>"$lock" || exit 88\n'
    printf 'if ! flock -n 9; then echo "%s=busy"; echo "--- lock holder ---"; cat "$lock" 2>/dev/null; exit 89; fi\n' \
      "$LOCK_MARKER"
    printf 'echo "%s=acquired"\n' "$LOCK_MARKER"
    printf 'printf "host=%%s\\npid=%%s\\nbranch=%%s\\nstarted=%%s\\n" %q %q %q "$(date -u +%%FT%%TZ)" > "$lock"\n' \
      "$(hostname)" "$$" "$BRANCH"
    printf 'cd "$repo" || exit 90\n'
    printf 'git fetch origin --prune --tags || exit 91\n'
    if [ -n "$REMOTE_BUNDLE" ]; then
      printf 'git fetch %q %q || exit 92\n' "$REMOTE_BUNDLE" "$BRANCH"
      printf 'target=FETCH_HEAD\n'
    else
      printf 'target=%q\n' "origin/${BRANCH}"
    fi
    # The checkout is shared and long-lived: a previous gate can have modified a
    # tracked file or dropped untracked sources into it, and a plain checkout
    # keeps both. Reset to the target and drop untracked files, but keep ignored
    # ones -- node_modules, the cargo target dirs and the Playwright browsers are
    # the whole point of the persistent volume.
    printf 'git checkout --detach "$target" || exit 93\n'
    printf 'git reset --hard "$target" || exit 93\n'
    printf 'git clean -f -d || exit 93\n'
    printf 'dirty="$(git status --porcelain)"\n'
    printf 'if [ -n "$dirty" ]; then\n'
    printf '  echo "remote-ci: the checkout is still dirty after reset; refusing to gate a mixed tree:"\n'
    printf '  printf "%%s\\n" "$dirty"\n'
    printf '  echo "%s=97"\n' "$EXIT_MARKER"
    printf '  exit 97\n'
    printf 'fi\n'
    printf 'git --no-pager log -1 --oneline\n'
    printf 'rm -rf desktop/playwright-report desktop/playwright-report.json\n'
    printf '. ./bin/activate-hermit || exit 94\n'
    # The box was bootstrapped from zs/main. A branch that changes
    # pnpm-lock.yaml, package.json or the Playwright version would otherwise be
    # gated against the wrong dependencies, and `just ci` installs nothing.
    printf 'just desktop-install-ci || exit 95\n'
    printf '( cd desktop && pnpm exec playwright install chromium ) || exit 96\n'
    printf 'timeout %q just' "$GATE_TIMEOUT"
    printf ' %q' "${TARGETS[@]}"
    printf '\nrc=$?\n'
    printf 'echo "%s=$rc"\n' "$EXIT_MARKER"
  } > "$1"
}

if [ "$PRINT_RUNNER" = 1 ]; then
  [ -n "$BRANCH" ] || die "--print-runner needs a branch"
  write_runner /dev/stdout
  exit 0
fi

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

# The runner key from ZS Vault is injected into the aws child environment only:
# it is never exported from this script and never appears in argv.
VAULT_RUNNER=0
if [ -n "${AWS_PROFILE-}" ]; then
  AWS=(aws --profile "$AWS_PROFILE" --region "$BUZZ_CI_REGION" --output text)
elif [ -n "${ZS_AWS_BUZZ_CI_RUNNER_KEY_ID-}" ] && [ -n "${ZS_AWS_BUZZ_CI_RUNNER_SECRET-}" ]; then
  VAULT_RUNNER=1
  AWS=(aws --region "$BUZZ_CI_REGION" --output text)
else
  AWS=(aws --profile buzz-ci-runner --region "$BUZZ_CI_REGION" --output text)
fi
run_aws() {
  if [ "$VAULT_RUNNER" = 1 ]; then
    AWS_ACCESS_KEY_ID="$ZS_AWS_BUZZ_CI_RUNNER_KEY_ID" \
    AWS_SECRET_ACCESS_KEY="$ZS_AWS_BUZZ_CI_RUNNER_SECRET" \
      "${AWS[@]}" "$@"
  else
    "${AWS[@]}" "$@"
  fi
}
command -v aws >/dev/null 2>&1 || die "aws CLI not on PATH"

ec2_field() { # ec2_field <jmespath>
  run_aws ec2 describe-instances --instance-ids "$BUZZ_CI_INSTANCE_ID" --query "$1" \
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
  if [ -s "$LEASE_HOLDER" ]; then
    printf 'local lease holder:\n'; cat "$LEASE_HOLDER"
  else
    printf 'local lease holder: none recorded\n'
  fi
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
        printf 'on-box lock holder:\n%s\n' "$holder"
      else
        printf 'on-box lock holder: none recorded\n'
      fi
    fi
  fi
  exit 0
fi

[ -n "$BRANCH" ] || { usage >&2; die "no branch given"; }
[ -f "$KEY_PATH" ] || die "private key ${KEY_PATH} is missing; re-run provision.sh"

# ── the local lease: taken before the first EC2 call ─────────────────────────
# Same mechanism as scripts/zs/with-gate-lock.sh. The flock is taken by a child
# on an inherited descriptor, so it belongs to this shell's open file
# description and is held until this shell exits, however it exits.
LEASE_OWNED=0
mkdir -p "$(dirname "$LEASE_FILE")" || die "could not create $(dirname "$LEASE_FILE")"
exec 9>>"$LEASE_FILE" || die "could not open the lease file ${LEASE_FILE}"
if python3 -c 'import fcntl, sys
try:
    fcntl.flock(9, fcntl.LOCK_EX | fcntl.LOCK_NB)
except OSError:
    sys.exit(1)
'; then
  LEASE_OWNED=1
  printf 'pid=%s\nbranch=%s\ntargets=%s\nstarted=%s\n' \
    "$$" "$BRANCH" "${TARGETS[*]}" "$(date -u +%FT%TZ)" > "$LEASE_HOLDER"
else
  holder_text="(no holder file at ${LEASE_HOLDER})"
  [ -s "$LEASE_HOLDER" ] && holder_text="$(cat "$LEASE_HOLDER")"
  die "another remote-ci run on this machine holds the box:
${holder_text}
Wait for it to finish. The box is one mutable working tree and one billable
instance, so runs take turns; this lease is what stops one run's cleanup from
stopping the box under another."
fi

SAFE_BRANCH="$(printf '%s' "$BRANCH" | tr '/ ' '--')"
TS="$(date -u +%Y%m%dT%H%M%SZ)"
LOG_FILE="${INBOX_NOTES}/remote-ci-${SAFE_BRANCH}-${TS}.log"
REPORT_DIR="${INBOX_MISC}/remote-ci-report-${SAFE_BRANCH}-${TS}"
mkdir -p "$INBOX_NOTES" "$INBOX_MISC"

WORKDIR="$(mktemp -d)" || die "mktemp failed"
[ -d "$WORKDIR" ] || die "mktemp produced no directory"
WE_STARTED=0
STOPPED=0
STOP_FAILED=0

# Stop the box and confirm EC2 agrees. A failed stop is never a warning: the box
# bills at about $1.20 an hour, so it is surfaced and it fails the run.
stop_box() {
  local attempt
  for attempt in 1 2 3 4 5; do
    if run_aws ec2 stop-instances --instance-ids "$BUZZ_CI_INSTANCE_ID" >/dev/null 2>&1 \
       && run_aws ec2 wait instance-stopped --instance-ids "$BUZZ_CI_INSTANCE_ID" >/dev/null 2>&1; then
      STOPPED=1
      log "stopped ${BUZZ_CI_INSTANCE_ID}"
      return 0
    fi
    log "stop attempt ${attempt} of 5 failed for ${BUZZ_CI_INSTANCE_ID}"
    [ "$attempt" = 5 ] || sleep $(( STOP_BACKOFF * attempt ))
  done
  STOP_FAILED=1
  cat >&2 <<EOF

remote-ci: COULD NOT STOP ${BUZZ_CI_INSTANCE_ID} AFTER 5 ATTEMPTS.
It is still billing, at about \$1.20 per hour. Stop it now:

  aws --region ${BUZZ_CI_REGION} ec2 stop-instances --instance-ids ${BUZZ_CI_INSTANCE_ID}

The box also stops itself after 90 minutes of uptime, and the
buzz-ci-box-idle CloudWatch alarm stops it after 30 minutes below 5% CPU,
but neither is a substitute for checking the console.

EOF
  return 1
}

cleanup() {
  local rc=$?
  rm -rf "$WORKDIR"
  if [ "$LEASE_OWNED" = 1 ] && [ "$STOPPED" = 0 ]; then
    if [ "$KEEP" = 1 ]; then
      log "--keep: leaving ${BUZZ_CI_INSTANCE_ID} running"
    elif [ "$WE_STARTED" = 1 ] || [ "$JOIN" = 1 ]; then
      log "stopping ${BUZZ_CI_INSTANCE_ID}"
      stop_box || true
    fi
  fi
  [ "$LEASE_OWNED" = 1 ] && rm -f "$LEASE_HOLDER"
  # A green gate whose box did not stop is not a green run.
  if [ "$STOP_FAILED" = 1 ] && [ "$rc" -eq 0 ]; then rc=1; fi
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
    # We hold the lease, so no other remote-ci run on this machine started it.
    if [ "$JOIN" = 0 ]; then
      holder=""
      if [ "$state" = running ]; then
        ip="$(instance_ip)"
        [ "$ip" != None ] && holder="$(read_remote_lock "$ip")"
      fi
      msg="the box is already ${state} although no other run on this machine holds the lease.
Someone started it by hand, a previous run used --keep, or a stop failed."
      [ -n "$holder" ] && msg="${msg}
on-box lock holder:
${holder}"
      die "${msg}
Check it with --status, then pass --join to use it (the run will stop it when
it finishes)."
    fi
    log "--join: the box is already ${state}"
    ;;
  stopped)
    log "starting ${BUZZ_CI_INSTANCE_ID}"
    run_aws ec2 start-instances --instance-ids "$BUZZ_CI_INSTANCE_ID" >/dev/null \
      || die "start-instances failed"
    WE_STARTED=1
    ;;
  *)
    die "the instance is '${state}'; this script only handles running, pending, stopping and stopped"
    ;;
esac

log "waiting for the instance to run"
run_aws ec2 wait instance-running --instance-ids "$BUZZ_CI_INSTANCE_ID" \
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
RUN_ID="${TS}-$$"
REMOTE_RUNNER="/tmp/remote-ci-run-${RUN_ID}.sh"
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

write_runner "${WORKDIR}/run.sh"
scp "${SSH_OPTS[@]}" "${WORKDIR}/run.sh" "${REMOTE}:${REMOTE_RUNNER}" >/dev/null \
  || die "scp of the runner failed"

log "running: just ${TARGETS[*]} on ${BRANCH} (gate timeout ${GATE_TIMEOUT}s)"
log "log: ${LOG_FILE}"
REMOTE_CMD="bash ${REMOTE_RUNNER}; rm -f ${REMOTE_RUNNER} ${REMOTE_BUNDLE}"

# The typescript goes through a pipe, not straight to a file, so the log can be
# capped: a gate that spins printing output must not fill this Mac's disk. The
# capper streams every byte to the terminal and keeps the tail on disk.
CAPPER_PY="${WORKDIR}/log-capper.py"
cat > "$CAPPER_PY" <<'CAPPER'
import sys

cap = int(sys.argv[1])
path = sys.argv[2]
out = open(path, "wb")
size = 0
for chunk in iter(lambda: sys.stdin.buffer.read(65536), b""):
    sys.stdout.buffer.write(chunk)
    sys.stdout.buffer.flush()
    out.write(chunk)
    out.flush()
    size += len(chunk)
    if size > cap:
        out.close()
        with open(path, "rb") as fh:
            tail = fh.read()[-(cap // 2):]
        out = open(path, "wb")
        out.write(b"[remote-ci: log passed %d bytes; the head was trimmed]\n" % cap)
        out.write(tail)
        out.flush()
        size = out.tell()
out.close()
CAPPER

: > "$LOG_FILE"
if [ "$(uname -s)" = Darwin ]; then
  script -q /dev/null ssh -tt "${SSH_OPTS[@]}" "$REMOTE" "$REMOTE_CMD" \
    | python3 "$CAPPER_PY" "$LOG_CAP_BYTES" "$LOG_FILE"
else
  # script -c takes one string, so every argument is quoted individually: a key
  # path with a space must survive being split again by script's own shell.
  linux_cmd="$(printf '%q ' ssh -tt "${SSH_OPTS[@]}" "$REMOTE" "$REMOTE_CMD")"
  script -q -e -c "$linux_cmd" /dev/null \
    | python3 "$CAPPER_PY" "$LOG_CAP_BYTES" "$LOG_FILE"
fi

# `script` does not report the remote command's status portably, so the runner
# prints its own exit code as the last line and we read it back.
if grep -aq "${LOCK_MARKER}=acquired" "$LOG_FILE"; then
  :
elif grep -aq "${LOCK_MARKER}=busy" "$LOG_FILE"; then
  die "the on-box lock is held by another run; nothing was run.
$(sed -n '/--- lock holder ---/,$p' "$LOG_FILE")"
else
  die "the run never reported whether it took the on-box lock; treating that as a failure"
fi

GATE_RC="$(tr -d '\r' < "$LOG_FILE" | grep -a "^${EXIT_MARKER}=" | tail -1 | cut -d= -f2)"
case "$GATE_RC" in
  ''|*[!0-9]*)
    log "the run did not report an exit code; treating it as a failure"
    GATE_RC=1
    ;;
esac
[ "$GATE_RC" = 124 ] && log "the gate hit its ${GATE_TIMEOUT}s timeout"

# ── bring the report home ────────────────────────────────────────────────────
# An absent report and a broken probe are different facts. The probe prints
# "absent" or "present" on stdout; anything else is a failure we name.
REPORT_NOTE=""
probe_err="${WORKDIR}/probe.err"
report_state="$(ssh "${SSH_OPTS[@]}" "$REMOTE" \
  "test -d ${REMOTE_REPO}/desktop/playwright-report && echo present || echo absent" \
  2>"$probe_err")" || report_state="probe-failed"
case "$report_state" in
  absent)
    :
    ;;
  present)
    mkdir -p "$REPORT_DIR"
    # Bounded transfer, not a preflight size check: a gate that is still
    # writing could pass a `du` check and then overrun the cap during the copy.
    # tar streams a snapshot, `head -c` cuts it at the cap, and the whole thing
    # has a deadline. A truncated stream makes tar fail, which is reported.
    cap_bytes=$(( REPORT_CAP_KB * 1024 ))
    if ssh "${SSH_OPTS[@]}" "$REMOTE" \
         "timeout 600 tar -C ${REMOTE_REPO}/desktop -cf - playwright-report" \
         2>"${WORKDIR}/tar.err" \
       | head -c "$cap_bytes" \
       | tar -C "$REPORT_DIR" -xf - 2>"${WORKDIR}/untar.err"; then
      log "playwright report: ${REPORT_DIR}"
    else
      REPORT_NOTE="the playwright report did not copy back in full (over the ${REPORT_CAP_KB} KB cap, or the transfer failed)"
      log "WARNING: ${REPORT_NOTE}"
      log "read it on the box: ssh -i ${KEY_PATH} ${REMOTE}"
      [ -s "${WORKDIR}/tar.err" ] && log "remote tar said: $(head -3 "${WORKDIR}/tar.err")"
    fi
    ;;
  *)
    REPORT_NOTE="could not tell whether a playwright report exists: $(head -3 "$probe_err" | tr '\n' ' ')"
    log "WARNING: ${REPORT_NOTE}"
    ;;
esac

log "gate exit code: ${GATE_RC}"
[ -n "$REPORT_NOTE" ] && log "report note: ${REPORT_NOTE}"
exit "$GATE_RC"
