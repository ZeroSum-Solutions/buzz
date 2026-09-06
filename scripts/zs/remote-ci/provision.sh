#!/usr/bin/env bash
# zs fork: one-time, idempotent provisioning of the on-demand Linux test box.
#
# Creates (or adopts, after proving it owns them) every AWS resource the
# `scripts/zs/remote-ci.sh` pre-push lane needs:
#
#   - EC2 key pair `buzz-ci-box`, private key at ~/.ssh/buzz-ci-box.pem (0600)
#   - security group `buzz-ci-box`, SSH from the caller's public IP only
#   - c7a.8xlarge Ubuntu 24.04 x86_64 instance, 200 GB gp3 root, tagged
#     Name=buzz-ci-box and zs:owner=buzz-remote-ci
#   - IAM role + instance profile letting the box stop only itself
#   - CloudWatch alarm `buzz-ci-box-idle` (CPU < 5% for 30 min -> stop)
#   - IAM user `buzz-ci-runner` with start/stop/describe on that instance only
#   - scripts/zs/remote-ci/box.env, which remote-ci.sh reads
#
# It ends by stopping the instance, so the box costs only its EBS volume when
# idle. Re-running it is safe: every step looks the resource up first, refuses
# to adopt a resource it does not own, and fails closed when a lookup errors.
#
# ── admin credential ─────────────────────────────────────────────────────────
# This script is the only consumer of the AWS admin credential. ZS Vault is the
# source of truth for it, as for every credential on this machine. Two entries:
#
#   aws_buzz_ci_admin_key_id        env ZS_AWS_BUZZ_CI_ADMIN_KEY_ID
#   aws_buzz_ci_admin_secret     env ZS_AWS_BUZZ_CI_ADMIN_SECRET
#
# The vault env file exports those two names into the shell. This script maps
# them into the environment of its own `aws` child processes only: they are
# never exported, never written to ~/.aws, and never printed. If they are unset
# it falls back to AWS_PROFILE (default buzz-ci-admin, which must exist; the root login profile is never used implicitly) so an SSO or assume-role
# profile also works.
#
# Deactivate the admin access key in IAM once provisioning has succeeded; the
# day-to-day lane uses the scoped buzz-ci-runner key instead.
#
# The scoped runner key this script creates goes straight into ZS Vault as
# aws_buzz_ci_runner_key_id and aws_buzz_ci_runner_secret. It is created
# only after the box has proved it bootstrapped, it never touches the disk, and
# a failed vault write deletes it again.
#
# Usage:
#   scripts/zs/remote-ci/provision.sh [--dry-run] [--allow-ip [CIDR]]
#                                     [--no-verify] [--no-wait-bootstrap]
set -euo pipefail

BOX_NAME="buzz-ci-box"
OWNER_TAG_KEY="zs:owner"
OWNER_TAG_VALUE="buzz-remote-ci"
RUNNER_USER="buzz-ci-runner"
ROLE_NAME="${BOX_NAME}-self-stop"
ROLE_POLICY_NAME="self-stop"
USER_POLICY_NAME="${BOX_NAME}-control"
INSTANCE_TYPE="${BUZZ_CI_INSTANCE_TYPE:-c7a.8xlarge}"
ROOT_VOLUME_GB="${BUZZ_CI_ROOT_VOLUME_GB:-200}"
SSH_USER="ci"
AMI_SSM_PARAM="/aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id"
KEY_PATH="${BUZZ_CI_KEY_PATH:-$HOME/.ssh/${BOX_NAME}.pem}"
KNOWN_HOSTS="${BUZZ_CI_KNOWN_HOSTS:-$HOME/.ssh/known_hosts.${BOX_NAME}}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BOX_ENV="${SCRIPT_DIR}/box.env"
BOOTSTRAP="${SCRIPT_DIR}/bootstrap.sh"
BOOTSTRAP_WAIT="${BUZZ_CI_BOOTSTRAP_WAIT:-3600}"
SSH_WAIT="${BUZZ_CI_SSH_WAIT:-300}"
MARKER=/var/lib/buzz-ci-bootstrap-done

# Everything from here on can expand a credential: the admin key below, and the
# runner secret on its way to ZS Vault. `bash -x` would print both to stderr,
# and a redirected provisioning log would then hold them, so tracing is turned
# off here and never turned back on. Trace the lines above this one if you need
# it; below it, --dry-run and the ==> log lines already narrate every aws call.
set +x
AWS_REGION_NAME="${AWS_REGION:-us-west-2}"
if [ -n "${ZS_AWS_BUZZ_CI_ADMIN_KEY_ID-}" ] && [ -n "${ZS_AWS_BUZZ_CI_ADMIN_SECRET-}" ]; then
  VAULT_ADMIN=1
  AWS=(aws --region "$AWS_REGION_NAME" --output text)
  ADMIN_SOURCE="ZS Vault (ZS_AWS_BUZZ_CI_ADMIN_KEY_ID)"
else
  VAULT_ADMIN=0
  AWS=(aws --profile "${AWS_PROFILE:-buzz-ci-admin}" --region "$AWS_REGION_NAME" --output text)
  ADMIN_SOURCE="profile ${AWS_PROFILE:-buzz-ci-admin}"
fi

DRY_RUN=0
ALLOW_IP_ONLY=0
ALLOW_IP_CIDR=""
WAIT_BOOTSTRAP=1
VERIFY_EXISTING=1
LAUNCH_ATTEMPTED=0
RUNNING_INSTANCE=""
STOP_FAILED=0
STOP_BACKOFF="${BUZZ_CI_STOP_BACKOFF:-5}"
# An access key that exists but is not yet in the vault is an obligation, not a
# result: the trap revokes it unless both vault writes finished.
PENDING_KEY_ID=""
CLIENT_TOKEN=""
TRUST_POLICY='{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"ec2.amazonaws.com"},"Action":"sts:AssumeRole"}]}'

