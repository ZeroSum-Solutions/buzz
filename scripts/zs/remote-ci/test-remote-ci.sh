#!/usr/bin/env bash
# zs fork: gate for the on-demand test box scripts.
#
# No AWS account is needed. Every AWS call goes through a stub on PATH that
# records its arguments and answers with canned output, so the whole provision
# and control flow is exercised offline. Each guard has a check that fails when
# the guard is removed.
#
#   scripts/zs/remote-ci/test-remote-ci.sh
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REMOTE_CI="${DIR}/../remote-ci.sh"
PROVISION="${DIR}/provision.sh"
BOOTSTRAP="${DIR}/bootstrap.sh"
SELF="${DIR}/test-remote-ci.sh"

WORK="$(mktemp -d)"
[ -n "$WORK" ] && [ -d "$WORK" ] || { echo "mktemp -d failed" >&2; exit 1; }
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
#   AWS_STUB_FAIL=<substring>   fail any call whose argv contains it
#   AWS_STUB_INSTANCES=<ids>    what describe-instances reports for InstanceId
#   AWS_STUB_STATE=<state>      what describe-instances reports for State.Name
printf '%s\n' "$*" >> "${AWS_STUB_LOG:?}"
if [ -n "${AWS_STUB_FAIL:-}" ]; then
  case "$*" in
    *"$AWS_STUB_FAIL"*)
      echo "An error occurred (UnauthorizedOperation): stub refuses ${AWS_STUB_FAIL}" >&2
      exit 254
      ;;
  esac
fi
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
      *InstanceId*) echo "${AWS_STUB_INSTANCES-}" ;;
      *) echo "None" ;;
    esac
    ;;
  *"describe-vpcs"*) echo "vpc-0abc" ;;
  *"start-instances"*|*"stop-instances"*|*" wait "*) : ;;
  *) echo "None" ;;
esac
exit 0
STUB
chmod +x "${WORK}/bin/aws"

run_provision() { # run_provision <extra env assignments handled by caller> -- args...
  PATH="${WORK}/bin:$PATH" \
  AWS_PROFILE=stub-admin AWS_REGION=us-east-1 \
  BUZZ_CI_KEY_PATH="${WORK}/should-not-exist.pem" \
  BUZZ_CI_BACKUP_DIR="${WORK}/backups" \
    "$PROVISION" "$@"
}

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
dry_rc=0
dry_out="$(run_provision --dry-run 2>&1)" || dry_rc=$?
if [ "$dry_rc" -eq 0 ]; then pass "provision.sh --dry-run exits 0"; else fail "provision.sh --dry-run exits 0" "rc=${dry_rc}: ${dry_out}"; fi
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
for expect in "c7a.8xlarge" "VolumeSize=200" "--client-token" "zs:owner"; do
  case "$dry_out" in
    *"$expect"*) pass "dry-run plan includes ${expect}" ;;
    *) fail "dry-run plan includes ${expect}" "missing" ;;
  esac
done
if [ ! -e "${WORK}/should-not-exist.pem" ]; then
  pass "provision.sh --dry-run writes no file"
else
  fail "provision.sh --dry-run writes no file" "a file appeared"
fi

# ── 3. provision.sh fails closed ─────────────────────────────────────────────
# Two owner-tagged instances: EC2 tags are not unique, so the script must not
# pick one arbitrarily.
export AWS_STUB_LOG="${WORK}/multi-aws.log"
: > "$AWS_STUB_LOG"
multi_rc=0
multi_out="$(AWS_STUB_INSTANCES="i-0aaaaaaaaaaaaaaaa	i-0bbbbbbbbbbbbbbbb" \
  run_provision 2>&1)" || multi_rc=$?
if [ "$multi_rc" -ne 0 ]; then pass "two tagged instances abort provisioning"; else fail "two tagged instances abort provisioning" "exited 0"; fi
case "$multi_out" in
  *"found 2 instances"*) pass "the abort names the collision" ;;
  *) fail "the abort names the collision" "$multi_out" ;;
