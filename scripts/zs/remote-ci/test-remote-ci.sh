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
printf 'ENV_KEY=%s\n' "${AWS_ACCESS_KEY_ID:-}" >> "${AWS_STUB_ENV_LOG:-/dev/null}"
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
owner="${AWS_STUB_OWNER:-buzz-remote-ci}"
case "$*" in
  *"sts get-caller-identity"*) echo "111122223333" ;;
  *"describe-instances"*"client-token"*)
    echo "${AWS_STUB_REDISCOVERED:-i-0recovered00000000}"
    ;;
  *"run-instances"*) echo "${AWS_STUB_RUN_ID-i-0launched000000000}" ;;
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
  *"describe-security-groups"*)
    case "$query" in
      *Tags*) echo "$owner" ;;
      *GroupId*) echo "${AWS_STUB_SG:-sg-0abc}" ;;
      *) echo "None" ;;
    esac
    ;;
  *"describe-security-group-rules"*) echo "" ;;
  *"describe-key-pairs"*) echo "None" ;;
  *"list-role-tags"*|*"list-user-tags"*|*"list-instance-profile-tags"*) echo "$owner" ;;
  *"list-attached-role-policies"*|*"list-attached-user-policies"*) echo "" ;;
  *"list-groups-for-user"*) echo "" ;;
  *"list-role-policies"*) echo "self-stop" ;;
  *"list-user-policies"*) echo "buzz-ci-box-control" ;;
  *"list-instance-profiles-for-role"*) echo "buzz-ci-box" ;;
  *"list-access-keys"*) echo "${AWS_STUB_ACCESS_KEYS-}" ;;
  *"create-access-key"*) echo "AKIASTUBKEYID	stub-secret-value" ;;
  *"iam get-role"*)
    case "$query" in
      *"length("*) echo "1" ;;
      *Principal.Service*) echo "ec2.amazonaws.com" ;;
      *Statement\[0\].Action*) echo "sts:AssumeRole" ;;
      *Statement\[0\].Effect*) echo "Allow" ;;
      *Principal.AWS*) printf 'None\tNone\tNone\n' ;;
      *) echo "arn:aws:iam::111122223333:role/buzz-ci-box-self-stop" ;;
    esac
    ;;
  *"iam get-instance-profile"*) echo "buzz-ci-box-self-stop" ;;
  *"iam get-user"*) echo "arn:aws:iam::111122223333:user/buzz-ci-runner" ;;
  *"start-instances"*|*"stop-instances"*|*" wait "*) : ;;
  *) echo "None" ;;
esac
exit 0
STUB
chmod +x "${WORK}/bin/aws"

run_provision() {
  PATH="${WORK}/bin:$PATH" \
  AWS_PROFILE=stub-admin AWS_REGION=us-west-2 \
  BUZZ_CI_KEY_PATH="${WORK}/should-not-exist.pem" \
  BUZZ_CI_STOP_BACKOFF=0 \
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
  "ec2 stop-instances" \
  "zsvault add aws_buzz_ci_runner_secret"
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

# Adopting an existing, owner-tagged instance: no launch, no ssh, so this run
# reaches the security-group and IAM ownership checks and the runner key.
adopt_provision() { # adopt_provision <log> [args...]
  local log="$1"; shift
  # The stub reports no key pair in AWS, so provision.sh refuses to overwrite a
  # PEM left by a previous check. Each run starts without one.
  rm -f "${WORK}/adopt.pem"
  AWS_STUB_LOG="$log" AWS_STUB_INSTANCES="i-0123456789abcdef0" \
  PATH="${EXTRA_PATH:-}${WORK}/bin:$PATH" AWS_PROFILE=stub-admin AWS_REGION=us-west-2 \
  BUZZ_CI_KEY_PATH="${WORK}/adopt.pem" BUZZ_CI_STOP_BACKOFF=0 \
    "$PROVISION" "$@"
}

# ssh/scp stubs that report a healthy, already-bootstrapped box. They let the
# real verify_bootstrap path run offline instead of a test-only shortcut.
mkdir -p "${WORK}/sshbin"
printf '#!/usr/bin/env bash\nexit 0\n' > "${WORK}/sshbin/ssh"
printf '#!/usr/bin/env bash\nexit 0\n' > "${WORK}/sshbin/scp"
chmod +x "${WORK}/sshbin/ssh" "${WORK}/sshbin/scp"

adopt_rc=0
adopt_out="$(adopt_provision "${WORK}/adopt-aws.log" --no-verify 2>&1)" || adopt_rc=$?
case "$adopt_out" in
  *"security group buzz-ci-box exists"*) pass "an owner-tagged security group is adopted" ;;
  *) fail "an owner-tagged security group is adopted" "$adopt_out (rc=${adopt_rc})" ;;