usage() {
  sed -n '2,44p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  cat <<'EOF'

Flags:
  --dry-run             Print every aws call that would run; change nothing.
  --allow-ip [CIDR]     Refresh the security group's SSH rule to CIDR (default:
                        this machine's current public IP) and exit. Use after a
                        network change; no other resource is touched.
  --no-verify           Do not start an existing box to confirm its bootstrap
                        marker. Faster, but a half-built box stays half-built.
  --no-wait-bootstrap   Do not wait for cloud-init on a freshly created
                        instance. The box is left running; you must check it.
  -h, --help            This text.

Environment:
  ZS_AWS_BUZZ_CI_ADMIN_KEY_ID / ZS_AWS_BUZZ_CI_ADMIN_SECRET
                        Admin credential from ZS Vault; preferred.
  AWS_PROFILE           Fallback admin profile (default buzz-ci-admin, which must exist; the root login profile is never used implicitly).
  AWS_REGION            Region (default us-west-2).
  BUZZ_CI_INSTANCE_TYPE Instance type (default c7a.8xlarge).
  BUZZ_CI_KEY_PATH      Private key path (default ~/.ssh/buzz-ci-box.pem).
  BUZZ_CI_BOOTSTRAP_WAIT  Seconds to wait for cloud-init (default 3600).
EOF
}

exec 3>&2   # dry-run narration survives a caller's 2>/dev/null
log() { printf '==> %s\n' "$*" >&2; }
die() { printf 'provision: %s\n' "$*" >&2; exit 1; }

# Runs aws with the admin credential injected into the child environment only.
run_aws() {
  if [ "$VAULT_ADMIN" = 1 ]; then
    AWS_ACCESS_KEY_ID="$ZS_AWS_BUZZ_CI_ADMIN_KEY_ID" \
    AWS_SECRET_ACCESS_KEY="$ZS_AWS_BUZZ_CI_ADMIN_SECRET" \
      "${AWS[@]}" "$@"
  else
    "${AWS[@]}" "$@"
  fi
}

# Strict read. Empty output means "no such resource"; a non-zero aws exit is a
# failure and never a "not found" (AGENTS.md rule 1: no swallowed failure).
aws_query() {
  if [ "$DRY_RUN" = 1 ]; then
    printf 'DRY-RUN query: %s %s\n' "${AWS[*]}" "$*" >&3
    return 0
  fi
  run_aws "$@" || die "aws $* failed; refusing to treat that as 'does not exist'"
}

# Mutating call. In --dry-run it prints and does nothing.
aws_do() {
  if [ "$DRY_RUN" = 1 ]; then
    printf 'DRY-RUN: %s %s\n' "${AWS[*]}" "$*" >&3
    return 0
  fi
  run_aws "$@"
}

# IAM has no filter form, so "not found" arrives as an error. Distinguish it
# from every other error rather than swallowing both. Returns, never exits:
# `die` inside a command substitution would only kill the subshell, and the
# caller would read a real failure as "does not exist".
#   rc 0 -> exists, value on stdout;  rc 1 -> NoSuchEntity;  rc 2 -> failed.
iam_get() {
  local err out
  if [ "$DRY_RUN" = 1 ]; then
    printf 'DRY-RUN query: %s %s\n' "${AWS[*]}" "$*" >&3
    return 1
  fi
  err="$(mktemp)"
  if out="$(run_aws "$@" 2>"$err")"; then
    rm -f "$err"; printf '%s\n' "$out"; return 0
  fi
  if grep -q 'NoSuchEntity' "$err"; then rm -f "$err"; return 1; fi
  cat "$err" >&2; rm -f "$err"
  return 2
}

# Sets IAM_VALUE. rc 0 exists, rc 1 missing, dies on any other failure.
IAM_VALUE=""
iam_lookup() {
  local rc
  IAM_VALUE="$(iam_get "$@")" && rc=0 || rc=$?
  case "$rc" in
    0) return 0 ;;
    1) return 1 ;;
    *) die "aws $* failed; refusing to treat that as 'does not exist'" ;;
  esac
}

# Every token of $3 must appear in the allow-list $2, or we do not own $1.
assert_only() {
  local label="$1" allowed="$2" actual="$3" item
  for item in $actual; do
    [ "$item" = None ] && continue
    case " $allowed " in
      *" $item "*) ;;
      *) die "${label} carries '${item}', which this script did not create.
Refusing to adopt it. Rename or remove the colliding resource, then re-run." ;;
    esac
  done
}

# The caller queries Tags[?Key=='<owner key>'].Value, so the value must equal
# the owner value exactly: a resource tagged purpose=buzz-remote-ci is not ours.
assert_owner_tag() { # assert_owner_tag <label> <value-of-the-owner-tag-key>
  [ "$2" = "$OWNER_TAG_VALUE" ] || die "$1 exists but is not tagged
${OWNER_TAG_KEY}=${OWNER_TAG_VALUE} (that tag key reads '${2:-<absent>}').
Something else owns that name. Refusing to adopt it."
}

# assert_equals <label> <expected> <actual>
assert_equals() {
  [ "$2" = "$3" ] || die "${1}: expected '${2}', found '${3}'.
Refusing to adopt a resource this script did not create."
}

