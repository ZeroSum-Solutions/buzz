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

**One box, one run.** The box is a single mutable working tree, so two runs would check out
different commits into it and one EXIT trap would stop the box under the other. Every run takes
`flock -n` on `/home/ci/.remote-ci.lock` for its whole duration and records the caller's
hostname, pid, branch and start time there. A run that finds the box already `running` or
`pending` refuses and prints that lock holder, unless `--join` is passed; a run that cannot take
the lock does nothing and leaves the box alone. Only the run that owned the lock (or that
started the box itself) stops it. A box in `stopping` is waited out, bounded at 5 minutes,
before `StartInstances` — AWS rejects a start during `stopping`, and that state is the normal
one right after the previous run.

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

### Runbook

1. **Put the admin credential in ZS Vault** (once, by Devin). Two entries, so the script can map
   them into its own `aws` child processes without writing anything to `~/.aws`:
   ```
   zsvault add aws_admin_access_key_id     --type api_key --env-name AWS_ADMIN_ACCESS_KEY_ID
   zsvault add aws_admin_secret_access_key --type api_key --env-name AWS_ADMIN_SECRET_ACCESS_KEY
   ```
   `provision.sh` reads those two env names and passes them to `aws` as child-process
   environment only — never exported, never printed, never written to disk. If they are unset it
   falls back to `AWS_PROFILE` (default `zs-admin`), so an SSO or assume-role profile also works.
2. **Dry-run, then provision.**
   ```
   scripts/zs/remote-ci/provision.sh --dry-run    # prints every aws call, changes nothing
   scripts/zs/remote-ci/provision.sh              # ~30 min: it waits for cloud-init, then stops the box
   ```
   It writes `~/.ssh/buzz-ci-box.pem` (0600), `scripts/zs/remote-ci/box.env`, and one access-key
   file under `~/Backups/aws/` (0600). The secret is written to that file once and never printed.
3. **Store the runner key**, using the two commands the script prints (it fills in the real file
   path), then delete the file:
   ```
   sed -n 's/^AWS_ACCESS_KEY_ID=//p' <file> \
     | zsvault add aws_ci_runner_access_key_id --type api_key \
         --env-name AWS_ACCESS_KEY_ID --yes --value-stdin
   sed -n 's/^AWS_SECRET_ACCESS_KEY=//p' <file> \
     | zsvault add aws_ci_runner_secret_access_key --type api_key \
         --env-name AWS_SECRET_ACCESS_KEY --yes --value-stdin
   rm -f <file>
   ```
   The env names must be exactly `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`; a named
   profile `buzz-ci-runner` with the same credentials works too — `remote-ci.sh` accepts either.
4. **Deactivate the admin access key in IAM.** Only `provision.sh` ever needs it, and the
   day-to-day lane uses the scoped runner key. Reactivate it if you re-provision.
5. **First run.** `scripts/zs/remote-ci.sh zs/main` (or a branch, or named targets:
   `scripts/zs/remote-ci.sh my-branch desktop-test desktop-tauri-test`).
6. **After a network change**, refresh the SSH allow-list: `scripts/zs/remote-ci/provision.sh
   --allow-ip`. The security group opens port 22 to the caller's public IP only.

Verification available now, with no AWS account: `scripts/zs/remote-ci/test-remote-ci.sh` (66
checks). It shellchecks all four scripts, runs `provision.sh --dry-run` against a stub `aws` and
asserts the whole plan (instance type, 200 GB volume, client token, owner tag, alarm, both IAM
policies, the final stop) while asserting the real `aws` was never called, and drives each new
guard to its failure: two owner-tagged instances abort with no launch, a denied
`describe-instances` aborts with no launch, and a `running` box refuses a second run without
starting or stopping anything. It also exercises `--help` and `box.env` parsing, including that
a hostile line in `box.env` is rejected rather than executed.

### What Devin still has to do

Three things, all credential work no agent may do:

1. Put the AWS admin key in ZS Vault as `aws_admin_access_key_id` and
   `aws_admin_secret_access_key` (billing lane: AWS, approved 2026-09-05).
2. Run `provision.sh`, then store the runner key it creates as the two vault entries above.
3. Deactivate the admin access key in IAM once provisioning has succeeded.

Until the first two are done, `box.env` does not exist and `remote-ci.sh` refuses with a message
that says so. Nothing else in the repo depends on it.

Hetzner (AX or CCX line) does the same job for a flat $40 to $60 per month with no start/stop
logic, but it is a second new vendor and Devin approved AWS specifically; revisit if the
EC2 bill passes that figure.
