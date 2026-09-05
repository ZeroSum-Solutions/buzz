#!/usr/bin/env bash
# zs fork: gate for the on-demand test box scripts.
#
# No AWS account is needed. Every AWS call goes through a stub on PATH that
# records its arguments and answers with canned output, so the whole provision
# and control flow is exercised offline.
#
#   scripts/zs/remote-ci/test-remote-ci.sh
set -uo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REMOTE_CI="${DIR}/../remote-ci.sh"
PROVISION="${DIR}/provision.sh"
BOOTSTRAP="${DIR}/bootstrap.sh"
SELF="${DIR}/test-remote-ci.sh"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
FAILURES=0

pass() { printf 'ok   %s\n' "$1"; }
fail() { printf 'FAIL %s\n     %s\n' "$1" "${2-}"; FAILURES=$((FAILURES + 1)); }

# ── a stub aws that records its arguments ────────────────────────────────────
mkdir -p "${WORK}/bin"
cat > "${WORK}/bin/aws" <<'STUB'
#!/usr/bin/env bash
# Records every invocation and answers with the smallest canned value the
# caller's --query asks for. Never touches the network.
printf '%s\n' "$*" >> "${AWS_STUB_LOG:?}"
query=""
prev=""
for arg in "$@"; do
  [ "$prev" = "--query" ] && query="$arg"
  prev="$arg"
done
case "$*" in
  *"sts get-caller-identity"*) echo "111122223333" ;;
  *"describe-instances"*)
    case "$query" in
      *State.Name*) echo "${AWS_STUB_STATE:-stopped}" ;;
      *LaunchTime*) echo "2026-09-05T12:00:00+00:00" ;;
      *PublicIpAddress*) echo "203.0.113.20" ;;
      *InstanceId*) echo "${AWS_STUB_INSTANCE:-None}" ;;
      *) echo "None" ;;
    esac
    ;;
  *"describe-vpcs"*) echo "vpc-0abc" ;;
  *"start-instances"*|*"stop-instances"*|*"wait "*) : ;;
  *) echo "None" ;;
esac
exit 0
STUB
chmod +x "${WORK}/bin/aws"

# ── 1. static analysis ───────────────────────────────────────────────────────
for f in "$PROVISION" "$BOOTSTRAP" "$REMOTE_CI" "$SELF"; do
  name="$(basename "$f")"
  if out="$(bash -n "$f" 2>&1)"; then pass "bash -n ${name}"; else fail "bash -n ${name}" "$out"; fi
  if command -v shellcheck >/dev/null 2>&1; then
    if out="$(shellcheck -S warning "$f" 2>&1)"; then
      pass "shellcheck ${name}"
    else
      fail "shellcheck ${name}" "$out"
    fi
  else
    printf 'skip shellcheck %s (not installed)\n' "$name"
  fi
done

# ── 2. provision.sh --dry-run makes no AWS call and prints the plan ──────────
export AWS_STUB_LOG="${WORK}/dryrun-aws.log"
: > "$AWS_STUB_LOG"
dry_out="$(PATH="${WORK}/bin:$PATH" AWS_PROFILE=stub-admin AWS_REGION=us-east-1 \
  BUZZ_CI_KEY_PATH="${WORK}/should-not-exist.pem" BUZZ_CI_BACKUP_DIR="${WORK}/backups" \
  "$PROVISION" --dry-run 2>&1)"
dry_rc=$?
if [ "$dry_rc" -eq 0 ]; then pass "provision.sh --dry-run exits 0"; else fail "provision.sh --dry-run exits 0" "rc=${dry_rc}"; fi
if [ ! -s "$AWS_STUB_LOG" ]; then
  pass "provision.sh --dry-run makes no aws call"
else
  fail "provision.sh --dry-run makes no aws call" "$(cat "$AWS_STUB_LOG")"
fi
for expect in \
  "ec2 create-security-group" \
  "ec2 authorize-security-group-ingress" \
  "ec2 create-key-pair" \
  "iam create-role" \
  "iam create-instance-profile" \
  "ssm get-parameter" \
  "ec2 run-instances" \
  "cloudwatch put-metric-alarm" \
  "iam put-user-policy" \
  "iam create-access-key" \
  "ec2 stop-instances"
do
  case "$dry_out" in
    *"$expect"*) pass "dry-run plans: ${expect}" ;;
    *) fail "dry-run plans: ${expect}" "not in the dry-run output" ;;
  esac
done
case "$dry_out" in
  *"c7a.8xlarge"*) pass "dry-run uses c7a.8xlarge" ;;
  *) fail "dry-run uses c7a.8xlarge" "instance type missing" ;;
esac
case "$dry_out" in
  *"VolumeSize=200"*) pass "dry-run uses a 200 GB root volume" ;;
  *) fail "dry-run uses a 200 GB root volume" "block device mapping missing" ;;
esac
if [ ! -e "${WORK}/should-not-exist.pem" ] && [ ! -e "${DIR}/box.env.dryrun" ]; then
  pass "provision.sh --dry-run writes no file"
else
  fail "provision.sh --dry-run writes no file" "a file appeared"
fi