while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY_RUN=1; shift ;;
    --no-wait-bootstrap) WAIT_BOOTSTRAP=0; shift ;;
    --no-verify) VERIFY_EXISTING=0; shift ;;
    --allow-ip)
      ALLOW_IP_ONLY=1; shift
      if [ $# -gt 0 ] && [ "${1#-}" = "$1" ]; then ALLOW_IP_CIDR="$1"; shift; fi
      ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown flag: $1 (try --help)" ;;
  esac
done

command -v aws >/dev/null 2>&1 || die "aws CLI not on PATH (brew install awscli)"
[ -f "$BOOTSTRAP" ] || die "missing $BOOTSTRAP"

# ── cleanup trap, armed before anything can leave an instance running ────────
lost_box() { # lost_box <what-we-know>
  cat >&2 <<EOF

provision: THE BOX MAY STILL BE RUNNING AND BILLING, at about \$1.20 per hour.
${1}
Open the EC2 console for ${AWS_REGION_NAME} and look for an instance tagged
Name=${BOX_NAME} / ${OWNER_TAG_KEY}=${OWNER_TAG_VALUE}, or with client token
${CLIENT_TOKEN}. Stop it:

  aws --region ${AWS_REGION_NAME} ec2 stop-instances --instance-ids <id>

The instance was launched before the idle alarm existed and its own uptime
cron only exists once cloud-init finished, so neither backstop is guaranteed.

EOF
  STOP_FAILED=1
}

# Stop the box and keep it recorded until EC2 confirms it stopped. A failed stop
# is never a warning: it costs money, so it is surfaced and it fails the run.
stop_box() {
  local id="$1" attempt
  [ -n "$id" ] || return 0
  if [ "$DRY_RUN" = 1 ]; then
    aws_do ec2 stop-instances --instance-ids "$id" >/dev/null
    RUNNING_INSTANCE=""
    return 0
  fi
  log "stopping ${id}"
  for attempt in 1 2 3 4 5; do
    if run_aws ec2 stop-instances --instance-ids "$id" >/dev/null 2>&1 \
       && run_aws ec2 wait instance-stopped --instance-ids "$id" >/dev/null 2>&1; then
      log "stopped ${id}"
      RUNNING_INSTANCE=""   # cleared only once EC2 agrees it is stopped
      return 0
    fi
    log "stop attempt ${attempt} of 5 failed for ${id}"
    [ "$attempt" = 5 ] || sleep $(( STOP_BACKOFF * attempt ))
  done
  lost_box "Five stop-instances attempts failed for ${id}."
  return 1
}

# run-instances can succeed while the CLI returns nothing useful. Look the
# instance up by its client token and owner tag before giving up on it.
rediscover_box() {
  local attempt found
  for attempt in 1 2 3; do
    found="$(run_aws ec2 describe-instances \
      --filters "Name=client-token,Values=${CLIENT_TOKEN}" \
        "Name=tag:${OWNER_TAG_KEY},Values=${OWNER_TAG_VALUE}" \
        'Name=instance-state-name,Values=pending,running,stopping,stopped' \
      --query 'Reservations[].Instances[].InstanceId' 2>/dev/null)" && {
        found="${found%%[[:space:]]*}"
        [ "$found" = None ] && found=""
        if [ -n "$found" ]; then printf '%s\n' "$found"; return 0; fi
      }
    sleep $(( STOP_BACKOFF * attempt ))
  done
  return 1
}

revoke_pending_key() {
  [ -n "$PENDING_KEY_ID" ] || return 0
  local id="$PENDING_KEY_ID"
  PENDING_KEY_ID=""
  log "revoking the runner access key ${id}: it never reached ZS Vault"
  run_aws iam delete-access-key --user-name "$RUNNER_USER" --access-key-id "$id" \
    >/dev/null 2>&1 && return 0
  cat >&2 <<EOF

provision: AN ACTIVE IAM ACCESS KEY WAS LEFT BEHIND.
${id} for ${RUNNER_USER} was created but never stored, and deleting it failed.
Delete it by hand NOW:

  aws iam delete-access-key --user-name ${RUNNER_USER} --access-key-id ${id}

EOF
  STOP_FAILED=1
  return 1
}

provision_trap() {
  local rc=$?
  revoke_pending_key || true
  if [ -z "$RUNNING_INSTANCE" ] && [ "$LAUNCH_ATTEMPTED" = 1 ] && [ "$DRY_RUN" = 0 ]; then
    if RUNNING_INSTANCE="$(rediscover_box)"; then
      log "recovered the launched instance id: ${RUNNING_INSTANCE}"
    else
      RUNNING_INSTANCE=""
      lost_box "run-instances was issued but the instance id could not be recovered."
    fi
  fi
  [ -n "$RUNNING_INSTANCE" ] && { stop_box "$RUNNING_INSTANCE" || true; }
  if [ "$STOP_FAILED" = 1 ] && [ "$rc" -eq 0 ]; then rc=1; fi
  exit "$rc"
}
# A signal must not look like success: exit with a failing status so the EXIT
# trap below discharges its obligations and the caller sees the interruption.
on_signal() { exit 130; }
trap on_signal INT TERM
trap provision_trap EXIT

caller_cidr() {
  if [ -n "$ALLOW_IP_CIDR" ]; then printf '%s\n' "$ALLOW_IP_CIDR"; return 0; fi
  if [ "$DRY_RUN" = 1 ]; then printf '203.0.113.10/32\n'; return 0; fi
  local ip
  ip="$(curl -fsS --max-time 10 https://checkip.amazonaws.com | tr -d '[:space:]')" \
    || die "could not determine this machine's public IP"
  case "$ip" in
    *[!0-9.]*|"") die "unexpected public IP response: $ip" ;;
  esac
  printf '%s/32\n' "$ip"
}

ssh_box() { # ssh_box <ip> <command...>
  local ip="$1"; shift
  ssh -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile="$KNOWN_HOSTS" \
      -o ConnectTimeout=10 -o BatchMode=yes -i "$KEY_PATH" "ubuntu@${ip}" "$@"
}