esac
if grep -q 'run-instances' "$AWS_STUB_LOG"; then
  fail "no third instance is launched on collision" "$(cat "$AWS_STUB_LOG")"
else
  pass "no third instance is launched on collision"
fi

# A failing describe must never be read as "no instance exists".
export AWS_STUB_LOG="${WORK}/deny-aws.log"
: > "$AWS_STUB_LOG"
deny_rc=0
deny_out="$(AWS_STUB_FAIL="describe-instances" run_provision 2>&1)" || deny_rc=$?
if [ "$deny_rc" -ne 0 ]; then pass "a denied describe-instances aborts provisioning"; else fail "a denied describe-instances aborts provisioning" "exited 0"; fi
case "$deny_out" in
  *"refusing to treat that as"*) pass "the abort says it will not assume 'does not exist'" ;;
  *) fail "the abort says it will not assume 'does not exist'" "$deny_out" ;;
esac
if grep -q 'run-instances' "$AWS_STUB_LOG"; then
  fail "a denied describe launches nothing" "$(cat "$AWS_STUB_LOG")"
else
  pass "a denied describe launches nothing"
fi

# ── 4. remote-ci.sh --help ───────────────────────────────────────────────────
help_out="$("$REMOTE_CI" --help 2>&1)"; help_rc=$?
if [ "$help_rc" -eq 0 ]; then pass "remote-ci.sh --help exits 0"; else fail "remote-ci.sh --help exits 0" "rc=${help_rc}"; fi
for flag in --push-local --keep --join --status AWS_PROFILE REMOTE_CI_BOX_ENV; do
  case "$help_out" in
    *"$flag"*) pass "--help documents ${flag}" ;;
    *) fail "--help documents ${flag}" "missing" ;;
  esac
done

# ── 5. box.env parsing ───────────────────────────────────────────────────────
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
status_rc=0
status_out="$(PATH="${WORK}/bin:$PATH" AWS_PROFILE=stub-runner REMOTE_CI_BOX_ENV="$good" \
  "$REMOTE_CI" --status 2>&1)" || status_rc=$?
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
bk_rc=0
bk_out="$(PATH="${WORK}/bin:$PATH" REMOTE_CI_BOX_ENV="$bad_key" "$REMOTE_CI" --status 2>&1)" || bk_rc=$?
if [ "$bk_rc" -eq 0 ]; then
  fail "box.env with an unknown key is rejected" "exited 0: ${bk_out}"
else
  case "$bk_out" in
    *"unexpected key"*) pass "box.env with an unknown key is rejected" ;;
    *) fail "box.env with an unknown key is rejected" "$bk_out" ;;
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
bi_rc=0
bi_out="$(PATH="${WORK}/bin:$PATH" REMOTE_CI_BOX_ENV="$bad_id" "$REMOTE_CI" --status 2>&1)" || bi_rc=$?
if [ "$bi_rc" -eq 0 ]; then
  fail "box.env with a bad instance id is rejected" "exited 0"
else
  case "$bi_out" in
    *"not an instance id"*) pass "box.env with a bad instance id is rejected" ;;
    *) fail "box.env with a bad instance id is rejected" "$bi_out" ;;
  esac
fi

miss_rc=0
miss_out="$(REMOTE_CI_BOX_ENV="${WORK}/absent.env" "$REMOTE_CI" zs/main 2>&1)" || miss_rc=$?
if [ "$miss_rc" -eq 0 ]; then
  fail "a missing box.env is a clear error" "exited 0"
else
  case "$miss_out" in
    *"Run scripts/zs/remote-ci/provision.sh first"*) pass "a missing box.env is a clear error" ;;
    *) fail "a missing box.env is a clear error" "$miss_out" ;;
  esac
fi

