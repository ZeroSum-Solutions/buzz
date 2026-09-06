# Merge gate time, local gate contention, and an on-demand test box

Date: 2026-09-05. Status: steps 1 and 2 implemented in PR `ci/merge-gate-time`; step 3 built in PR
`ci/remote-test-box` and waiting only on Devin creating the AWS profile (see Step 3).

## What was measured (zs/main, 2026-09-04 to 2026-09-05)

| Lane | Runs | Failed | Cause of every failure |
|---|---|---|---|
| Merge queue (`merge_group`) | 15 | 3 | 2 × apt dropout in Playwright `install-deps` (before PR #10 added the retry); 1 × hermit bootstrap download hung 13 min then reset (`curl: (56)`), plus `persistent-agent-audience.spec.ts:537` failing 3 of 3 |
| Post-merge push CI | 8 | 3 | `cancelled_turn_with_usage_emits_notification_before_response` (nextest retry added since); `huddle-transcription.spec.ts` voice menu 15 s timeout; `messaging.spec.ts:2439` root-thread link |

Wall clock per queue entry: 17 to 28 minutes, one entry at a time (`max_entries_to_build: 1`).
Critical path of a green run: Desktop Core 28 min (compiled-flag verification 15, unit tests 5,
Tauri tests 5), smoke e2e shards 15 to 21 min (one Playwright worker each), Windows Rust 15,
Unit Tests 13, macOS build 8. No real regression was found in any failed run.

## Step 1: shorten and stabilise the merge gate (done in this PR)

1. `cashapp/activate-hermit` with `cache: "true"` in every reusable CI workflow. The action
   caches `~/.cache/hermit/pkg`, which holds the bootstrap binary, so the release download that
   hung is skipped on a cache hit.
2. `Desktop Tauri compiled-flag verification` moved from Desktop Core to its own job,
   `Desktop Compiled Flags`, sharing Desktop Core's `rust-cache` key. Desktop Core should drop
   to about 13 minutes.
3. Smoke e2e: eight shards instead of four. Each shard should take about half the time; the
   total runner minutes stay roughly the same.
4. Kept: macOS and Windows jobs. They are required checks, they catch platform breaks (PR 14's
   Windows `unsafe`), and they were never the long pole.

Expected: about 15 minutes per queue entry instead of 17 to 28. Verify on the next five queue
runs with `gh run list --event merge_group` and the job durations in `gh run view --json jobs`.

Measured after PR #20 landed (queue runs 33990765603 for #20 and 33992052113 for #21):
entries took 27 and 24 minutes. Desktop Core 9 min, Desktop Compiled Flags 10 to 11 min, smoke
shards 6 to 10 min, Unit Tests 9 min: step 1 held. The long pole moved to the two GitHub-hosted
jobs, which the first measurement under-reported: Windows Rust 26 and 23 min (the rust-cache
save alone 4 to 6 min per entry, and it repeats even when Cargo.lock is unchanged) and Desktop
Build (macOS) 20 and 9 min (`cargo tauri build` 18 min on the slow run).

### Step 1b: move those two jobs to Blacksmith (PR `ci/blacksmith-mac-win`)

`Desktop Build (macOS)` on `blacksmith-6vcpu-macos-latest` (M4 Pro) and `Windows Rust` on
`blacksmith-8vcpu-windows-2025` (public beta). On that PR's own run (33993211509): macOS job
7.5 min (`cargo tauri build` 5.6 min), Windows job 8.5 min (Tauri crate test 2.8 min, cache
restore 14 s, no save on a pull_request run). Both were picked up within a minute. Expected
queue entry after this lands: bounded by Desktop Compiled Flags at about 11 minutes, plus the
Windows cache save on merge_group runs. Watch the first two queue entries; if the Windows beta
runner fails to pick a job up, revert that one label.

Still open after this PR: the three flaky specs above. Rerun and name them in the PR body until
someone fixes the wait conditions. Option not taken yet: `max_entries_to_build` 2 or 3 with
`ALLGREEN` grouping lets several PRs share one CI run; try it once the run itself is stable.

## Step 2: serialise heavy local gates on the Mac (done in this PR)

`scripts/zs/with-gate-lock.sh <command>` takes one exclusive lock per machine
(`~/.cache/zs/buzz-gate.lock`), waits up to 45 minutes, runs the command, and passes its exit
code through. Builders and testers wrap `just desktop-test`, `just desktop-tauri-test`,
`just desktop-tauri-test-compiled-flags` and every `cargo test` / `cargo nextest run` in it.
Fast gates stay unlocked. The rule is in the implementation plan's operational notes so every
workflow prompt that reads those notes picks it up.

## Step 3: an on-demand Linux test box — built, not yet provisioned

Status: the scripts are written, shellchecked and tested offline. No AWS resource exists yet,
because no AWS profile exists on this machine yet. See "What Devin still has to do" below.

Decision, 2026-09-05: Devin decided to build the box now and skip the trigger condition this
document originally set (a week of measurement first). Queue entries were measured at 25 to 27
minutes, and he wants the Mac offloaded. The trigger is therefore spent; do not wait on it.

### What was built

| File | What it does |
|---|---|
| `scripts/zs/remote-ci/provision.sh` | One-time, idempotent creation of every AWS resource. `--dry-run` prints every `aws` call and touches nothing. |
| `scripts/zs/remote-ci/bootstrap.sh` | cloud-init user-data for the instance. Logs to `/var/log/buzz-bootstrap.log`. |
| `scripts/zs/remote-ci.sh` | The lane a builder actually runs: `scripts/zs/remote-ci.sh <branch> [just targets...]`. |
| `scripts/zs/remote-ci/test-remote-ci.sh` | The gate for all three, with a stub `aws` on PATH. Needs no AWS account. |
| `scripts/zs/remote-ci/box.env` | Written by `provision.sh`, read by `remote-ci.sh`. Gitignored. |

The instance: one `c7a.8xlarge` (32 vCPU x86_64 — matching CI's architecture, so a Rust
failure here means the same thing it means on Blacksmith), Ubuntu 24.04 from the SSM parameter
`/aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id`, a 200 GB
encrypted gp3 root volume, tag `Name=buzz-ci-box`. The root volume is the persistent volume: it
holds the clone at `/home/ci/buzz`, `~/.cargo`, `~/.rustup`, the cargo `target` dirs, the pnpm
store and the Playwright browsers, so a stop/start keeps every cache.

`bootstrap.sh` installs the same apt packages as `_ci-desktop.yml` (with the same five-attempt
retry loop, because the mirror dropouts are not specific to Blacksmith) plus `mold`, `git` and
`pkg-config`; creates the `ci` user and copies the launch key to it; clones
`https://github.com/ZeroSum-Solutions/buzz.git` at `zs/main` (the fork is public — checked with
`gh repo view ZeroSum-Solutions/buzz --json isPrivate`; the header comment carries the
read-only-deploy-key procedure to use if that ever changes, and no token ever goes in
user-data); bootstraps hermit; installs `cargo-nextest` the way `_ci-rust.yml` does; runs
`just desktop-install`, `playwright install chromium` and `install-deps`; and then warms the
build with `just desktop-build` and a full `cargo build --workspace --all-targets` of the
desktop crate, so the first real run is not a cold compile.

`remote-ci.sh` starts the instance, waits for `running` and then for SSH (5-minute cap), checks
the branch out on the box (`git fetch origin && git checkout --detach origin/<branch>`),
**reinstalls the branch's dependencies** (`just desktop-install-ci` plus the branch's Playwright
Chromium — the box was bootstrapped from `zs/main` and `just ci` installs nothing, so without
this a branch that touches `pnpm-lock.yaml` would be gated against the wrong packages), runs
`just <targets>` under `script -q` so the log streams to the terminal and to
`~/Inbox/notes/remote-ci-<branch>-<ts>.log`, copies `desktop/playwright-report` back to
`~/Inbox/misc/remote-ci-report-<branch>-<ts>/` when it exists, and stops the instance from an
EXIT trap whatever happened. It exits with the gate's own exit code, which the runner reports
back through a sentinel line because `script` does not pass a remote status through portably.
`--push-local` sends a not-yet-pushed branch's **committed** tree as a `git bundle` over ssh
(incremental against `origin/zs/main` when it can, full history otherwise). `--keep` skips the
stop. `--status` prints the state, uptime and current lock holder and touches nothing.
`--help` documents all of it.