wait_for_ssh() { # wait_for_ssh <ip>
  local ip="$1" deadline
  deadline=$(( $(date +%s) + SSH_WAIT ))
  until ssh_box "$ip" true 2>/dev/null; do
    [ "$(date +%s)" -lt "$deadline" ] \
      || die "ssh to ${ip} did not come up within ${SSH_WAIT}s"
    sleep 5
  done
}

# ── account ──────────────────────────────────────────────────────────────────
ACCOUNT_ID="$(aws_query sts get-caller-identity --query Account)"
ACCOUNT_ID="${ACCOUNT_ID:-000000000000}"
CLIENT_TOKEN="${BOX_NAME}-${AWS_REGION_NAME}-${ACCOUNT_ID}"
log "account ${ACCOUNT_ID}, region ${AWS_REGION_NAME}, admin from ${ADMIN_SOURCE}"

# ── instance discovery first: fail before creating anything ─────────────────
INSTANCE_IDS="$(aws_query ec2 describe-instances \
  --filters "Name=tag:Name,Values=${BOX_NAME}" \
    "Name=tag:${OWNER_TAG_KEY},Values=${OWNER_TAG_VALUE}" \
    'Name=instance-state-name,Values=pending,running,stopping,stopped' \
  --query 'Reservations[].Instances[].InstanceId')"
INSTANCE_COUNT=0
for _id in $INSTANCE_IDS; do INSTANCE_COUNT=$((INSTANCE_COUNT + 1)); done
if [ "$INSTANCE_COUNT" -gt 1 ]; then
  die "found ${INSTANCE_COUNT} instances tagged Name=${BOX_NAME} ${OWNER_TAG_KEY}=${OWNER_TAG_VALUE}:
${INSTANCE_IDS}
EC2 tags are not unique, so this script cannot tell which one is the box.
Terminate the extras, then re-run."
fi
INSTANCE_ID="${INSTANCE_IDS%%[[:space:]]*}"
[ "$INSTANCE_ID" = None ] && INSTANCE_ID=""

# ── security group ───────────────────────────────────────────────────────────
VPC_ID="$(aws_query ec2 describe-vpcs --filters Name=isDefault,Values=true \
  --query 'Vpcs[0].VpcId')"
[ "$VPC_ID" = "None" ] && VPC_ID=""
VPC_ID="${VPC_ID:-vpc-0dryrun}"

SG_ID="$(aws_query ec2 describe-security-groups \
  --filters "Name=group-name,Values=${BOX_NAME}" "Name=vpc-id,Values=${VPC_ID}" \
  --query 'SecurityGroups[0].GroupId')"
[ "$SG_ID" = "None" ] && SG_ID=""
if [ -z "$SG_ID" ]; then
  log "creating security group ${BOX_NAME}"
  SG_ID="$(aws_do ec2 create-security-group --group-name "$BOX_NAME" \
    --description "SSH to the buzz on-demand CI box" --vpc-id "$VPC_ID" \
    --tag-specifications "ResourceType=security-group,Tags=[{Key=${OWNER_TAG_KEY},Value=${OWNER_TAG_VALUE}}]" \
    --query GroupId)"
  SG_ID="${SG_ID:-sg-0dryrun}"
else
  log "security group ${BOX_NAME} exists (${SG_ID})"
  # A group with this name that we did not create could already permit anything
  # from anywhere, and the box would launch into it.
  sg_owner="$(aws_query ec2 describe-security-groups --group-ids "$SG_ID" \
    --query "SecurityGroups[0].Tags[?Key=='${OWNER_TAG_KEY}']|[0].Value")"
  [ "$sg_owner" = None ] && sg_owner=""
  assert_owner_tag "security group ${BOX_NAME} (${SG_ID})" "$sg_owner"
fi

# One ingress rule, ours. Every other ingress rule is revoked rather than left
# in place: a stray "test port from 0.0.0.0/0" would otherwise survive, because
# a rule refresh that only touches port 22 never sees it.
refresh_ssh_rule() {
  local cidr rule_ids
  cidr="$(caller_cidr)"
  rule_ids="$(aws_query ec2 describe-security-group-rules \
    --filters "Name=group-id,Values=${SG_ID}" \
    --query 'SecurityGroupRules[?IsEgress==`false`].SecurityGroupRuleId')"
  if [ -n "$rule_ids" ] && [ "$rule_ids" != "None" ]; then
    log "revoking every existing ingress rule on ${SG_ID}"
    # shellcheck disable=SC2086 # rule ids are a space-separated list by design
    aws_do ec2 revoke-security-group-ingress --group-id "$SG_ID" \
      --security-group-rule-ids $rule_ids >/dev/null
  fi
  log "allowing SSH from ${cidr}"
  aws_do ec2 authorize-security-group-ingress --group-id "$SG_ID" \
    --ip-permissions "IpProtocol=tcp,FromPort=22,ToPort=22,IpRanges=[{CidrIp=${cidr},Description=buzz-ci-caller}]" \
    >/dev/null
}
refresh_ssh_rule

if [ "$ALLOW_IP_ONLY" = 1 ]; then
  log "security group rule refreshed; nothing else touched"
  exit 0
fi

# ── key pair ─────────────────────────────────────────────────────────────────
# --filters, not --key-names: a filter that matches nothing is an empty result,
# while --key-names on a missing key is an error we would have to swallow.
KEY_FINGERPRINT="$(aws_query ec2 describe-key-pairs \
  --filters "Name=key-name,Values=${BOX_NAME}" \
  --query 'KeyPairs[0].KeyFingerprint')"