esac

# The same group without our owner tag belongs to someone else.
sg_rc=0
sg_out="$(export AWS_STUB_OWNER='not-ours'; adopt_provision "${WORK}/sg-aws.log" --no-verify 2>&1)" || sg_rc=$?
unset AWS_STUB_OWNER   # a prefix assignment on a function call would persist
if [ "$sg_rc" -ne 0 ]; then
  pass "an untagged security group aborts provisioning"
else
  fail "an untagged security group aborts provisioning" "exited 0"
fi
case "$sg_out" in
  *"security group buzz-ci-box (sg-0abc) exists but is not tagged"*)
    pass "the abort names the untagged group" ;;
  *) fail "the abort names the untagged group" "$sg_out" ;;
esac

# ── 4. the runner key goes to the vault, or it is deleted again ─────────────
cat > "${WORK}/bin/zsvault" <<'VAULT'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "${ZSVAULT_STUB_LOG:?}"
cat >/dev/null
# ZSVAULT_STUB_KILL simulates an interrupt arriving between create-access-key
# and the vault write landing: the caller is signalled, not returned to.
if [ -n "${ZSVAULT_STUB_KILL:-}" ]; then
  kill -TERM "$PPID" 2>/dev/null
  sleep 5
fi
exit "${ZSVAULT_STUB_RC:-0}"
VAULT
chmod +x "${WORK}/bin/zsvault"
export ZSVAULT_STUB_LOG="${WORK}/zsvault.log"
: > "$ZSVAULT_STUB_LOG"

# --no-verify leaves the box unverified, so no key may be created at all.
if grep -q 'create-access-key' "${WORK}/adopt-aws.log"; then
  fail "no runner key is created for an unverified box" "$(cat "${WORK}/adopt-aws.log")"
else
  pass "no runner key is created for an unverified box"
fi
case "$adopt_out" in
  *"the box was not verified, so no runner key was created"*)
    pass "an unverified box says why it has no runner key" ;;
  *) fail "an unverified box says why it has no runner key" "$adopt_out" ;;
esac

# A verified box creates the key; a failing vault write must delete it again.
: > "${WORK}/vault-aws.log"
vault_rc=0
vault_out="$(export ZSVAULT_STUB_RC=1 EXTRA_PATH="${WORK}/sshbin:"
  adopt_provision "${WORK}/vault-aws.log" 2>&1)" || vault_rc=$?
unset ZSVAULT_STUB_RC EXTRA_PATH
if [ "$vault_rc" -ne 0 ]; then
  pass "a failed vault write fails provisioning"
else
  fail "a failed vault write fails provisioning" "exited 0: ${vault_out}"
fi
if grep -q 'iam delete-access-key' "${WORK}/vault-aws.log"; then
  pass "a failed vault write deletes the IAM access key again"
else
  fail "a failed vault write deletes the IAM access key again" "$(cat "${WORK}/vault-aws.log")"
fi
if grep -q 'aws_buzz_ci_runner_secret' "$ZSVAULT_STUB_LOG" \
   || grep -q 'aws_buzz_ci_runner_key_id' "$ZSVAULT_STUB_LOG"; then
  pass "the runner key is handed to zsvault"
else
  fail "the runner key is handed to zsvault" "$(cat "$ZSVAULT_STUB_LOG")"
fi
if [ -z "$(find "$WORK" -name '*buzz-ci-runner*' 2>/dev/null)" ]; then
  pass "the runner secret is never written to a file"
else
  fail "the runner secret is never written to a file" "$(find "$WORK" -name '*buzz-ci-runner*')"
fi

