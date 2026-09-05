#!/usr/bin/env bash
# zs fork: run the heavy gates on the on-demand AWS Linux box instead of this Mac.
#
#   scripts/zs/remote-ci.sh <branch> [just targets...]
#
# Starts the stopped EC2 instance provisioned by scripts/zs/remote-ci/provision.sh,
# checks the branch out on it, runs `just <targets>` (default: `ci`), streams the
# log to this terminal and to ~/Inbox/notes/, copies desktop/playwright-report
# back to ~/Inbox/misc/, then always stops the instance again. The exit code is
# the gate's own.
#
# This is the pre-push lane. Blacksmith stays the merge gate.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BOX_ENV="${REMOTE_CI_BOX_ENV:-${SCRIPT_DIR}/remote-ci/box.env}"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
INBOX_NOTES="${REMOTE_CI_NOTES_DIR:-$HOME/Inbox/notes}"
INBOX_MISC="${REMOTE_CI_MISC_DIR:-$HOME/Inbox/misc}"
REMOTE_REPO=/home/ci/buzz
SSH_WAIT_SECONDS="${REMOTE_CI_SSH_WAIT:-300}"
EXIT_MARKER='__REMOTE_CI_EXIT__'

KEEP=0
STATUS_ONLY=0
PUSH_LOCAL=0
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
  --status         Print the instance state and, when running, its uptime, then
                   exit. Does not start or stop anything.
  -h, --help       This text.

Environment:
  AWS_PROFILE            Profile used for the three EC2 calls. Default
                         buzz-ci-runner. If you instead export the scoped key
                         from ZS Vault, the vault entry must export exactly
                         AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY, and you
                         should set AWS_PROFILE= (empty) so the env wins.
  REMOTE_CI_BOX_ENV      Path to box.env (default scripts/zs/remote-ci/box.env).
  REMOTE_CI_SSH_WAIT     Seconds to wait for SSH after start (default 300).
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
[ -n "$BUZZ_CI_REGION" ] || die "BUZZ_CI_REGION missing from ${BOX_ENV}"
[ -n "$BUZZ_CI_KEY_PATH" ] || die "BUZZ_CI_KEY_PATH missing from ${BOX_ENV}"
[ -n "$BUZZ_CI_SSH_USER" ] || die "BUZZ_CI_SSH_USER missing from ${BOX_ENV}"
KEY_PATH="${BUZZ_CI_KEY_PATH/#\~/$HOME}"

AWS=(aws --region "$BUZZ_CI_REGION" --output text)
if [ -n "${AWS_PROFILE-}" ]; then
  AWS=(aws --profile "$AWS_PROFILE" --region "$BUZZ_CI_REGION" --output text)
elif [ -z "${AWS_ACCESS_KEY_ID-}" ]; then
  AWS=(aws --profile buzz-ci-runner --region "$BUZZ_CI_REGION" --output text)
fi
command -v aws >/dev/null 2>&1 || die "aws CLI not on PATH"

instance_state() {
  "${AWS[@]}" ec2 describe-instances --instance-ids "$BUZZ_CI_INSTANCE_ID" \
    --query 'Reservations[0].Instances[0].State.Name'
}
instance_launch_time() {
  "${AWS[@]}" ec2 describe-instances --instance-ids "$BUZZ_CI_INSTANCE_ID" \
    --query 'Reservations[0].Instances[0].LaunchTime'
}
instance_ip() {
  "${AWS[@]}" ec2 describe-instances --instance-ids "$BUZZ_CI_INSTANCE_ID" \
    --query 'Reservations[0].Instances[0].PublicIpAddress'
}

if [ "$STATUS_ONLY" = 1 ]; then
  state="$(instance_state)" || die "describe-instances failed"
  printf 'instance %s (%s): %s\n' "$BUZZ_CI_INSTANCE_ID" "$BUZZ_CI_REGION" "$state"
  if [ "$state" = running ]; then
    launched="$(instance_launch_time)"
    printf 'launched: %s\n' "$launched"
    started_epoch="$(date -j -f '%Y-%m-%dT%H:%M:%S' "${launched%%+*}" +%s 2>/dev/null \
      || date -d "$launched" +%s 2>/dev/null || echo '')"
    if [ -n "$started_epoch" ]; then
      printf 'uptime: %d minutes (box stops itself at 90)\n' \
        $(( ( $(date +%s) - started_epoch ) / 60 ))
    fi
    printf 'public ip: %s\n' "$(instance_ip)"
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