**One box, one run.** The box is a single mutable working tree and a single billable instance,
so two runs would check out different commits into it and one EXIT trap would stop the box under
the other. Ownership is a **local lease**, taken before the first EC2 call and held for the
whole run: an exclusive `flock` on `~/.cache/zs/buzz-remote-ci.lock`, the same mechanism
`scripts/zs/with-gate-lock.sh` uses for the local gates, with the holder's pid, branch and start
time in a sidecar file for the refusal message. Only the lease holder starts or stops the box.
A second `flock -n` on `/home/ci/.remote-ci.lock` guards the box itself. Every caller runs on
this Mac, so a local lease is the whole population — **the box belongs to one machine**; a
second machine would not see this lease and needs its own box.

A run that finds the box already `running` or `pending` while holding the lease refuses, prints
the on-box lock holder, and points at `--join`: that state means someone started it by hand,
used `--keep`, or a stop failed. A box in `stopping` is waited out, bounded at 5 minutes, before
`StartInstances` — AWS rejects a start during `stopping`, and that state is the normal one right
after the previous run.

**The checkout is reset, not merely updated.** A previous gate can modify a tracked file or drop
untracked sources into the shared tree, and `git checkout` keeps both. Each run resets hard to
the target and removes untracked files while keeping ignored ones (`node_modules`, the cargo
`target` dirs and the Playwright browsers are the point of the persistent volume), then asserts
`git status --porcelain` is empty and aborts rather than gate a mixed tree.

