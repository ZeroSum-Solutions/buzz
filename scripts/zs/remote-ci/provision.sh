#!/usr/bin/env bash
# zs fork: one-time, idempotent provisioning of the on-demand Linux test box.
#
# Creates (or adopts, when they already exist) every AWS resource the
# `scripts/zs/remote-ci.sh` pre-push lane needs:
#
#   - EC2 key pair `buzz-ci-box`, private key at ~/.ssh/buzz-ci-box.pem (0600)
#   - security group `buzz-ci-box`, SSH from the caller's public IP only
#   - c7a.8xlarge Ubuntu 24.04 x86_64 instance, 200 GB gp3 root, tag Name=buzz-ci-box
#   - IAM role + instance profile letting the box stop only itself
#   - CloudWatch alarm `buzz-ci-box-idle` (CPU < 5% for 30 min -> stop)
#   - IAM user `buzz-ci-runner` with start/stop/describe on that instance only
#   - scripts/zs/remote-ci/box.env, which remote-ci.sh reads
#
# It ends by stopping the instance, so the box costs only its EBS volume when
# idle. Re-running it is safe: every step looks the resource up first.
#
# Credentials: this script is the ONLY consumer of the admin profile. Devin
# creates it once with
#
#   aws configure --profile zs-admin
#
# and the admin access key is never stored in ZS Vault. The scoped runner key
# this script creates IS stored there (see the printed `zsvault add` line).
#
# Usage:
#   scripts/zs/remote-ci/provision.sh [--dry-run] [--allow-ip [CIDR]] [--no-wait-bootstrap]
set -euo pipefail

BOX_NAME="buzz-ci-box"
RUNNER_USER="buzz-ci-runner"
INSTANCE_TYPE="${BUZZ_CI_INSTANCE_TYPE:-c7a.8xlarge}"
ROOT_VOLUME_GB="${BUZZ_CI_ROOT_VOLUME_GB:-200}"
SSH_USER="ci"
AMI_SSM_PARAM="/aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id"
KEY_PATH="${BUZZ_CI_KEY_PATH:-$HOME/.ssh/${BOX_NAME}.pem}"
BACKUP_DIR="${BUZZ_CI_BACKUP_DIR:-$HOME/Backups/aws}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BOX_ENV="${SCRIPT_DIR}/box.env"
BOOTSTRAP="${SCRIPT_DIR}/bootstrap.sh"
BOOTSTRAP_WAIT="${BUZZ_CI_BOOTSTRAP_WAIT:-3600}"

AWS_PROFILE_NAME="${AWS_PROFILE:-zs-admin}"
AWS_REGION_NAME="${AWS_REGION:-us-east-1}"
AWS=(aws --profile "$AWS_PROFILE_NAME" --region "$AWS_REGION_NAME" --output text)

DRY_RUN=0
ALLOW_IP_ONLY=0
ALLOW_IP_CIDR=""
WAIT_BOOTSTRAP=1

usage() {
  sed -n '2,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  cat <<'EOF'

Flags:
  --dry-run             Print every aws call that would run; change nothing.
  --allow-ip [CIDR]     Refresh the security group's SSH rule to CIDR (default:
                        this machine's current public IP) and exit. Use after a
                        network change; no other resource is touched.
  --no-wait-bootstrap   Do not wait for cloud-init to finish on a freshly
                        created instance before stopping it.
  -h, --help            This text.

Environment:
  AWS_PROFILE           Admin profile (default zs-admin).
  AWS_REGION            Region (default us-east-1).
  BUZZ_CI_INSTANCE_TYPE Instance type (default c7a.8xlarge).
  BUZZ_CI_KEY_PATH      Where the private key is written (default ~/.ssh/buzz-ci-box.pem).
EOF
}

exec 3>&2   # dry-run narration survives a caller's 2>/dev/null
log() { printf '==> %s\n' "$*"; }
die() { printf 'provision: %s\n' "$*" >&2; exit 1; }

# A read-only AWS call. In --dry-run it prints the call and yields no output,
# so every caller takes its "resource does not exist yet" branch and the create
# calls are printed too.
aws_query() {
  if [ "$DRY_RUN" = 1 ]; then
    printf 'DRY-RUN query: %s %s\n' "${AWS[*]}" "$*" >&3
    return 0
  fi
  "${AWS[@]}" "$@"
}

# A mutating AWS call. In --dry-run it prints and does nothing.
aws_do() {
  if [ "$DRY_RUN" = 1 ]; then
    printf 'DRY-RUN: %s %s\n' "${AWS[*]}" "$*" >&3
    return 0
  fi
  "${AWS[@]}" "$@"
}