[ "$KEY_FINGERPRINT" = "None" ] && KEY_FINGERPRINT=""
if [ -z "$KEY_FINGERPRINT" ]; then
  log "creating key pair ${BOX_NAME} -> ${KEY_PATH}"
  if [ "$DRY_RUN" = 1 ]; then
    aws_do ec2 create-key-pair --key-name "$BOX_NAME" --query KeyMaterial
    printf 'DRY-RUN: write %s (0600)\n' "$KEY_PATH" >&3
  else
    [ -e "$KEY_PATH" ] && die "AWS has no key pair ${BOX_NAME} but ${KEY_PATH} exists.
Move that stale file aside, then re-run."
    mkdir -p "$(dirname "$KEY_PATH")"
    ( umask 077 && run_aws ec2 create-key-pair --key-name "$BOX_NAME" \
        --tag-specifications "ResourceType=key-pair,Tags=[{Key=${OWNER_TAG_KEY},Value=${OWNER_TAG_VALUE}}]" \
        --query KeyMaterial > "$KEY_PATH" )
    chmod 600 "$KEY_PATH"
  fi
elif [ "$DRY_RUN" = 0 ]; then
  log "key pair ${BOX_NAME} exists"
  [ -f "$KEY_PATH" ] || die "key pair ${BOX_NAME} exists in AWS but ${KEY_PATH} is missing.
AWS cannot re-issue a private key. Delete the key pair
(aws ec2 delete-key-pair --key-name ${BOX_NAME}), then re-run and re-create
the instance."
  [ -O "$KEY_PATH" ] || die "${KEY_PATH} is not owned by you; refusing to use it."
  # OpenSSH refuses a group- or world-readable private key, so repair the mode
  # on every run rather than failing later inside ssh.
  chmod 600 "$KEY_PATH"
  # AWS reports a SHA-1 fingerprint of the PKCS#8 DER private key for the RSA
  # keys create-key-pair issues. Check it when we can; a mismatch means this
  # PEM belongs to a deleted-and-recreated key pair and ssh would fail later.
  if command -v openssl >/dev/null 2>&1 && [ "${#KEY_FINGERPRINT}" -eq 59 ]; then
    local_fp="$(openssl pkcs8 -in "$KEY_PATH" -nocrypt -topk8 -outform DER 2>/dev/null \
      | openssl sha1 -c 2>/dev/null | awk '{print $NF}')" || local_fp=""
    if [ -n "$local_fp" ] && [ "$local_fp" != "$KEY_FINGERPRINT" ]; then
      die "${KEY_PATH} does not match the AWS key pair ${BOX_NAME}
(local ${local_fp}, AWS ${KEY_FINGERPRINT}). Move the stale PEM aside, delete
the AWS key pair, and re-run."
    fi
  fi
fi

# ── IAM role: the box may stop only itself ───────────────────────────────────
if iam_lookup iam get-role --role-name "$ROLE_NAME" --query 'Role.Arn'; then
  log "IAM role ${ROLE_NAME} exists (${IAM_VALUE})"
  role_tags="$(aws_query iam list-role-tags --role-name "$ROLE_NAME" --query "Tags[?Key=='${OWNER_TAG_KEY}']|[0].Value")"
  [ "$role_tags" = None ] && role_tags=""
  assert_owner_tag "IAM role ${ROLE_NAME}" "$role_tags"
  role_managed="$(aws_query iam list-attached-role-policies --role-name "$ROLE_NAME" --query "AttachedPolicies[].PolicyName")"
  assert_only "IAM role ${ROLE_NAME}" "" "$role_managed"
  role_inline="$(aws_query iam list-role-policies --role-name "$ROLE_NAME" --query "PolicyNames")"
  assert_only "IAM role ${ROLE_NAME}" "$ROLE_POLICY_NAME" "$role_inline"
  # Policy names alone do not say who may assume the role. A role that also
  # trusts an arbitrary AWS principal would let that principal stop this box,
  # so require the exact EC2-only trust document, statement by statement.
  trust_count="$(aws_query iam get-role --role-name "$ROLE_NAME" \
    --query 'length(Role.AssumeRolePolicyDocument.Statement)')"
  assert_equals "IAM role ${ROLE_NAME} trust policy statement count" 1 "$trust_count"
  trust_service="$(aws_query iam get-role --role-name "$ROLE_NAME" \
    --query 'Role.AssumeRolePolicyDocument.Statement[0].Principal.Service')"
  assert_equals "IAM role ${ROLE_NAME} trusted service" ec2.amazonaws.com "$trust_service"
  trust_action="$(aws_query iam get-role --role-name "$ROLE_NAME" \
    --query 'Role.AssumeRolePolicyDocument.Statement[0].Action')"
  assert_equals "IAM role ${ROLE_NAME} trusted action" sts:AssumeRole "$trust_action"
  trust_effect="$(aws_query iam get-role --role-name "$ROLE_NAME" \
    --query 'Role.AssumeRolePolicyDocument.Statement[0].Effect')"
  assert_equals "IAM role ${ROLE_NAME} trust effect" Allow "$trust_effect"
  trust_other="$(aws_query iam get-role --role-name "$ROLE_NAME" \
    --query 'Role.AssumeRolePolicyDocument.Statement[0].[Principal.AWS,Principal.Federated,Condition]')"
  case "$(printf '%s' "$trust_other" | tr -d '[:space:]')" in
    NoneNoneNone|None|'') ;;
    *) die "IAM role ${ROLE_NAME} trusts more than the EC2 service (${trust_other}).
Refusing to adopt it." ;;
  esac
  # And it must belong to our instance profile, and only to ours.
  role_profiles="$(aws_query iam list-instance-profiles-for-role --role-name "$ROLE_NAME" \
    --query 'InstanceProfiles[].InstanceProfileName')"
  assert_only "IAM role ${ROLE_NAME} instance-profile membership" "$BOX_NAME" "$role_profiles"