### Cost guardrails as implemented

- The instance is created stopped and every run stops it again from an EXIT trap, so `--keep`
  or a killed terminal is the only way it stays up. `provision.sh` arms its own stop trap
  **before** `run-instances`, so an interruption between launch and the final stop still stops
  the box; the launch carries a stable `--client-token`, so a retry adopts that instance rather
  than launching a second $1.20-per-hour box.
- CloudWatch alarm `buzz-ci-box-idle`: `CPUUtilization` average `< 5%` over six 5-minute periods
  (30 minutes) fires `arn:aws:automate:<region>:ec2:stop`. Created by `provision.sh` in the same
  run as the instance, so it exists before the first gate.
- `/etc/cron.d/buzz-ci-uptime-stop` runs `/usr/local/sbin/buzz-ci-uptime-guard` every minute; at
  90 minutes of uptime it runs `shutdown -h now`. The instance is launched with
  `--instance-initiated-shutdown-behavior stop`, so that stops rather than terminates. Bootstrap
  **fails** unless the cron service is active: a hung high-CPU process never trips the idle
  alarm, so this guard is the only one that bounds the bill in that case.
- Least privilege on both sides. The box's own instance role may call `ec2:StopInstances` on its
  own instance ARN and nothing else. The `buzz-ci-runner` IAM user may call `StartInstances` and
  `StopInstances` on that one instance ARN, plus `DescribeInstances`/`DescribeInstanceStatus` on
  `*` — AWS does not let `Describe*` be resource-scoped, and that is called out in the policy's
  own `Sid`. No self-hosted GitHub runner is registered on this box.
- The `ci` user has **no sudo**. Branch code runs as `ci`, and root on that box could disable
  the uptime guard, poison the persistent caches, or read instance metadata. The one privileged
  step, `playwright install-deps`, runs as root during bootstrap only.
- The gate itself is bounded: it runs under `timeout 5400` on the box, the same 90 minutes as
  the uptime guard, so a hung target cannot ride the instance to the shutdown. The local log is
  capped at 200 MB (the head is trimmed, the tail kept) and a Playwright report over 500 MB is
  left on the box rather than copied, so a runaway gate cannot fill this Mac's disk either.
- Expected cost unchanged from the estimate: about $1.20 per hour running, about $16 per month
  for the stopped volume, so about $1 per full gate run.

### Failing closed

`provision.sh` never turns a failure into a successful provision:

- A `describe` that errors (denied, throttled) is an error, never "the resource does not exist".
  IAM's `NoSuchEntity` is the one "missing" signal it accepts, and only that one.
- The instance is found by **both** `Name=buzz-ci-box` and `zs:owner=buzz-remote-ci`. EC2 tags
  are not unique, so more than one match aborts instead of picking one.