# An interrupt between create-access-key and the vault write must still revoke
# the key: the stub signals provision.sh instead of returning.
: > "${WORK}/kill-aws.log"
kill_rc=0
kill_out="$(export ZSVAULT_STUB_KILL=1 EXTRA_PATH="${WORK}/sshbin:"
  adopt_provision "${WORK}/kill-aws.log" 2>&1)" || kill_rc=$?
unset ZSVAULT_STUB_KILL EXTRA_PATH
if [ "$kill_rc" -ne 0 ]; then
  pass "an interrupt during the vault handoff fails provisioning"
else
  fail "an interrupt during the vault handoff fails provisioning" "exited 0: ${kill_out}"
fi
if grep -q 'iam delete-access-key' "${WORK}/kill-aws.log"; then
  pass "an interrupt during the vault handoff revokes the pending key"
else
  fail "an interrupt during the vault handoff revokes the pending key" "$(cat "${WORK}/kill-aws.log")"
fi

# `bash -x` must never print either credential.
: > "${WORK}/xtrace-aws.log"
xtrace_err="${WORK}/xtrace.err"
AWS_STUB_LOG="${WORK}/xtrace-aws.log" \
ZS_AWS_BUZZ_CI_ADMIN_KEY_ID=AKIAFAKEADMINKEYID \
ZS_AWS_BUZZ_CI_ADMIN_SECRET=s3cr3t-admin-value-never-print \
PATH="${WORK}/bin:$PATH" AWS_REGION=us-west-2 \
BUZZ_CI_KEY_PATH="${WORK}/xtrace.pem" \
  bash -x "$PROVISION" --dry-run >/dev/null 2>"$xtrace_err" || true
if grep -q 's3cr3t-admin-value-never-print' "$xtrace_err"; then
  fail "bash -x never prints the admin secret" "the secret is in the trace"
else
  pass "bash -x never prints the admin secret"
fi
if grep -q 'stub-secret-value' "$xtrace_err"; then
  fail "bash -x never prints the runner secret" "the secret is in the trace"
else
  pass "bash -x never prints the runner secret"
fi

# A launch that returns no id is rediscovered, never recorded as a placeholder.
: > "${WORK}/relaunch-aws.log"
relaunch_rc=0
relaunch_out="$(export AWS_STUB_RUN_ID='None'
  AWS_STUB_LOG="${WORK}/relaunch-aws.log" AWS_STUB_INSTANCES="" \
  PATH="${WORK}/bin:$PATH" AWS_PROFILE=stub-admin AWS_REGION=us-west-2 \
  BUZZ_CI_KEY_PATH="${WORK}/relaunch.pem" BUZZ_CI_STOP_BACKOFF=0 \
    "$PROVISION" --no-wait-bootstrap 2>&1)" || relaunch_rc=$?
case "$relaunch_out" in
  *"recovered the launched instance id: i-0recovered00000000"*)
    pass "an id-less launch is rediscovered by client token" ;;
  *) fail "an id-less launch is rediscovered by client token" "$relaunch_out (rc=${relaunch_rc})" ;;
esac
if grep -q 'i-0dryrun' "${WORK}/relaunch-aws.log"; then
  fail "no placeholder instance id is ever used" "$(cat "${WORK}/relaunch-aws.log")"
else
  pass "no placeholder instance id is ever used"
fi

# ── 5. remote-ci.sh --help ───────────────────────────────────────────────────
help_out="$("$REMOTE_CI" --help 2>&1)"; help_rc=$?
if [ "$help_rc" -eq 0 ]; then pass "remote-ci.sh --help exits 0"; else fail "remote-ci.sh --help exits 0" "rc=${help_rc}"; fi
for flag in --push-local --keep --join --status REMOTE_CI_LEASE REMOTE_CI_GATE_TIMEOUT REMOTE_CI_LOG_CAP; do
  case "$help_out" in
    *"$flag"*) pass "--help documents ${flag}" ;;
    *) fail "--help documents ${flag}" "missing" ;;
  esac
done