# ── 3. remote-ci.sh --help ───────────────────────────────────────────────────
help_out="$("$REMOTE_CI" --help 2>&1)"; help_rc=$?
if [ "$help_rc" -eq 0 ]; then pass "remote-ci.sh --help exits 0"; else fail "remote-ci.sh --help exits 0" "rc=${help_rc}"; fi
for flag in --push-local --keep --status AWS_PROFILE REMOTE_CI_BOX_ENV; do
  case "$help_out" in
    *"$flag"*) pass "--help documents ${flag}" ;;
    *) fail "--help documents ${flag}" "missing" ;;
  esac
done

# ── 4. box.env parsing ───────────────────────────────────────────────────────
export AWS_STUB_LOG="${WORK}/status-aws.log"
: > "$AWS_STUB_LOG"
good="${WORK}/box.env"
cat > "$good" <<EOF
# fixture
BUZZ_CI_INSTANCE_ID=i-0123456789abcdef0
BUZZ_CI_REGION=us-east-1
BUZZ_CI_KEY_PATH=${WORK}/fake-key.pem
BUZZ_CI_SSH_USER=ci
EOF
touch "${WORK}/fake-key.pem"
status_out="$(PATH="${WORK}/bin:$PATH" AWS_PROFILE=stub-runner REMOTE_CI_BOX_ENV="$good" \
  "$REMOTE_CI" --status 2>&1)"; status_rc=$?
if [ "$status_rc" -eq 0 ]; then pass "--status with a valid box.env exits 0"; else fail "--status with a valid box.env exits 0" "rc=${status_rc}: ${status_out}"; fi
case "$status_out" in
  *"i-0123456789abcdef0"*stopped*) pass "--status reports the instance and state" ;;
  *) fail "--status reports the instance and state" "$status_out" ;;
esac
if grep -q 'describe-instances' "$AWS_STUB_LOG"; then
  pass "--status calls describe-instances"
else
  fail "--status calls describe-instances" "$(cat "$AWS_STUB_LOG")"
fi
if grep -qE 'start-instances|stop-instances' "$AWS_STUB_LOG"; then
  fail "--status starts or stops nothing" "$(cat "$AWS_STUB_LOG")"
else
  pass "--status starts or stops nothing"
fi

bad_key="${WORK}/box.env.badkey"
printf 'BUZZ_CI_INSTANCE_ID=i-0123456789abcdef0\nEVIL=$(touch %s/pwned)\n' "$WORK" > "$bad_key"
if out="$(PATH="${WORK}/bin:$PATH" REMOTE_CI_BOX_ENV="$bad_key" "$REMOTE_CI" --status 2>&1)"; then
  fail "box.env with an unknown key is rejected" "exited 0: ${out}"
else
  case "$out" in
    *"unexpected key"*) pass "box.env with an unknown key is rejected" ;;
    *) fail "box.env with an unknown key is rejected" "$out" ;;
  esac
fi
if [ -e "${WORK}/pwned" ]; then
  fail "box.env is parsed, never executed" "the fixture's command substitution ran"
else
  pass "box.env is parsed, never executed"
fi

bad_id="${WORK}/box.env.badid"
cat > "$bad_id" <<EOF
BUZZ_CI_INSTANCE_ID=not-an-instance
BUZZ_CI_REGION=us-east-1
BUZZ_CI_KEY_PATH=${WORK}/fake-key.pem
BUZZ_CI_SSH_USER=ci
EOF
if out="$(PATH="${WORK}/bin:$PATH" REMOTE_CI_BOX_ENV="$bad_id" "$REMOTE_CI" --status 2>&1)"; then
  fail "box.env with a bad instance id is rejected" "exited 0"
else
  case "$out" in
    *"not an instance id"*) pass "box.env with a bad instance id is rejected" ;;
    *) fail "box.env with a bad instance id is rejected" "$out" ;;
  esac
fi

if out="$(REMOTE_CI_BOX_ENV="${WORK}/absent.env" "$REMOTE_CI" zs/main 2>&1)"; then
  fail "a missing box.env is a clear error" "exited 0"
else
  case "$out" in
    *"Run scripts/zs/remote-ci/provision.sh first"*) pass "a missing box.env is a clear error" ;;
    *) fail "a missing box.env is a clear error" "$out" ;;
  esac
fi

# ── 5. bootstrap.sh guardrails ───────────────────────────────────────────────
if grep -q 'shutdown -h now' "$BOOTSTRAP" && grep -q '/etc/cron.d/buzz-ci-uptime-stop' "$BOOTSTRAP"; then
  pass "bootstrap installs the 90-minute uptime stop"
else
  fail "bootstrap installs the 90-minute uptime stop" "cron guard missing"
fi
if grep -q 'buzz-bootstrap.log' "$BOOTSTRAP"; then
  pass "bootstrap logs to /var/log/buzz-bootstrap.log"
else
  fail "bootstrap logs to /var/log/buzz-bootstrap.log" "log path missing"
fi

printf '\n%s\n' "-----"
if [ "$FAILURES" -eq 0 ]; then
  printf 'all checks passed\n'
  exit 0
fi
printf '%d check(s) failed\n' "$FAILURES"
exit 1