- An existing role, instance profile or runner user is adopted only if it carries the owner tag
  and holds no policy, group or profile membership this script did not create. Anything else
  aborts: a colliding role would otherwise hand branch code whatever that role can do.
- A box whose `/var/lib/buzz-ci-bootstrap-done` marker is missing is never adopted silently. On
  a fresh launch a missing marker stops the box and exits non-zero; on a re-run the script
  starts the box, re-runs `bootstrap.sh` over ssh, and fails if the marker still does not
  appear. `--no-verify` skips that check when you only want to refresh metadata.
- `bootstrap.sh` removes any old marker first and writes the new one only after every step has
  succeeded, so a failed repair cannot leave a stale "this box is fine" marker behind.
- The security group is adopted only if it carries the owner tag, and every ingress rule that is
  not the one SSH rule is revoked. A colliding group permitting a test port from `0.0.0.0/0`
  would otherwise survive a refresh that only touches port 22.
- An adopted role must carry the exact EC2-only trust policy (one statement, `ec2.amazonaws.com`,
  `sts:AssumeRole`, no other principal and no condition) and belong to our instance profile and
  no other. Policy names alone do not say who may assume a role.
- `provision.sh` turns `xtrace` off before the first credential-bearing expansion and never
  turns it back on, so `bash -x` cannot put the admin key or the runner secret into a redirected
  log. An access key that exists but is not yet in the vault is a cleanup obligation the
  EXIT/INT/TERM trap discharges, so a Ctrl-C mid-handoff revokes the key rather than leaving an
  active credential nobody holds the secret for. A signal exits non-zero, never 0.
- A launch that returns no instance id is resolved by asking EC2 for the instance carrying the
  client token and owner tag; a placeholder id is never recorded.
- The Playwright report comes back through a bounded stream (`tar` over ssh with a deadline, cut
  at the cap) rather than a preflight size check a still-writing gate could beat, and a probe
  that fails is reported as a probe failure, not as "no report".
- A stop that fails is retried five times with backoff and then reported loudly, naming the
  instance id and the exact `aws ec2 stop-instances` command. It is never a warning, and it
  makes the run exit non-zero even when the gate itself was green. If `run-instances` succeeded
  but the id was lost, the instance is looked up again by its client token and owner tag before
  the script gives up — and if it still cannot be found, that too is a loud non-zero exit.

### Runbook

1. **Put the admin credential in ZS Vault** (once, by Devin). Two entries, so the script can map
   them into its own `aws` child processes without writing anything to `~/.aws`:
   ```
   zsvault add aws_buzz_ci_admin_key_id --type api_key --env-name ZS_AWS_BUZZ_CI_ADMIN_KEY_ID
   zsvault add aws_buzz_ci_admin_secret --type api_key --env-name ZS_AWS_BUZZ_CI_ADMIN_SECRET
   ```
   `provision.sh` reads those two env names and passes them to `aws` as child-process
   environment only — never exported, never printed, never written to disk. If they are unset it
   falls back to `AWS_PROFILE` (default `buzz-ci-admin`, which must exist; the root login profile `zerosum` is never used implicitly), so an SSO or assume-role profile also works.
2. **Dry-run, then provision.**
   ```
   scripts/zs/remote-ci/provision.sh --dry-run    # prints every aws call, changes nothing
   scripts/zs/remote-ci/provision.sh              # ~30 min: it waits for cloud-init, then stops the box
   ```
   It writes `~/.ssh/buzz-ci-box.pem` (0600) and `scripts/zs/remote-ci/box.env`. Once the box
   has proved it bootstrapped, it creates the scoped runner access key and hands both halves
   straight to ZS Vault as `aws_buzz_ci_runner_key_id` (env `ZS_AWS_BUZZ_CI_RUNNER_KEY_ID`) and
   `aws_buzz_ci_runner_secret` (env `ZS_AWS_BUZZ_CI_RUNNER_SECRET`). **The runner secret never
   touches the disk and is never printed**; if either vault write fails, the IAM key is deleted
   again and provisioning exits non-zero. `zsvault` must therefore be on `PATH` — the script
   checks before it creates the key. A named profile `buzz-ci-runner` with the same credentials
   works too; `remote-ci.sh` accepts either.