# ── 5b. the vault runner key reaches aws through the child environment only ──
export AWS_STUB_LOG="${WORK}/vault-runner-aws.log"
export AWS_STUB_ENV_LOG="${WORK}/vault-runner-env.log"
: > "$AWS_STUB_LOG"; : > "$AWS_STUB_ENV_LOG"
vr_env="${WORK}/vault-runner-box.env"
cat > "$vr_env" <<EOF
BUZZ_CI_INSTANCE_ID=i-0123456789abcdef0
BUZZ_CI_REGION=us-west-2
BUZZ_CI_KEY_PATH=${WORK}/fake-key.pem
BUZZ_CI_SSH_USER=ci
EOF
touch "${WORK}/fake-key.pem"
vr_out="$(env -u AWS_PROFILE ZS_AWS_BUZZ_CI_RUNNER_KEY_ID=AKIAFAKERUNNERKEYID \
  ZS_AWS_BUZZ_CI_RUNNER_SECRET=runner-value-never-print \
  PATH="${WORK}/bin:$PATH" REMOTE_CI_BOX_ENV="$vr_env" REMOTE_CI_LEASE="${WORK}/vr.lock" \
  REMOTE_CI_NOTES_DIR="${WORK}/notes" REMOTE_CI_MISC_DIR="${WORK}/misc" \
  "$REMOTE_CI" --status 2>&1)" || true
if grep -q 'ENV_KEY=AKIAFAKERUNNERKEYID' "$AWS_STUB_ENV_LOG"; then
  pass "remote-ci uses the vault runner key when AWS_PROFILE is unset"
else
  fail "remote-ci uses the vault runner key when AWS_PROFILE is unset" "$(cat "$AWS_STUB_ENV_LOG")"
fi
if grep -q -- '--profile' "$AWS_STUB_LOG"; then
  fail "remote-ci passes no --profile alongside the vault key" "$(cat "$AWS_STUB_LOG")"
else
  pass "remote-ci passes no --profile alongside the vault key"
fi
if grep -q 'runner-value-never-print' "$AWS_STUB_LOG" || printf '%s' "$vr_out" | grep -q 'runner-value-never-print'; then
  fail "the runner key value never reaches argv or the output" "$vr_out"
else
  pass "the runner key value never reaches argv or the output"
fi
unset AWS_STUB_ENV_LOG

# ── 6. box.env parsing ───────────────────────────────────────────────────────
export AWS_STUB_LOG="${WORK}/status-aws.log"
: > "$AWS_STUB_LOG"
good="${WORK}/box.env"
cat > "$good" <<EOF
# fixture
BUZZ_CI_INSTANCE_ID=i-0123456789abcdef0
BUZZ_CI_REGION=us-west-2
BUZZ_CI_KEY_PATH=${WORK}/fake-key.pem
BUZZ_CI_SSH_USER=ci
EOF
touch "${WORK}/fake-key.pem"
LEASE="${WORK}/lease.lock"
run_remote_ci() {
  PATH="${WORK}/bin:$PATH" AWS_PROFILE=stub-runner REMOTE_CI_BOX_ENV="$good" \
  REMOTE_CI_LEASE="$LEASE" REMOTE_CI_NOTES_DIR="${WORK}/notes" \
  REMOTE_CI_MISC_DIR="${WORK}/misc" REMOTE_CI_STOP_BACKOFF=0 \
  REMOTE_CI_SSH_WAIT="${REMOTE_CI_SSH_WAIT:-1}" \
    "$REMOTE_CI" "$@"
}
status_rc=0
status_out="$(run_remote_ci --status 2>&1)" || status_rc=$?
if [ "$status_rc" -eq 0 ]; then pass "--status with a valid box.env exits 0"; else fail "--status with a valid box.env exits 0" "rc=${status_rc}: ${status_out}"; fi
case "$status_out" in
  *"i-0123456789abcdef0"*stopped*) pass "--status reports the instance and state" ;;
  *) fail "--status reports the instance and state" "$status_out" ;;
esac
if grep -qE 'start-instances|stop-instances' "$AWS_STUB_LOG"; then
  fail "--status starts or stops nothing" "$(cat "$AWS_STUB_LOG")"
else
  pass "--status starts or stops nothing"
fi