while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY_RUN=1; shift ;;
    --no-wait-bootstrap) WAIT_BOOTSTRAP=0; shift ;;
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

# ── account ──────────────────────────────────────────────────────────────────
ACCOUNT_ID="$(aws_query sts get-caller-identity --query Account || true)"
ACCOUNT_ID="${ACCOUNT_ID:-000000000000}"
log "account $ACCOUNT_ID, region $AWS_REGION_NAME, profile $AWS_PROFILE_NAME"

# ── security group ───────────────────────────────────────────────────────────
VPC_ID="$(aws_query ec2 describe-vpcs --filters Name=isDefault,Values=true \
  --query 'Vpcs[0].VpcId' || true)"
[ "$VPC_ID" = "None" ] && VPC_ID=""
VPC_ID="${VPC_ID:-vpc-0dryrun}"

SG_ID="$(aws_query ec2 describe-security-groups \
  --filters "Name=group-name,Values=${BOX_NAME}" "Name=vpc-id,Values=${VPC_ID}" \
  --query 'SecurityGroups[0].GroupId' 2>/dev/null || true)"
[ "$SG_ID" = "None" ] && SG_ID=""
if [ -z "$SG_ID" ]; then
  log "creating security group ${BOX_NAME}"
  SG_ID="$(aws_do ec2 create-security-group --group-name "$BOX_NAME" \
    --description "SSH to the buzz on-demand CI box" --vpc-id "$VPC_ID" \
    --query GroupId || true)"
  SG_ID="${SG_ID:-sg-0dryrun}"
else
  log "security group ${BOX_NAME} exists (${SG_ID})"
fi

