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
the branch out on the box (`git fetch origin && git checkout --detach origin/<branch>`), runs
`just <targets>` under `script -q` so the log streams to the terminal and to
`~/Inbox/notes/remote-ci-<branch>-<ts>.log`, copies `desktop/playwright-report` back to
`~/Inbox/misc/remote-ci-report-<branch>-<ts>/` when it exists, and stops the instance from an
EXIT trap whatever happened. It exits with the gate's own exit code, which the runner reports
back through a sentinel line because `script` does not pass a remote status through portably.
`--push-local` sends a not-yet-pushed branch's **committed** tree as a `git bundle` over ssh
(incremental against `origin/zs/main` when it can, full history otherwise). `--keep` skips the
stop. `--status` prints the state and uptime and touches nothing. `--help` documents all of it.

### Cost guardrails as implemented

- The instance is created stopped and every run stops it again from an EXIT trap, so `--keep`
  or a killed terminal is the only way it stays up.
- CloudWatch alarm `buzz-ci-box-idle`: `CPUUtilization` average `< 5%` over six 5-minute periods
  (30 minutes) fires `arn:aws:automate:<region>:ec2:stop`. Created by `provision.sh` in the same
  run as the instance, so it exists before the first gate.
- `/etc/cron.d/buzz-ci-uptime-stop` runs `/usr/local/sbin/buzz-ci-uptime-guard` every minute; at
  90 minutes of uptime it runs `shutdown -h now`. The instance is launched with
  `--instance-initiated-shutdown-behavior stop`, so that stops rather than terminates.
- Least privilege on both sides. The box's own instance role may call `ec2:StopInstances` on its
  own instance ARN and nothing else. The `buzz-ci-runner` IAM user may call `StartInstances` and
  `StopInstances` on that one instance ARN, plus `DescribeInstances`/`DescribeInstanceStatus` on
  `*` — AWS does not let `Describe*` be resource-scoped, and that is called out in the policy's
  own `Sid`. No self-hosted GitHub runner is registered on this box.
- Expected cost unchanged from the estimate: about $1.20 per hour running, about $16 per month
  for the stopped volume, so about $1 per full gate run.

### Runbook

1. **Create the admin profile** (once, by Devin). `aws configure --profile zs-admin` — region
   `us-east-1`, output `json`. This admin key is used only by `provision.sh` and is deliberately
   **not** stored in ZS Vault.
2. **Dry-run, then provision.**
   ```
   scripts/zs/remote-ci/provision.sh --dry-run    # prints every aws call, changes nothing
   scripts/zs/remote-ci/provision.sh              # ~30 min: it waits for cloud-init, then stops the box
   ```
   It writes `~/.ssh/buzz-ci-box.pem` (0600), `scripts/zs/remote-ci/box.env`, and one access-key
   file under `~/Backups/aws/` (0600). The secret is written to that file once and never printed.
3. **Store the runner key.** Run the `zsvault add aws_ci_runner` line the script prints, then
   delete the file under `~/Backups/aws/`. The vault entry must export exactly
   `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`, or else configure a named profile
   `buzz-ci-runner` with the same credentials — `remote-ci.sh` accepts either.
4. **First run.** `scripts/zs/remote-ci.sh zs/main` (or a branch, or named targets:
   `scripts/zs/remote-ci.sh my-branch desktop-test desktop-tauri-test`).
5. **After a network change**, refresh the SSH allow-list: `scripts/zs/remote-ci/provision.sh
   --allow-ip`. The security group opens port 22 to the caller's public IP only.

Verification available now, with no AWS account: `scripts/zs/remote-ci/test-remote-ci.sh`
shellchecks all four scripts, runs `provision.sh --dry-run` against a stub `aws` and asserts the
whole plan (instance type, 200 GB volume, alarm, both IAM policies, the final stop) while
asserting the real `aws` was never called, exercises `--help`, and checks `box.env` parsing
including that a hostile line in it is rejected rather than executed.

### What Devin still has to do

Two things, both credential work no agent may do:

1. `aws configure --profile zs-admin` with an admin key (billing lane: AWS, approved 2026-09-05).
2. Run `provision.sh`, then `zsvault add aws_ci_runner` with the key it creates.

Until both are done, `box.env` does not exist and `remote-ci.sh` refuses with a message that
says so. Nothing else in the repo depends on it.

Hetzner (AX or CCX line) does the same job for a flat $40 to $60 per month with no start/stop
logic, but it is a second new vendor and Devin approved AWS specifically; revisit if the
EC2 bill passes that figure.