else
  log "creating IAM role ${ROLE_NAME}"
  aws_do iam create-role --role-name "$ROLE_NAME" \
    --tags "Key=${OWNER_TAG_KEY},Value=${OWNER_TAG_VALUE}" \
    --assume-role-policy-document "$TRUST_POLICY" \
    >/dev/null
fi
# Scoped by tag so the policy is valid before the instance exists; tightened to
# the exact instance ARN once the id is known (further down).
aws_do iam put-role-policy --role-name "$ROLE_NAME" --policy-name "$ROLE_POLICY_NAME" \
  --policy-document "{\"Version\":\"2012-10-17\",\"Statement\":[{\"Effect\":\"Allow\",\"Action\":\"ec2:StopInstances\",\"Resource\":\"arn:aws:ec2:${AWS_REGION_NAME}:${ACCOUNT_ID}:instance/*\",\"Condition\":{\"StringEquals\":{\"ec2:ResourceTag/Name\":\"${BOX_NAME}\"}}}]}" \
  >/dev/null

if iam_lookup iam get-instance-profile --instance-profile-name "$BOX_NAME" \
    --query 'InstanceProfile.Roles[].RoleName'; then
  log "instance profile ${BOX_NAME} exists"
  profile_roles="$IAM_VALUE"
  profile_tags="$(aws_query iam list-instance-profile-tags --instance-profile-name "$BOX_NAME" --query "Tags[?Key=='${OWNER_TAG_KEY}']|[0].Value")"
  [ "$profile_tags" = None ] && profile_tags=""
  assert_owner_tag "instance profile ${BOX_NAME}" "$profile_tags"
  assert_only "instance profile ${BOX_NAME}" "$ROLE_NAME" "$profile_roles"
else
  log "creating instance profile ${BOX_NAME}"
  aws_do iam create-instance-profile --instance-profile-name "$BOX_NAME" \
    --tags "Key=${OWNER_TAG_KEY},Value=${OWNER_TAG_VALUE}" >/dev/null
  aws_do iam add-role-to-instance-profile --instance-profile-name "$BOX_NAME" \
    --role-name "$ROLE_NAME" >/dev/null
  [ "$DRY_RUN" = 1 ] || sleep 12  # IAM instance profiles propagate slowly
fi

# ── instance ─────────────────────────────────────────────────────────────────
CREATED=0
if [ -z "$INSTANCE_ID" ]; then
  AMI_ID="$(aws_query ssm get-parameter --name "$AMI_SSM_PARAM" --query Parameter.Value)"
  AMI_ID="${AMI_ID:-ami-0dryrun}"
  log "launching ${INSTANCE_TYPE} from ${AMI_ID}"
  # A stable client token makes run-instances idempotent, so an interrupted run
  # that already launched adopts that instance instead of launching a second.
  LAUNCH_ATTEMPTED=1
  INSTANCE_ID="$(aws_do ec2 run-instances \
    --client-token "$CLIENT_TOKEN" \
    --image-id "$AMI_ID" \
    --instance-type "$INSTANCE_TYPE" \
    --key-name "$BOX_NAME" \
    --security-group-ids "$SG_ID" \
    --iam-instance-profile "Name=${BOX_NAME}" \
    --instance-initiated-shutdown-behavior stop \
    --metadata-options 'HttpTokens=required,HttpEndpoint=enabled' \
    --block-device-mappings "DeviceName=/dev/sda1,Ebs={VolumeSize=${ROOT_VOLUME_GB},VolumeType=gp3,DeleteOnTermination=true,Encrypted=true}" \
    --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=${BOX_NAME}},{Key=${OWNER_TAG_KEY},Value=${OWNER_TAG_VALUE}}]" \
      "ResourceType=volume,Tags=[{Key=Name,Value=${BOX_NAME}},{Key=${OWNER_TAG_KEY},Value=${OWNER_TAG_VALUE}}]" \
    --user-data "file://${BOOTSTRAP}" \
    --query 'Instances[0].InstanceId')"
  if [ "$DRY_RUN" = 1 ]; then
    INSTANCE_ID="i-0dryrun0000000000"
  else
    # EC2 can accept run-instances and still return nothing useful. Recording a
    # placeholder here would make the trap "stop" an id that does not exist
    # while the real instance keeps billing with no alarm and no cron, so ask
    # EC2 what it actually launched instead of inventing an id.
    case "$INSTANCE_ID" in
      i-[0-9a-f]*) : ;;
      *)
        log "run-instances returned '${INSTANCE_ID:-<empty>}'; asking EC2 what it launched"
        INSTANCE_ID="$(rediscover_box)" \
          || lost_box "run-instances was issued but no instance could be found by
its client token ${CLIENT_TOKEN} and owner tag."
        case "$INSTANCE_ID" in
          i-[0-9a-f]*) log "recovered the launched instance id: ${INSTANCE_ID}" ;;
          *) die "could not determine the launched instance id" ;;
        esac
        ;;
    esac
  fi
  RUNNING_INSTANCE="$INSTANCE_ID"
  CREATED=1
else
  log "instance ${INSTANCE_ID} exists"
fi

INSTANCE_ARN="arn:aws:ec2:${AWS_REGION_NAME}:${ACCOUNT_ID}:instance/${INSTANCE_ID}"

# Tighten the box's own policy to the exact instance now that the id is known.
aws_do iam put-role-policy --role-name "$ROLE_NAME" --policy-name "$ROLE_POLICY_NAME" \
  --policy-document "{\"Version\":\"2012-10-17\",\"Statement\":[{\"Effect\":\"Allow\",\"Action\":\"ec2:StopInstances\",\"Resource\":\"${INSTANCE_ARN}\"}]}" \
  >/dev/null