3. **Deactivate the admin access key in IAM.** Only `provision.sh` ever needs it, and the
   day-to-day lane uses the scoped runner key. Reactivate it if you re-provision.
4. **First run.** `scripts/zs/remote-ci.sh zs/main` (or a branch, or named targets:
   `scripts/zs/remote-ci.sh my-branch desktop-test desktop-tauri-test`).
5. **After a network change**, refresh the SSH allow-list: `scripts/zs/remote-ci/provision.sh
   --allow-ip`. The security group opens port 22 to the caller's public IP only.

Verification available now, with no AWS account: `scripts/zs/remote-ci/test-remote-ci.sh` (119
checks). It shellchecks all four scripts, runs `provision.sh --dry-run` against a stub `aws` and
asserts the whole plan (instance type, 200 GB volume, client token, owner tag, alarm, both IAM
policies, the final stop) while asserting the real `aws` was never called. Each guard is driven
to its failure: two owner-tagged instances abort with no launch; a denied `describe-instances`
aborts with no launch; a security group without the owner tag aborts; a failing `zsvault` write
makes the script delete the IAM access key again and exit non-zero; a held lease refuses the run
before any EC2 call; a `running` box refuses a run without `--join`; a stop that keeps failing
is retried five times and then names the instance loudly; and the generated remote runner,
executed against stubbed tools, aborts rather than gate a tree that is still dirty after its
reset. It also exercises `--help` and `box.env` parsing, including that a hostile line in
`box.env` is rejected rather than executed. Each of those guards was verified falsifiable by
removing it and watching the matching check fail.

### What Devin still has to do

Three things, all credential work no agent may do:

Account facts (2026-09-06): shared account 767866852083, region us-west-2 next to the MishMash box.
`buzz-ci-admin` exists with an inline policy scoped to EC2 provisioning, IAM on `buzz-ci-*` names,
the `buzz-ci-*` CloudWatch alarm and the canonical-AMI SSM parameter; its key is in ZS Vault. The
account's on-demand Standard vCPU quota (L-1216C47A) was 16 and fully used by the running MishMash
box, so the first launch failed with VcpuLimitExceeded; a quota case for 32 was already open from
the MishMash setup and AWS allows one open case per quota. Until it lands, provision with
`BUZZ_CI_INSTANCE_TYPE=c7a.4xlarge` (16 vCPU) and resize later with
`ec2 modify-instance-attribute --instance-type` while stopped; request 96 once the 32 case closes.
Security group, key pair, role and instance profile from the first attempt are tagged and adopted
on rerun.

First real run (2026-09-06, Buzz account 702617649747, box i-0ef87bf1645fb35ba as c7a.2xlarge,
8 vCPU): `scripts/zs/remote-ci.sh ci/remote-test-box-account desktop-tauri-test` took 3 minutes
wall from `start-instances` to `stopped`, including ssh wait and the cargo test build reusing the
bootstrap's cache (52 s); the Tauri crate's 3,408 tests ran in 81 s; lane total 3,509 passed,
0 failed, 22 ignored. On the Mac the same gate takes 2 to 4 minutes when idle and much longer
under gauntlet load, so at 8 vCPUs the box already offloads it; the 32-vCPU size is for the
full pre-push suite and parallel gauntlets once the quota case lands.

1. Put the AWS admin key in ZS Vault as `aws_buzz_ci_admin_key_id` and
   `aws_buzz_ci_admin_secret` (billing lane: AWS, approved 2026-09-05).
2. Run `provision.sh`. It stores the runner key in ZS Vault itself; no manual key handling.
3. Deactivate the admin access key in IAM once provisioning has succeeded.

Until the first two are done, `box.env` does not exist and `remote-ci.sh` refuses with a message
that says so. Nothing else in the repo depends on it.

One more thing to know rather than do: **this box belongs to this Mac.** The lease that keeps
two runs from fighting over it is a local `flock`, so a second machine pointed at the same
`box.env` would not see it and could stop the box under a running gate. A second machine needs
its own box.

Hetzner (AX or CCX line) does the same job for a flat $40 to $60 per month with no start/stop
logic, but it is a second new vendor and Devin approved AWS specifically; revisit if the
EC2 bill passes that figure.