refresh_ssh_rule() {
  local cidr rule_ids
  cidr="$(caller_cidr)"
  rule_ids="$(aws_query ec2 describe-security-group-rules \
    --filters "Name=group-id,Values=${SG_ID}" \
    --query 'SecurityGroupRules[?IsEgress==`false` && FromPort==`22`].SecurityGroupRuleId' \
    || true)"
  if [ -n "$rule_ids" ] && [ "$rule_ids" != "None" ]; then
    # shellcheck disable=SC2086 # rule ids are a space-separated list by design
    aws_do ec2 revoke-security-group-rules --group-id "$SG_ID" \
      --security-group-rule-ids $rule_ids >/dev/null || true
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
KEY_EXISTS="$(aws_query ec2 describe-key-pairs --key-names "$BOX_NAME" \
  --query 'KeyPairs[0].KeyName' 2>/dev/null || true)"
[ "$KEY_EXISTS" = "None" ] && KEY_EXISTS=""
if [ -z "$KEY_EXISTS" ]; then
  log "creating key pair ${BOX_NAME} -> ${KEY_PATH}"
  if [ "$DRY_RUN" = 1 ]; then
    aws_do ec2 create-key-pair --key-name "$BOX_NAME" --query KeyMaterial
    printf 'DRY-RUN: write %s (0600)\n' "$KEY_PATH" >&3
  else
    mkdir -p "$(dirname "$KEY_PATH")"
    ( umask 077 && "${AWS[@]}" ec2 create-key-pair --key-name "$BOX_NAME" \
        --query KeyMaterial > "$KEY_PATH" )
    chmod 600 "$KEY_PATH"
  fi
else
  log "key pair ${BOX_NAME} exists"
  if [ "$DRY_RUN" = 0 ] && [ ! -f "$KEY_PATH" ]; then
    die "key pair ${BOX_NAME} exists in AWS but ${KEY_PATH} is missing.
AWS cannot re-issue the private key. Delete the key pair
(aws --profile ${AWS_PROFILE_NAME} ec2 delete-key-pair --key-name ${BOX_NAME}),
then re-run this script and re-create the instance."
  fi
fi

# ── IAM role: the box may stop only itself ───────────────────────────────────
ROLE_NAME="${BOX_NAME}-self-stop"
ROLE_EXISTS="$(aws_query iam get-role --role-name "$ROLE_NAME" --query 'Role.RoleName' 2>/dev/null || true)"
[ "$ROLE_EXISTS" = "None" ] && ROLE_EXISTS=""
if [ -z "$ROLE_EXISTS" ]; then
  log "creating IAM role ${ROLE_NAME}"
  aws_do iam create-role --role-name "$ROLE_NAME" \
    --assume-role-policy-document '{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"ec2.amazonaws.com"},"Action":"sts:AssumeRole"}]}' \
    >/dev/null
fi
# Scoped by tag so the policy is valid before the instance exists; tightened to
# the exact instance ARN once the id is known (further down).
aws_do iam put-role-policy --role-name "$ROLE_NAME" --policy-name self-stop \
  --policy-document "{\"Version\":\"2012-10-17\",\"Statement\":[{\"Effect\":\"Allow\",\"Action\":\"ec2:StopInstances\",\"Resource\":\"arn:aws:ec2:${AWS_REGION_NAME}:${ACCOUNT_ID}:instance/*\",\"Condition\":{\"StringEquals\":{\"ec2:ResourceTag/Name\":\"${BOX_NAME}\"}}}]}" \
  >/dev/null

PROFILE_EXISTS="$(aws_query iam get-instance-profile --instance-profile-name "$BOX_NAME" \
  --query 'InstanceProfile.InstanceProfileName' 2>/dev/null || true)"
[ "$PROFILE_EXISTS" = "None" ] && PROFILE_EXISTS=""
if [ -z "$PROFILE_EXISTS" ]; then
  log "creating instance profile ${BOX_NAME}"
  aws_do iam create-instance-profile --instance-profile-name "$BOX_NAME" >/dev/null
  aws_do iam add-role-to-instance-profile --instance-profile-name "$BOX_NAME" \
    --role-name "$ROLE_NAME" >/dev/null
  [ "$DRY_RUN" = 1 ] || sleep 12  # IAM instance profiles propagate slowly
fi

# ── instance ─────────────────────────────────────────────────────────────────
INSTANCE_ID="$(aws_query ec2 describe-instances \
  --filters "Name=tag:Name,Values=${BOX_NAME}" \
    'Name=instance-state-name,Values=pending,running,stopping,stopped' \
  --query 'Reservations[0].Instances[0].InstanceId' 2>/dev/null || true)"
[ "$INSTANCE_ID" = "None" ] && INSTANCE_ID=""
CREATED=0
if [ -z "$INSTANCE_ID" ]; then
  AMI_ID="$(aws_query ssm get-parameter --name "$AMI_SSM_PARAM" --query Parameter.Value || true)"
  AMI_ID="${AMI_ID:-ami-0dryrun}"
  log "launching ${INSTANCE_TYPE} from ${AMI_ID}"
  INSTANCE_ID="$(aws_do ec2 run-instances \
    --image-id "$AMI_ID" \
    --instance-type "$INSTANCE_TYPE" \
    --key-name "$BOX_NAME" \
    --security-group-ids "$SG_ID" \
    --iam-instance-profile "Name=${BOX_NAME}" \
    --instance-initiated-shutdown-behavior stop \
    --metadata-options 'HttpTokens=required,HttpEndpoint=enabled' \
    --block-device-mappings "DeviceName=/dev/sda1,Ebs={VolumeSize=${ROOT_VOLUME_GB},VolumeType=gp3,DeleteOnTermination=true,Encrypted=true}" \
    --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=${BOX_NAME}}]" \
      "ResourceType=volume,Tags=[{Key=Name,Value=${BOX_NAME}}]" \
    --user-data "file://${BOOTSTRAP}" \
    --query 'Instances[0].InstanceId' || true)"
  INSTANCE_ID="${INSTANCE_ID:-i-0dryrun0000000000}"
  CREATED=1
else
  log "instance ${INSTANCE_ID} exists"
fi

INSTANCE_ARN="arn:aws:ec2:${AWS_REGION_NAME}:${ACCOUNT_ID}:instance/${INSTANCE_ID}"

# Tighten the box's own policy to the exact instance now that the id is known.
aws_do iam put-role-policy --role-name "$ROLE_NAME" --policy-name self-stop \
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
RUNNER_EXISTS="$(aws_query iam get-user --user-name "$RUNNER_USER" --query 'User.UserName' 2>/dev/null || true)"
[ "$RUNNER_EXISTS" = "None" ] && RUNNER_EXISTS=""
if [ -z "$RUNNER_EXISTS" ]; then
  log "creating IAM user ${RUNNER_USER}"
  aws_do iam create-user --user-name "$RUNNER_USER" >/dev/null