# ── 6. concurrency guard ─────────────────────────────────────────────────────
export AWS_STUB_LOG="${WORK}/busy-aws.log"
: > "$AWS_STUB_LOG"
busy_rc=0
busy_out="$(PATH="${WORK}/bin:$PATH" AWS_PROFILE=stub-runner REMOTE_CI_BOX_ENV="$good" \
  AWS_STUB_STATE=running REMOTE_CI_NOTES_DIR="${WORK}/notes" \
  REMOTE_CI_MISC_DIR="${WORK}/misc" "$REMOTE_CI" zs/main 2>&1)" || busy_rc=$?
if [ "$busy_rc" -ne 0 ]; then pass "a running box refuses a second run"; else fail "a running box refuses a second run" "exited 0"; fi
case "$busy_out" in
  *"already running"*"--join"*) pass "the refusal names --join" ;;
  *) fail "the refusal names --join" "$busy_out" ;;
esac
if grep -qE 'start-instances|stop-instances' "$AWS_STUB_LOG"; then
  fail "the refusal starts or stops nothing" "$(cat "$AWS_STUB_LOG")"
else
  pass "the refusal starts or stops nothing"
fi

# ── 7. the guards that live in the scripts' text ─────────────────────────────
check_contains() { # check_contains <label> <file> <needle>
  if grep -qF -- "$3" "$2"; then pass "$1"; else fail "$1" "missing: $3"; fi
}
check_absent() { # check_absent <label> <file> <needle>
  if grep -qF -- "$3" "$2"; then fail "$1" "still present: $3"; else pass "$1"; fi
}
check_contains "bootstrap installs the 90-minute uptime stop" "$BOOTSTRAP" \
  "/etc/cron.d/buzz-ci-uptime-stop"
check_contains "bootstrap fails when cron is not active" "$BOOTSTRAP" \
  "systemctl is-active --quiet cron"
check_contains "bootstrap logs to /var/log/buzz-bootstrap.log" "$BOOTSTRAP" \
  "buzz-bootstrap.log"
check_absent "bootstrap grants the ci user no blanket sudo" "$BOOTSTRAP" \
  "NOPASSWD: ALL"
check_contains "bootstrap removes any earlier sudo grant" "$BOOTSTRAP" \
  "rm -f /etc/sudoers.d/buzz-ci"
check_contains "bootstrap writes its marker only at the end" "$BOOTSTRAP" \
  'date -u +%FT%TZ > "$MARKER"'
check_contains "provision arms the stop trap before launching" "$PROVISION" \
  "trap provision_trap EXIT INT TERM"
check_contains "provision fails when cloud-init never finishes" "$PROVISION" \
  "cloud-init did not finish within"
check_contains "provision repairs an instance with no marker" "$PROVISION" \
  "re-running bootstrap.sh over ssh"
check_contains "provision reads the admin key from the vault env" "$PROVISION" \
  "AWS_ADMIN_ACCESS_KEY_ID"
check_contains "provision prints the two zsvault add commands" "$PROVISION" \
  "zsvault add aws_ci_runner_secret_access_key"
check_contains "remote-ci installs dependencies after checkout" "$REMOTE_CI" \
  "just desktop-install-ci"
check_contains "remote-ci installs the branch's Playwright browser" "$REMOTE_CI" \
  "playwright install chromium"
check_contains "remote-ci takes an exclusive lock on the box" "$REMOTE_CI" \
  "flock -n 9"
check_contains "remote-ci waits out a stopping box" "$REMOTE_CI" \
  "waiting up to \${STOPPING_WAIT_SECONDS}s"
check_contains "remote-ci quotes the Linux script(1) command" "$REMOTE_CI" \
  'linux_cmd="$(printf'

printf '\n%s\n' "-----"
if [ "$FAILURES" -eq 0 ]; then
  printf 'all checks passed\n'
  exit 0
fi
printf '%d check(s) failed\n' "$FAILURES"
exit 1