WORKDIR="$(mktemp -d)"
STOPPED=0
cleanup() {
  local rc=$?
  rm -rf "$WORKDIR"
  if [ "$KEEP" = 1 ]; then
    log "--keep: leaving ${BUZZ_CI_INSTANCE_ID} running"
  elif [ "$STOPPED" = 0 ]; then
    STOPPED=1
    log "stopping ${BUZZ_CI_INSTANCE_ID}"
    "${AWS[@]}" ec2 stop-instances --instance-ids "$BUZZ_CI_INSTANCE_ID" >/dev/null \
      || log "WARNING: stop-instances failed. Stop it by hand; the box also stops itself at 90 minutes of uptime."
  fi
  exit "$rc"
}
trap cleanup EXIT INT TERM

# ── start and wait ───────────────────────────────────────────────────────────
state="$(instance_state)" || die "describe-instances failed (is the AWS profile set up?)"
if [ "$state" != running ]; then
  log "starting ${BUZZ_CI_INSTANCE_ID} (was ${state})"
  "${AWS[@]}" ec2 start-instances --instance-ids "$BUZZ_CI_INSTANCE_ID" >/dev/null \
    || die "start-instances failed"
fi
log "waiting for the instance to run"
"${AWS[@]}" ec2 wait instance-running --instance-ids "$BUZZ_CI_INSTANCE_ID" \
  || die "instance did not reach running"
IP="$(instance_ip)"
[ -n "$IP" ] && [ "$IP" != None ] || die "instance has no public IP"

SSH_OPTS=(-o StrictHostKeyChecking=accept-new -o UserKnownHostsFile="$HOME/.ssh/known_hosts"
          -o ConnectTimeout=10 -o ServerAliveInterval=30 -i "$KEY_PATH")
REMOTE="${BUZZ_CI_SSH_USER}@${IP}"

log "waiting up to ${SSH_WAIT_SECONDS}s for ssh on ${REMOTE}"
deadline=$(( $(date +%s) + SSH_WAIT_SECONDS ))
until ssh "${SSH_OPTS[@]}" "$REMOTE" true 2>/dev/null; do
  [ "$(date +%s)" -lt "$deadline" ] || die "ssh did not come up within ${SSH_WAIT_SECONDS}s"
  sleep 5
done

# ── send the tree ────────────────────────────────────────────────────────────
BUNDLE_ARG=""
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
  scp "${SSH_OPTS[@]}" "$bundle" "${REMOTE}:/tmp/remote-ci.bundle" >/dev/null \
    || die "--push-local: scp of the bundle failed"
  BUNDLE_ARG=/tmp/remote-ci.bundle
fi

# ── remote runner ────────────────────────────────────────────────────────────
# Written locally and copied over, so no quoting of the just targets survives a
# round trip through two shells.
{
  printf '#!/bin/bash\nset -uo pipefail\n'
  printf '[ -f "$HOME/.buzz-ci-env" ] && . "$HOME/.buzz-ci-env"\n'
  printf 'cd %q || exit 90\n' "$REMOTE_REPO"
  printf 'git fetch origin --prune --tags || exit 91\n'
  if [ -n "$BUNDLE_ARG" ]; then
    printf 'git fetch %q %q || exit 92\n' "$BUNDLE_ARG" "$BRANCH"
    printf 'git checkout --detach FETCH_HEAD || exit 93\n'
  else
    printf 'git checkout --detach %q || exit 93\n' "origin/${BRANCH}"
  fi
  printf 'git --no-pager log -1 --oneline\n'
  printf 'rm -rf desktop/playwright-report desktop/playwright-report.json\n'
  printf '. ./bin/activate-hermit || exit 94\n'
  printf 'just'
  printf ' %q' "${TARGETS[@]}"
  printf '\nrc=$?\n'
  printf 'echo "%s=$rc"\n' "$EXIT_MARKER"
} > "${WORKDIR}/run.sh"
scp "${SSH_OPTS[@]}" "${WORKDIR}/run.sh" "${REMOTE}:/tmp/remote-ci-run.sh" >/dev/null \
  || die "scp of the runner failed"

log "running: just ${TARGETS[*]} on ${BRANCH}"
log "log: ${LOG_FILE}"
if [ "$(uname -s)" = Darwin ]; then
  script -q "$LOG_FILE" ssh -tt "${SSH_OPTS[@]}" "$REMOTE" 'bash /tmp/remote-ci-run.sh'
else
  script -q -e -c "ssh -tt ${SSH_OPTS[*]} ${REMOTE} 'bash /tmp/remote-ci-run.sh'" "$LOG_FILE"
fi

# `script` does not report the remote command's status portably, so the runner
# prints its own exit code as the last line and we read it back.
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