bad_key="${WORK}/box.env.badkey"
printf 'BUZZ_CI_INSTANCE_ID=i-0123456789abcdef0\nEVIL=$(touch %s/pwned)\n' "$WORK" > "$bad_key"
bk_rc=0
bk_out="$(PATH="${WORK}/bin:$PATH" REMOTE_CI_BOX_ENV="$bad_key" REMOTE_CI_LEASE="$LEASE" \
  "$REMOTE_CI" --status 2>&1)" || bk_rc=$?
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
BUZZ_CI_REGION=us-west-2
BUZZ_CI_KEY_PATH=${WORK}/fake-key.pem
BUZZ_CI_SSH_USER=ci
EOF
bi_rc=0
bi_out="$(PATH="${WORK}/bin:$PATH" REMOTE_CI_BOX_ENV="$bad_id" REMOTE_CI_LEASE="$LEASE" \
  "$REMOTE_CI" --status 2>&1)" || bi_rc=$?
if [ "$bi_rc" -eq 0 ]; then
  fail "box.env with a bad instance id is rejected" "exited 0"
else
  case "$bi_out" in
    *"not an instance id"*) pass "box.env with a bad instance id is rejected" ;;
    *) fail "box.env with a bad instance id is rejected" "$bi_out" ;;
  esac
fi

# A limit that can be set to 0 is not a limit.
for pair in "REMOTE_CI_GATE_TIMEOUT=0" "REMOTE_CI_LOG_CAP=0" "REMOTE_CI_REPORT_CAP_KB=0"; do
  name="${pair%%=*}"
  lim_rc=0
  lim_out="$(export "${pair?}"; run_remote_ci zs/main 2>&1)" || lim_rc=$?
  unset "$name"
  if [ "$lim_rc" -ne 0 ]; then
    pass "${name}=0 is rejected"
  else
    fail "${name}=0 is rejected" "exited 0"
  fi
  case "$lim_out" in
    *"outside"*) pass "${name}=0 says why it is rejected" ;;
    *) fail "${name}=0 says why it is rejected" "$lim_out" ;;
  esac
done
bad_rc=0
bad_out="$(export REMOTE_CI_GATE_TIMEOUT=abc; run_remote_ci zs/main 2>&1)" || bad_rc=$?
unset REMOTE_CI_GATE_TIMEOUT
if [ "$bad_rc" -ne 0 ]; then pass "a non-numeric override is rejected"; else fail "a non-numeric override is rejected" "exited 0"; fi
case "$bad_out" in
  *"must be a whole number"*) pass "a non-numeric override says why" ;;
  *) fail "a non-numeric override says why" "$bad_out" ;;
esac

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

# ── 7. the local lease ───────────────────────────────────────────────────────
# Hold the lease from another process, exactly as a concurrent run would.
printf 'pid=99999\nbranch=other-branch\nstarted=2026-09-05T00:00:00Z\n' > "${LEASE}.holder"
python3 - "$LEASE" "${WORK}/held" <<'HOLD' &
import fcntl, os, sys, time
path, flag = sys.argv[1], sys.argv[2]
fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o644)
fcntl.flock(fd, fcntl.LOCK_EX)
open(flag, "w").close()
time.sleep(120)
HOLD
HOLD_PID=$!
tries=0
until [ -f "${WORK}/held" ] || [ "$tries" -ge 100 ]; do sleep 0.1; tries=$((tries + 1)); done
export AWS_STUB_LOG="${WORK}/lease-aws.log"
: > "$AWS_STUB_LOG"
lease_rc=0
lease_out="$(run_remote_ci zs/main 2>&1)" || lease_rc=$?
kill "$HOLD_PID" 2>/dev/null || true
wait "$HOLD_PID" 2>/dev/null || true
rm -f "${LEASE}.holder"
if [ "$lease_rc" -ne 0 ]; then pass "a held lease refuses the run"; else fail "a held lease refuses the run" "exited 0"; fi
case "$lease_out" in
  *"another remote-ci run on this machine holds the box"*"other-branch"*)
    pass "the refusal names the lease holder" ;;
  *) fail "the refusal names the lease holder" "$lease_out" ;;
esac
if [ -s "$AWS_STUB_LOG" ]; then
  fail "a refused run makes no aws call" "$(cat "$AWS_STUB_LOG")"
else
  pass "a refused run makes no aws call"