# ── idle alarm: CPU < 5% for 30 minutes stops the box ────────────────────────
log "putting CloudWatch alarm ${BOX_NAME}-idle"
aws_do cloudwatch put-metric-alarm \
  --alarm-name "${BOX_NAME}-idle" \
  --alarm-description "Stop ${BOX_NAME} when it has been idle for 30 minutes" \
  --namespace AWS/EC2 --metric-name CPUUtilization --statistic Average \
  --dimensions "Name=InstanceId,Value=${INSTANCE_ID}" \
  --period 300 --evaluation-periods 6 --threshold 5 \
  --comparison-operator LessThanThreshold --treat-missing-data missing \
  --alarm-actions "arn:aws:automate:${AWS_REGION_NAME}:ec2:stop" >/dev/null

# ── scoped runner user ───────────────────────────────────────────────────────
if iam_lookup iam get-user --user-name "$RUNNER_USER" --query 'User.Arn'; then
  log "IAM user ${RUNNER_USER} exists (${IAM_VALUE})"
  user_tags="$(aws_query iam list-user-tags --user-name "$RUNNER_USER" --query "Tags[?Key=='${OWNER_TAG_KEY}']|[0].Value")"
  [ "$user_tags" = None ] && user_tags=""
  assert_owner_tag "IAM user ${RUNNER_USER}" "$user_tags"
  user_managed="$(aws_query iam list-attached-user-policies --user-name "$RUNNER_USER" --query "AttachedPolicies[].PolicyName")"
  assert_only "IAM user ${RUNNER_USER}" "" "$user_managed"
  user_groups="$(aws_query iam list-groups-for-user --user-name "$RUNNER_USER" --query "Groups[].GroupName")"
  assert_only "IAM user ${RUNNER_USER}" "" "$user_groups"
  user_inline="$(aws_query iam list-user-policies --user-name "$RUNNER_USER" --query "PolicyNames")"
  assert_only "IAM user ${RUNNER_USER}" "$USER_POLICY_NAME" "$user_inline"
else
  log "creating IAM user ${RUNNER_USER}"
  aws_do iam create-user --user-name "$RUNNER_USER" \
    --tags "Key=${OWNER_TAG_KEY},Value=${OWNER_TAG_VALUE}" >/dev/null
fi
# ec2:Describe* cannot be resource-scoped (AWS does not support it); the two
# mutating actions are pinned to this one instance ARN.
aws_do iam put-user-policy --user-name "$RUNNER_USER" --policy-name "$USER_POLICY_NAME" \
  --policy-document "{\"Version\":\"2012-10-17\",\"Statement\":[{\"Sid\":\"ControlThisBox\",\"Effect\":\"Allow\",\"Action\":[\"ec2:StartInstances\",\"ec2:StopInstances\"],\"Resource\":\"${INSTANCE_ARN}\"},{\"Sid\":\"DescribeIsNotResourceScopable\",\"Effect\":\"Allow\",\"Action\":[\"ec2:DescribeInstances\",\"ec2:DescribeInstanceStatus\"],\"Resource\":\"*\"}]}" \
  >/dev/null

# The runner access key is created LAST, after the box has proved it works.
# See "runner key" below: a key created here and then abandoned by a failing
# bootstrap would be an active long-lived credential outside the vault.

# ── bootstrap must have completed, or this box is not usable ────────────────
verify_bootstrap() { # verify_bootstrap <instance-id> <created:0|1>
  local id="$1" created="$2" state ip deadline
  state="$(aws_query ec2 describe-instances --instance-ids "$id" \
    --query 'Reservations[0].Instances[0].State.Name')"
  if [ "$state" = stopping ]; then
    run_aws ec2 wait instance-stopped --instance-ids "$id" || die "instance stuck stopping"
    state=stopped
  fi
  if [ "$state" != running ]; then
    log "starting ${id} to check its bootstrap marker"
    aws_do ec2 start-instances --instance-ids "$id" >/dev/null
  fi
  RUNNING_INSTANCE="$id"
  run_aws ec2 wait instance-running --instance-ids "$id" || die "instance did not reach running"
  ip="$(aws_query ec2 describe-instances --instance-ids "$id" \
    --query 'Reservations[0].Instances[0].PublicIpAddress')"
  [ -n "$ip" ] && [ "$ip" != None ] || die "instance ${id} has no public IP"

  if [ "$created" = 1 ]; then
    log "waiting up to ${BOOTSTRAP_WAIT}s for cloud-init on ${ip}"
    deadline=$(( $(date +%s) + BOOTSTRAP_WAIT ))
    until ssh_box "$ip" "test -f ${MARKER}" 2>/dev/null; do
      [ "$(date +%s)" -lt "$deadline" ] \
        || die "cloud-init did not finish within ${BOOTSTRAP_WAIT}s.
Read /var/log/buzz-bootstrap.log on the box (it is being stopped now), fix the
cause, then re-run this script: it repairs an instance with no marker."
      sleep 20
    done
    log "cloud-init finished"
    return 0
  fi

  wait_for_ssh "$ip"
  if ssh_box "$ip" "test -f ${MARKER}" 2>/dev/null; then
    log "bootstrap marker present on ${id}"
    return 0
  fi
  log "${id} has no bootstrap marker; re-running bootstrap.sh over ssh"
  scp -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile="$KNOWN_HOSTS" \
      -o BatchMode=yes -i "$KEY_PATH" "$BOOTSTRAP" "ubuntu@${ip}:/tmp/buzz-bootstrap.sh" \
    >/dev/null || die "could not copy bootstrap.sh to ${id}"
  ssh_box "$ip" 'sudo bash /tmp/buzz-bootstrap.sh' \
    || die "bootstrap.sh failed on ${id}; see /var/log/buzz-bootstrap.log"
  ssh_box "$ip" "test -f ${MARKER}" \
    || die "bootstrap.sh finished without writing ${MARKER} on ${id}"
  log "bootstrap repaired"
}

