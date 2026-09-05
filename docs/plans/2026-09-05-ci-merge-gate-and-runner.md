# Merge gate time, local gate contention, and an on-demand test box

Date: 2026-09-05. Status: steps 1 and 2 implemented in PR `ci/merge-gate-time`; step 3 approved
in principle (AWS added to the billing lanes 2026-09-05) and gated on the measurement below.

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

## Step 3: an on-demand Linux test box (approved lane, not yet built)

Trigger: after steps 1 and 2 have run for a week, pushes still wait on the Mac (a builder
reports more than 10 minutes in gate-lock waits per ticket) or the queue still exceeds 20
minutes per entry. If neither is true, do not build this.

Shape, when triggered:
- One EC2 instance (c7a.8xlarge or c7g.8xlarge, 32 vCPU) that is stopped between uses, with a
  persistent 200 GB gp3 volume holding the repo clone, `~/.cargo`, the cargo `target` dirs, the
  pnpm store and the Playwright browsers. Ubuntu 24.04, the same apt packages as
  `_ci-desktop.yml`, hermit bootstrapped once.
- `scripts/zs/remote-ci.sh <branch>`: `aws ec2 start-instances`, wait for SSH, `git fetch` and
  check out the branch, run `just ci` (or the named gates), stream the log, copy
  `playwright-report` back, then `aws ec2 stop-instances`. Exit code is the gate's.
- Hard stop: a CloudWatch alarm on the instance, `CPUUtilization < 5% for 30 min` → stop, plus a
  cron on the box that stops it after 90 minutes of uptime whatever happens. Both exist before
  the first run.
- Access: one IAM user limited to `ec2:StartInstances`, `ec2:StopInstances`,
  `ec2:DescribeInstances` on that instance id, key stored with `zsvault add aws_ci_runner`.
  No self-hosted GitHub runner registration on this box.
- Cost: about $1.20 per hour while running, $16 per month for the stopped volume, so about
  $1 per full gate run. Blacksmith stays the merge gate; this box is only the pre-push lane.

Hetzner (AX or CCX line) does the same job for a flat $40 to $60 per month with no start/stop
logic, but it is a second new vendor and Devin approved AWS specifically; revisit if the
EC2 bill passes that figure.