fi

# ── 8. a box that is running without our lease ───────────────────────────────
export AWS_STUB_LOG="${WORK}/busy-aws.log"
: > "$AWS_STUB_LOG"
busy_rc=0
busy_out="$(AWS_STUB_STATE=running REMOTE_CI_SSH_WAIT=1 run_remote_ci zs/main 2>&1)" || busy_rc=$?
if [ "$busy_rc" -ne 0 ]; then pass "a running box refuses a run without --join"; else fail "a running box refuses a run without --join" "exited 0"; fi
case "$busy_out" in
  *"already running"*"--join"*) pass "the refusal names --join" ;;
  *) fail "the refusal names --join" "$busy_out" ;;
esac
if grep -qE 'start-instances|stop-instances' "$AWS_STUB_LOG"; then
  fail "the refusal starts or stops nothing" "$(cat "$AWS_STUB_LOG")"
else
  pass "the refusal starts or stops nothing"
fi

# ── 9. a failed stop is loud and fails the run ───────────────────────────────
# The box starts, ssh never comes up, and every stop attempt is refused.
export AWS_STUB_LOG="${WORK}/stopfail-aws.log"
: > "$AWS_STUB_LOG"
stop_rc=0
stop_out="$(AWS_STUB_FAIL="stop-instances" REMOTE_CI_SSH_WAIT=1 \
  run_remote_ci zs/main 2>&1)" || stop_rc=$?
if [ "$stop_rc" -ne 0 ]; then pass "a failed stop fails the run"; else fail "a failed stop fails the run" "exited 0"; fi
case "$stop_out" in
  *"COULD NOT STOP i-0123456789abcdef0"*)
    pass "the failed stop names the instance loudly" ;;
  *) fail "the failed stop names the instance loudly" "$stop_out" ;;
esac
attempts="$(grep -c 'stop-instances' "$AWS_STUB_LOG" || true)"
if [ "${attempts:-0}" -ge 5 ]; then
  pass "the stop is retried 5 times"
else
  fail "the stop is retried 5 times" "saw ${attempts} attempts"
fi

# ── 10. the runner script resets the shared checkout ─────────────────────────
runner="${WORK}/runner.sh"
"$REMOTE_CI" --print-runner zs/main desktop-test > "$runner" 2>/dev/null \
  || fail "--print-runner emits the remote script" "non-zero exit"
for needle in 'git reset --hard' 'git clean -f -d' 'git status --porcelain' \
              'just desktop-install-ci' 'playwright install chromium' \
              'flock -n 9' 'timeout '; do
  if grep -qF -- "$needle" "$runner"; then
    pass "the remote runner contains: ${needle}"
  else
    fail "the remote runner contains: ${needle}" "missing"
  fi
done

# And it aborts rather than gating a tree that is still dirty. Everything the
# runner calls is stubbed, and `git status --porcelain` reports a modification.
FAKE="${WORK}/fake-box"
mkdir -p "${FAKE}/bin" "${FAKE}/repo/bin" "${FAKE}/repo/desktop"
cat > "${FAKE}/bin/git" <<'GITSTUB'
#!/usr/bin/env bash
[ "$1" = "status" ] && { printf ' M desktop/src/App.tsx\n'; exit 0; }
exit 0
GITSTUB
cat > "${FAKE}/bin/flock" <<'FLOCKSTUB'
#!/usr/bin/env bash
exit 0
FLOCKSTUB
for tool in just pnpm timeout hostname; do
  printf '#!/usr/bin/env bash\nexit 0\n' > "${FAKE}/bin/${tool}"