BOX_VERIFIED=0
if [ "$DRY_RUN" = 1 ]; then
  log "dry run: skipping the bootstrap check"
  BOX_VERIFIED=1
  stop_box "$INSTANCE_ID" || true
elif [ "$CREATED" = 1 ] && [ "$WAIT_BOOTSTRAP" = 0 ]; then
  log "--no-wait-bootstrap: the box is left RUNNING and unverified."
  log "check ${MARKER} yourself, then stop it: aws ec2 stop-instances --instance-ids ${INSTANCE_ID}"
  RUNNING_INSTANCE=""
elif [ "$CREATED" = 0 ] && [ "$VERIFY_EXISTING" = 0 ]; then
  log "--no-verify: not starting ${INSTANCE_ID} to check its bootstrap marker"
  stop_box "$INSTANCE_ID" || true
else
  verify_bootstrap "$INSTANCE_ID" "$CREATED"
  BOX_VERIFIED=1
  stop_box "$INSTANCE_ID" || true
fi

# ── runner key: created last, stored straight into ZS Vault ─────────────────
# The secret never touches the disk. It is created only once the box has proved
# it bootstrapped, and if either vault write fails the IAM key is deleted again,
# so an active long-lived credential can never be left outside the vault.
EXISTING_KEYS="$(aws_query iam list-access-keys --user-name "$RUNNER_USER" \
  --query 'AccessKeyMetadata[?Status==`Active`].AccessKeyId')"
[ "$EXISTING_KEYS" = "None" ] && EXISTING_KEYS=""
if [ -n "$EXISTING_KEYS" ]; then
  log "${RUNNER_USER} already has an active access key (${EXISTING_KEYS}); not creating another"
  log "if you no longer hold its secret, delete that key and re-run this script"
elif [ "$BOX_VERIFIED" = 0 ]; then
  log "the box was not verified, so no runner key was created."
  log "re-run this script once the box bootstraps; it will create the key then."
else
  command -v zsvault >/dev/null 2>&1 || [ "$DRY_RUN" = 1 ] \
    || die "zsvault is not on PATH. The runner secret is only ever handed to
ZS Vault, never written to disk, so provisioning stops here. Install or fix
zsvault, then re-run: everything else is already in place."
  log "creating one access key for ${RUNNER_USER} and storing it in ZS Vault"
  if [ "$DRY_RUN" = 1 ]; then
    aws_do iam create-access-key --user-name "$RUNNER_USER" \
      --query 'AccessKey.[AccessKeyId,SecretAccessKey]'
    printf 'DRY-RUN: zsvault add aws_buzz_ci_runner_key_id --type api_key --env-name ZS_AWS_BUZZ_CI_RUNNER_KEY_ID --yes --value-stdin\n' >&3
    printf 'DRY-RUN: zsvault add aws_buzz_ci_runner_secret --type api_key --env-name ZS_AWS_BUZZ_CI_RUNNER_SECRET --yes --value-stdin\n' >&3
  else
    key_line="$(run_aws iam create-access-key --user-name "$RUNNER_USER" \
      --query 'AccessKey.[AccessKeyId,SecretAccessKey]')"
    key_id="$(printf '%s' "$key_line" | awk '{print $1}')"
    [ -n "$key_id" ] || die "create-access-key returned no key id"
    # From here until both vault writes land, this key is an obligation the
    # EXIT/INT/TERM trap must discharge: a Ctrl-C in between would otherwise
    # leave an active long-lived credential whose secret nobody holds.
    PENDING_KEY_ID="$key_id"
    vault_ok=1
    printf '%s' "$key_line" | awk '{printf "%s", $1}' \
      | zsvault add aws_buzz_ci_runner_key_id --type api_key \
          --env-name ZS_AWS_BUZZ_CI_RUNNER_KEY_ID --yes --value-stdin >/dev/null || vault_ok=0
    if [ "$vault_ok" = 1 ]; then
      printf '%s' "$key_line" | awk '{printf "%s", $2}' \
        | zsvault add aws_buzz_ci_runner_secret --type api_key \
            --env-name ZS_AWS_BUZZ_CI_RUNNER_SECRET --yes --value-stdin >/dev/null || vault_ok=0
    fi
    key_line=""
    if [ "$vault_ok" = 0 ]; then
      log "a zsvault write failed; the IAM access key is being revoked again"
      revoke_pending_key || true
      die "could not store the runner key in ZS Vault.
Fix zsvault, then re-run this script."
    fi
    PENDING_KEY_ID=""   # both halves are in the vault; the obligation is met
    log "runner key ${key_id} stored as aws_buzz_ci_runner_key_id and aws_buzz_ci_runner_secret"
  fi
fi

# ── box.env ──────────────────────────────────────────────────────────────────
if [ "$DRY_RUN" = 1 ]; then
  printf 'DRY-RUN: write %s\n' "$BOX_ENV" >&3
else
  cat > "$BOX_ENV" <<EOF
# Written by scripts/zs/remote-ci/provision.sh. Not committed.
BUZZ_CI_INSTANCE_ID=${INSTANCE_ID}
BUZZ_CI_REGION=${AWS_REGION_NAME}
BUZZ_CI_KEY_PATH=${KEY_PATH}
BUZZ_CI_SSH_USER=${SSH_USER}
EOF
fi

log "instance id: ${INSTANCE_ID}"
log "box.env: ${BOX_ENV}"
log "next: store the runner key in ZS Vault, then scripts/zs/remote-ci.sh <branch>"
log "then deactivate the admin access key in IAM; it is only needed here"