fi
# ec2:Describe* cannot be resource-scoped (AWS does not support it); the two
# mutating actions are pinned to this one instance ARN.
aws_do iam put-user-policy --user-name "$RUNNER_USER" --policy-name "${BOX_NAME}-control" \
  --policy-document "{\"Version\":\"2012-10-17\",\"Statement\":[{\"Sid\":\"ControlThisBox\",\"Effect\":\"Allow\",\"Action\":[\"ec2:StartInstances\",\"ec2:StopInstances\"],\"Resource\":\"${INSTANCE_ARN}\"},{\"Sid\":\"DescribeIsNotResourceScopable\",\"Effect\":\"Allow\",\"Action\":[\"ec2:DescribeInstances\",\"ec2:DescribeInstanceStatus\"],\"Resource\":\"*\"}]}" \
  >/dev/null

EXISTING_KEYS="$(aws_query iam list-access-keys --user-name "$RUNNER_USER" \
  --query 'AccessKeyMetadata[?Status==`Active`].AccessKeyId' 2>/dev/null || true)"
[ "$EXISTING_KEYS" = "None" ] && EXISTING_KEYS=""
if [ -n "$EXISTING_KEYS" ]; then
  log "${RUNNER_USER} already has an active access key (${EXISTING_KEYS}); not creating another"
  log "if you no longer hold its secret, delete the key and re-run this script"
else
  CRED_FILE="${BACKUP_DIR}/${RUNNER_USER}-$(date -u +%Y%m%dT%H%M%SZ).env"
  log "creating one access key for ${RUNNER_USER}"
  if [ "$DRY_RUN" = 1 ]; then
    aws_do iam create-access-key --user-name "$RUNNER_USER" \
      --query 'AccessKey.[AccessKeyId,SecretAccessKey]'
    printf 'DRY-RUN: write %s (0600)\n' "${BACKUP_DIR}/${RUNNER_USER}-<ts>.env" >&3
  else
    mkdir -p "$BACKUP_DIR"; chmod 700 "$BACKUP_DIR"
    key_line="$("${AWS[@]}" iam create-access-key --user-name "$RUNNER_USER" \
      --query 'AccessKey.[AccessKeyId,SecretAccessKey]')"
    key_id="$(printf '%s' "$key_line" | awk '{print $1}')"
    ( umask 077 && printf 'AWS_ACCESS_KEY_ID=%s\nAWS_SECRET_ACCESS_KEY=%s\n' \
        "$key_id" "$(printf '%s' "$key_line" | awk '{print $2}')" > "$CRED_FILE" )
    chmod 600 "$CRED_FILE"
    cat <<EOF

  The secret is written once, to ${CRED_FILE} (0600). It is NOT printed here.
  Store it in ZS Vault, then delete that file:

    zsvault add aws_ci_runner --env-file "${CRED_FILE}"

  If your zsvault build has no --env-file flag, run \`zsvault add aws_ci_runner\`
  and paste the two lines from that file. The vault entry MUST export the names
  AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY, because remote-ci.sh reads those.
  Access key id (not secret): ${key_id}

EOF
  fi
fi

# ── wait for cloud-init, then stop ───────────────────────────────────────────
PUBLIC_IP=""
if [ "$CREATED" = 1 ] && [ "$WAIT_BOOTSTRAP" = 1 ] && [ "$DRY_RUN" = 0 ]; then
  log "waiting for the instance to run"
  "${AWS[@]}" ec2 wait instance-running --instance-ids "$INSTANCE_ID"
  PUBLIC_IP="$("${AWS[@]}" ec2 describe-instances --instance-ids "$INSTANCE_ID" \
    --query 'Reservations[0].Instances[0].PublicIpAddress')"
  log "waiting up to ${BOOTSTRAP_WAIT}s for cloud-init (ssh ${SSH_USER}@${PUBLIC_IP})"
  deadline=$(( $(date +%s) + BOOTSTRAP_WAIT ))
  until ssh -o StrictHostKeyChecking=accept-new -o ConnectTimeout=10 \
      -i "$KEY_PATH" "ubuntu@${PUBLIC_IP}" \
      'test -f /var/lib/buzz-ci-bootstrap-done' 2>/dev/null; do
    if [ "$(date +%s)" -ge "$deadline" ]; then
      log "bootstrap did not finish in time; check /var/log/buzz-bootstrap.log on the box"
      break
    fi
    sleep 20
  done
fi

if [ "$DRY_RUN" = 1 ] || [ "$("${AWS[@]}" ec2 describe-instances --instance-ids "$INSTANCE_ID" \
    --query 'Reservations[0].Instances[0].State.Name' 2>/dev/null || echo running)" != "stopped" ]; then
  log "stopping ${INSTANCE_ID}"
  aws_do ec2 stop-instances --instance-ids "$INSTANCE_ID" >/dev/null
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
log "next: scripts/zs/remote-ci.sh <branch>"