done
chmod +x "${FAKE}"/bin/*
printf 'true\n' > "${FAKE}/repo/bin/activate-hermit"
dirty_rc=0
dirty_out="$(cd "${FAKE}/repo" && PATH="${FAKE}/bin:/usr/bin:/bin" \
  REMOTE_CI_REPO_DIR="${FAKE}/repo" REMOTE_CI_LOCK_FILE="${FAKE}/lock" \
  bash "$runner" 2>&1)" || dirty_rc=$?
if [ "$dirty_rc" -ne 0 ]; then
  pass "the remote runner aborts on a dirty tree"
else
  fail "the remote runner aborts on a dirty tree" "exited 0: ${dirty_out}"
fi
case "$dirty_out" in
  *"still dirty after reset"*) pass "the dirty-tree abort says why" ;;
  *) fail "the dirty-tree abort says why" "$dirty_out" ;;
esac
case "$dirty_out" in
  *"__REMOTE_CI_EXIT__=97"*) pass "the dirty-tree abort reports its exit code" ;;
  *) fail "the dirty-tree abort reports its exit code" "$dirty_out" ;;
esac

# ── 11. the guards that live in the scripts' text ────────────────────────────
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
check_absent "bootstrap grants the ci user no blanket root" "$BOOTSTRAP" \
  "NOPASSWD: ALL"
check_contains "bootstrap writes its marker only at the end" "$BOOTSTRAP" \
  'date -u +%FT%TZ > "$MARKER"'
check_contains "provision arms the stop trap before launching" "$PROVISION" \
  "trap provision_trap EXIT"
check_contains "provision treats a signal as a failure" "$PROVISION" \
  "trap on_signal INT TERM"
check_contains "provision rediscovers a lost instance by client token" "$PROVISION" \
  "Name=client-token,Values="
check_contains "provision keeps the instance recorded until EC2 confirms" "$PROVISION" \
  "RUNNING_INSTANCE=\"\"   # cleared only once EC2 agrees it is stopped"
check_contains "provision fails when cloud-init never finishes" "$PROVISION" \
  "cloud-init did not finish within"
check_contains "provision repairs an instance with no marker" "$PROVISION" \
  "re-running bootstrap.sh over ssh"
check_contains "provision checks the adopted role's trust policy" "$PROVISION" \
  "Role.AssumeRolePolicyDocument.Statement[0].Principal.Service"
check_contains "provision checks the role's instance-profile membership" "$PROVISION" \
  "list-instance-profiles-for-role"
check_contains "provision requires the owner tag on the security group" "$PROVISION" \
  "assert_owner_tag \"security group"
check_contains "provision revokes every unexpected ingress rule" "$PROVISION" \
  "revoking every existing ingress rule"
check_contains "provision deletes the IAM key when the vault write fails" "$PROVISION" \
  "iam delete-access-key"
check_absent "provision writes no credential file" "$PROVISION" \
  "BACKUP_DIR"
check_contains "remote-ci takes a local lease before any EC2 call" "$REMOTE_CI" \
  "fcntl.flock(9, fcntl.LOCK_EX | fcntl.LOCK_NB)"
check_contains "remote-ci waits out a stopping box" "$REMOTE_CI" \
  "waiting up to \${STOPPING_WAIT_SECONDS}s"
check_contains "remote-ci quotes the Linux script(1) command" "$REMOTE_CI" \
  'linux_cmd="$(printf'
check_contains "remote-ci caps the local log" "$REMOTE_CI" \
  "the head was trimmed"
check_contains "remote-ci refuses an oversized report" "$REMOTE_CI" \
  "over the \${REPORT_CAP_KB} KB cap"
check_contains "remote-ci fails a green gate whose box did not stop" "$REMOTE_CI" \
  'if [ "$STOP_FAILED" = 1 ] && [ "$rc" -eq 0 ]; then rc=1; fi'
check_contains "remote-ci copies the report through a bounded stream" "$REMOTE_CI" \
  'head -c "$cap_bytes"'
check_contains "remote-ci bounds the report transfer in time" "$REMOTE_CI" \
  "timeout 600 tar -C"
check_contains "remote-ci tells a missing report from a failed probe" "$REMOTE_CI" \
  "could not tell whether a playwright report exists"
check_contains "provision turns xtrace off before touching a credential" "$PROVISION" \
  "set +x"
check_contains "provision records the new key as a cleanup obligation" "$PROVISION" \
  'PENDING_KEY_ID="$key_id"'
check_contains "provision revokes a pending key from its trap" "$PROVISION" \
  "revoke_pending_key || true"

printf '\n%s\n' "-----"
if [ "$FAILURES" -eq 0 ]; then
  printf 'all checks passed\n'
  exit 0
fi
printf '%d check(s) failed\n' "$FAILURES"
exit 1
