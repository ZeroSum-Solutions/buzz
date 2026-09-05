# ZeroSum Buzz fork — throughput plan (what eats time, and the fix for each)

Date: 2026-09-05. Scope: the wave-1 execution loop defined in
`2026-09-04-zs-implementation-plan.md`, measured on its first run (7 tickets, 88 agents,
13.6 hours wall clock, 26 CI runs, 4 landings). Status: revision 2, for Devin's acceptance.
Audited by Grok 4.6 (pass 1 applied; pass 2 recorded at the end). Nothing here changes the
running loop until accepted.

## Measured baseline

Agent time by stage (minutes of agent work, all tickets; these are agent-minutes, not wall):

| Stage | Runs | Total min | Share | Avg per run |
|---|---|---|---|---|
| Sol audit fix rounds (`fix-audit`) | 12 | 790 | 31% | 66 (one run of 332; the other 11 average 42) |
| Sol audit (codex run + verification) | 19 | 606 | 23% | 32 |
| Builder (first build) | 7 | 274 | 11% | 39 |
| Critic (blind comparison) | 16 | 241 | 9% | 15 |
| Critic fix rounds | 9 | 238 | 9% | 26 |
| Gemini tester | 11 | 193 | 7% | 18 |
| Tester fix rounds | 7 | 154 | 6% | 22 |
| PR opener (rebase, push, PR body) | 7 | 91 | 4% | 13 (the local `just ci` inside it is machine time, about 30 min, not counted here) |

Totals: 2,587 agent-minutes (43 agent-hours) and 8.7 M output tokens for 7 tickets. Wall clock
per ticket: 3.9 h (T13), 4.3 h (T11), 4.9 h (T8), 5.9 h (T4), 9.4 h (T2), 9.9 h (T1), 13.6 h
(T5). The wave's wall clock is T5.

### Where the real defects came from (round and severity)

| Defect | Ticket | Found by | Round | Severity as verified | In original diff or in a fix commit |
|---|---|---|---|---|---|
| Byte-slice panic on MCP names over 128 bytes | T4 | Gemini | 1 | FAIL with repro | original |
| Prompt mapping persisted before the persona save | T5 | Gemini | 1 | FAIL with repro | original |
| Untrusted MCP server keeps hook authority | T4 | Sol | 1 | BLOCK | original |
| Credential withholding enforced only on one spawn path | T4 | Sol | 2 | BLOCK | fix commit |
| Three of four fatal tool-registration paths lack containment | T4 | driver, during Sol verification | 3 | BLOCK | fix commit |
| Markdown viewer complexity gate bounds bytes, not nodes | T2 | Sol | 1 | BLOCK | original |
| Replacement gate counts only newlines | T2 | Sol | 2 | BLOCK | fix commit |
| Replacement gate counts only `\n` and `[` | T2 | Sol | 3 | BLOCK | fix commit |
| Agent role line unbounded from untrusted kind-0 `about` | T1 | Sol | 2 | WARN | fix commit |
| Uncapped `about` in the batch DTO | T1 | Sol | 3 | WARN | original diff, found late |
| Degenerate 500 KB perf fixture; unnamed deviations | T2, T1 | critic | 1 | gap | original |

Three conclusions the rest of this plan rests on: of the six Sol BLOCK rows, four came in rounds
2 and 3 from fix commits (T4 twice, T2 twice), so any later pass must keep looking at the fix
diff; the unbounded-input class was graded WARN both times, and one of those was in the original
diff and found only in round 3, so a delta pass must also re-scan untrusted-input surfaces the
fix did not touch; and both Gemini defects were round-1 FAILs with a repro, so no later Gemini
round produced a listed defect.

### Rounds

Sol ran three times on every code ticket except T13 (once). Gemini ran three times on T13 and
T5, twice on T2 and T4, once on T1, never on the two docs tickets. The critic ran three times on
T2, T5, T8 and T11. The single 332-minute run was T5's first Sol fix round (2 UI BLOCKs and 6
WARNs across Rust, TypeScript and tests in one agent). The calendar memo (T11) had three Sol
passes inside the workflow and a fourth run by the driver, none converging; it is parked.

### CI

26 runs of the CI workflow: 8 on pushes to `zs/main` (7 of them my fork-only CI or docs
commits; 2 failed on the flaky Rust cancellation test), 18 on pull requests. Of the 18: 10
green, 5 cancelled because a newer push superseded them (all five were my own base re-merges
after a landing, not agent pushes; agents never pushed mid-loop), 3 failed (2 flaky smoke
specs, 1 the viewer's wall-clock benchmark). Per landed PR that is about 2.5 PR runs plus the
`zs/main` push run. Durations 28 to 68 minutes; the floor is the E2E shards, which failed on a
5 s expect budget, so they are wait-bound, not CPU-bound.

Local: `just ci` about 30 minutes; pre-push hook 2 to 10 minutes and one push blocked on a
flaky desktop test; seven worktrees building and testing at once on one machine produced every
local flake (the viewer benchmark, `useKnownAgentPubkeys`, the buzz-agent cancellation test all
pass in isolation).

Sol: `codex exec` at `xhigh` 10 to 20 minutes per pass; the external-process semaphore of two
was shared by codex and Gemini, so audits queued behind tests.

## Findings and fixes

### F1. The Sol audit loop is over half of all agent time (54%)

Cause, supported by the data: three full passes per ticket with a full fix round after each,
WARNs treated like BLOCKs, and one outlier fix round (T5, 332 min) that was a rewrite across
three layers. Not supported: that later rounds were harmless; four of the six Sol BLOCK rows
came in rounds 2 and 3 from fix commits.

Fix:
- One full pass, then a delta pass scoped to the fix diff, the prior findings, and a re-scan of
  every untrusted-input surface in the whole diff (relay-sourced strings, DTOs, file and env
  input) whether or not the fix touched it, because T1's DTO cap was in the original diff and
  surfaced only in round 3. The delta may raise new BLOCKs (that is where T4's and T2's later
  BLOCKs were). If it does, one more fix and one more delta. Three Sol runs at most, two of them
  deltas at roughly a third of the cost. A ticket with an open verified BLOCK does not land,
  whatever the round count; the driver may split work out, never wave a BLOCK through.
- Effort by files touched, not by ticket label: `xhigh` when the diff touches `crates/buzz-agent/src/mcp.rs`,
  `crates/buzz-acp/`, `managed_agents/runtime.rs`, spawn or env code, `secret_store`, keychain,
  relay crates, or any DTO that carries relay-sourced data; `high` otherwise.
- Severity rubric before anything is downgraded to a follow-up. BLOCK: unbounded untrusted
  input (the `about` class), missing containment or credential exposure, an unbounded resource,
  loop or process tree, a swallowed failure, torn multi-write state, a guard whose removal fails
  no test, and a complexity or resource gate that bounds the wrong quantity (the T2 class:
  bytes, then newlines, then two characters, never nodes). These are AGENTS.md Review-Proven
  Rules 1 to 4 restated as a gate. Everything else verified at WARN becomes a listed follow-up
  in the PR body and an issue on the fork.
- Cap the fix round: a fix agent that needs more than 90 minutes stops, commits what passes, and
  reports. Only WARN and out-of-scope work may be parked into a follow-up; an unfixed BLOCK
  keeps the ticket open and the driver splits the ticket, not the BLOCK.

Expected: audit plus fix-audit from 1,396 agent-minutes to 550 to 750 for the same seven
tickets (Grok's estimate; mine was 500 and assumed WARN-only later rounds, which the tags
disprove). The wall-clock gain from fewer `xhigh` runs on the semaphore is larger than the
agent-minute gain.

### F2. The critic loop adds 19%

Cause: the critic recursed on checklist items (deviations unnamed, counts unmet) three times on
four tickets. Its real finds were checklist failures, not "the bar is better".

Fix: memos get no critic. Ports keep one independent check, single pass, not blind: PR tests
present and passing, eval counts met, every deviation named against the port diff; a failed
check is one fix round, then done. Features (T5-class) keep one blind critic pass and at most
one fix round. The builder's own checklist in the PR body is in addition, never instead.

Expected: 479 agent-minutes to 90 to 130 for this mix (seven single passes plus one or two fix
rounds).

### F3. The Gemini tester loops on "missing tests"

Cause: a PASS with a missing-test list was treated as a fix round; both real Gemini defects
were round-1 FAILs.

Fix: the first pass always runs. FAIL with a repro means one fix and one retest, then the
driver. A missing-test note that describes an untested crash or race is a FAIL. Other
missing-test notes are follow-ups in the PR body.

Expected: 347 agent-minutes to 180 to 250.

### F4. Local `just ci` duplicates CI and starves the machine

Cause: the plan required `just ci` locally before every PR on top of full CI; seven worktrees
at once made every stage slower and produced the flakes; the PR stage hit the workflow time
limit twice while running it.

Fix: local gates are the fast set only (`fmt-check`, `clippy`, `desktop-check`,
`desktop-tauri-clippy`, `file-size-check`, the ticket's own test files). CI on Blacksmith is
the full gate. Fan-out cap of four builders at once. Two separate semaphores: two for auditors
(codex, Gemini) and two for heavy compile or test commands (`cargo test`, `desktop-test`, E2E);
never one shared, or builders queue behind Sol.

Expected: 25 to 40 minutes of wall time per ticket in the PR stage, and no more cut-off PR
stages; agent-minutes roughly unchanged; fewer local flakes.

### F5. CI runs

Already done today: merge queue on `zs/main`, overlap-aware landing rule, Blacksmith runners
for Linux jobs, smoke expect budget raised under CI, one nextest retry for the flaky
cancellation test, push CI on `zs/main` to keep caches warm, image and release workflows
disabled on the fork, repository auto-merge and auto-delete of merged branches.

Remaining:
- Open PRs as drafts; CI's `pull_request` jobs run only when the PR is not a draft
  (`if: github.event.pull_request.draft == false` on the `changes` job). Fix pushes and base
  merges on a draft cost nothing; marking ready runs CI once; the queue runs it again on the
  merged result. This is what removes the five cancelled runs and the re-merge runs.
- Flake policy: one automatic retry (the nextest override, Playwright's two CI retries), then
  quarantine or fix within three landings. No standing "rerun, don't diagnose" list.
- Bigger runners only after a profiled run shows a job is CPU-bound; the E2E shards are not.
- `max_entries_to_build: 2` only after three clean queue landings and only with the overlap
  rule in force.
- Drop `zs/main` from `ci.yml`'s push trigger once the queue is in use: the merge-group run
  already tested the exact merged commit and saves the caches (the cache steps save on any
  event that is not `pull_request`). Direct docs and CI commits to `zs/main` then get their
  check on the next queue run.

Expected: 2 to 3 CI runs per landing (ready-PR run, merge-group run, occasional retry) at
28 to 45 minutes each; 3 to 4 if the `zs/main` push run is kept. Not 2 runs at 20 minutes;
the E2E floor is 28.

### F6. Memo tickets loop without converging

Cause: an adversarial reviewer on prose always finds more; T11 went from one page to 990 lines
chasing it. This is a convergence problem, not a wall-clock one (T11 was 4.3 h and 7.5% of
agent time).

Fix: hard length (one page, nine decisions for T11) in the builder prompt; one Sol pass; the
driver decides which findings change a decision, the rest are listed as risks; implementation
detail goes to the implementing ticket's tests.

Expected: 1 to 1.5 h wall per memo, 80 to 120 agent-minutes.

### F7. Serial human landing

Cause: I landed one PR at a time, re-merging by hand and watching CI. Tens of minutes of
driver time per landing, mostly overlapped with T5's build.

Fix: done. The merge queue lands on green; the driver enqueues after the Sol gate.

### F8. T5-class tickets set the wave's wall clock

Cause: T5 spanned a Rust command, a sidecar store, a React dialog and tests; its first Sol fix
round alone was 332 minutes and its wall clock 13.6 h equals the wave's.

Fix: split tickets that cross the Rust and UI boundary into a backend ticket (command, store,
tests, its own Sol pass) and a UI ticket on top. Applies to T7 (registry: loader and launcher
first, Settings panel second) and T12 (OAuth and storage first, view second). Combined with
the 90-minute fix cap in F1.

Expected: each half is a 5 to 8 h feature ticket; the pair is sequential because the UI half
waits for the landed backend, so its wave contribution is 6 to 10 h unless the UI half is
specified against a stub. Still shorter than 13.6 h, and each half lands and is reviewed on
its own.

### F9. The stages run in series when they could overlap

Cause: build → test → critic → Sol → fix ran one after another; the tester and the auditor
read the same diff and do not depend on each other.

Fix: after the build, run Gemini and Sol's full pass concurrently on the auditor semaphore
(two slots, two external processes); the critic, when a feature has one, is an in-process Claude
agent and takes no auditor slot, so it runs alongside. Consolidate all findings into one fix
round; then Sol's delta and, if Gemini FAILed, its retest.

Expected: 25 to 40 minutes of wall time per ticket removed from the critical path (today's
serial 18 + 15 + 32 becomes the longest of the three, about 32, less residual queueing).

### F10. Defect classes get rediscovered per ticket

Cause: bounded input, persist order, containment and complexity gates were each found by a
reviewer on a ticket after the builder had already shipped them.

Fix: a builder checklist in the ticket prompt, drawn from the defect table above: cap every
relay-sourced string at the DTO, order writes so every prefix is consistent, give every
external server or child process an explicit env, bound every gate by the quantity that costs
(nodes, not bytes), and write the test that fails when the guard is removed. Zero agent cost;
it removes review rounds.

## What the loop looks like after the changes

Builder with the checklist (fast gates only) → Gemini, critic (features), Sol full pass in
parallel → one consolidated fix round (90-minute cap) → Sol delta on the fix diff, Gemini retest
if it FAILed → one more delta only if the delta raised a new BLOCK → draft PR marked ready →
queue. Four builders at a time; auditors and heavy commands on separate semaphores of two.

Expected per ticket (ranges, until a second measured wave exists): ports 2 to 4 h wall and
150 to 220 agent-minutes; a single feature ticket 5 to 8 h wall and 250 to 350 agent-minutes; a
split backend-then-UI pair 6 to 10 h sequential; memos 1 to 1.5 h. CI 2 to 3 runs per landing.
A 6-hour feature ticket with no open BLOCK is the model, not a failure.

## What does not change

Sol still audits every ticket and still looks at fix diffs; Gemini still tests every code
ticket; every landing still goes through CI on the merged result; DCO, conventional commits,
and the port procedure stay. Defects of the classes in the table above still block.

## Second baseline, provisional (waves 2 and 3, measured 2026-09-05)

Status: observational, not a causal test of the revised loop. It records what waves 2 and 3 cost under the
revised loop; it does not show that the loop change caused the difference, because the tickets were
smaller by design (see caveat 1). Reviewed by GPT-5.6 Sol on 2026-09-05 before publication; its two
corrections (reconcile the table totals, narrow the claims) are applied here.

Scope: every workflow agent that ran for the wave 2 and wave 3 tickets between 2026-09-04T22:55Z and
2026-09-05T10:09Z, read from the workflow transcripts. Method: one agent = one transcript; agent-minutes =
last timestamp minus first timestamp, rounded per agent; stage = the role line of the agent's prompt. The
same method reproduces the wave-1 table above to within one agent-minute (2,588 vs 2,587; audit 606 in
19 runs; tester 193 in 11; PR opener 91 in 7), so the two tables are comparable. Wave 4 (T3b, T12, T7b)
and the first wave-5 ticket (T15) were still running when this was measured and are not included.

### Agent time by stage

| Stage | Wave 1 (7 tickets) | Waves 2 and 3 (4 code tickets, 3 memos) |
|---|---|---|
| Builder (first build) | 274 min | 303 min in 7 runs |
| Fix rounds, all sources | 1,182 min (audit 790, critic 238, tester 154) | 328 min in 12 consolidated runs |
| Sol audit, full pass | 606 min in 19 runs | 116 min in 7 runs |
| Sol delta pass | did not exist | 157 min in 10 runs |
| Sol-finding verifiers (driver-added, T3) | did not exist | 51 min in 18 runs |
| Critic | 241 min in 16 runs | 156 min in 9 runs |
| Port check | did not exist | 72 min in 2 runs |
| Gemini tester | 193 min in 11 runs | 77 min in 9 runs |
| PR opener | 91 min in 7 runs | 66 min in 5 runs |
| Extra review-round workflows (T12a rounds 3 to 5; T9 and T7a round 3) | none | 119 min in 9 runs |
| Total | 2,588 agent-minutes, 88 agents | 1,443 agent-minutes, 88 agents (rows sum to 1,445 from per-row rounding) |

### Per ticket

| Ticket | Kind | Agents | Agent-min | Wall clock | Sol runs (full + delta) |
|---|---|---|---|---|---|
| T3 port/4316 | port | 32 | 451 | 7.4 h (incl. a 2 h driver fix round and 18 verifiers) | 3 |
| T6-config | feature half | 10 | 163 | 2.2 h | 2 |
| T9 PDF export | feature | 11 | 219 | 3.5 h | 3 |
| T7a registry core | feature half | 17 | 275 | 4.4 h | 4 (one over the cap) |
| T7 memo | memo | 2 | 26 | 0.4 h | 1 |
| T11 cut | memo | 2 | 22 | 0.4 h | 1 |
| T12a memo | memo | 10 | 183 | 6.8 h across four workflows | 3 plus critic rounds 4 to 6 |
| Shared round-3 workflow (T9 and T7a findings re-verified together) | review round | 4 | 104 | 1.4 h | 0 |
| Total | | 88 | 1,443 | | |

### Reading against the plan's expectations

- **Audit plus fix-audit** (F1) was expected to fall from 1,396 to 550-750 agent-minutes per seven tickets.
  Measured upper bound: full audit 116 + delta 157 + verifiers 51 + all fix rounds 328 = 652 for four code
  tickets and three memos. It is an upper bound because the fix rounds are consolidated and the audit-driven
  share cannot be separated from the tester- and critic-driven share. Per code ticket that is at most about
  160 agent-minutes against about 200 in wave 1: a modest drop, not the predicted 40 to 60 percent, and not
  attributable to the loop shape alone (caveat 1).
- **Critic loop** (F2) fell from 479 to 156 agent-minutes; memos got no critic, ports got the single port check.
- **Gemini tester** (F3) fell from 347 to 77 agent-minutes; no tester fix round was triggered by a missing-test list.
- **Wall clock per code ticket** fell from 3.9-13.6 h to 2.2-4.4 h. Most of that is F8 (split tickets: T6
  config half, T7a backend half) and F9 (tester and auditor in parallel), not the audit shape.
- **Sol run cap** held at three for every ticket except T7a (four) and the T12a memo, which ran three Sol
  passes and then critic rounds 4 to 6 in separate workflows despite the one-pass memo rule (F6). The cap has
  to live in the workflow script, not only in this document.
- **PR stage** (F4): 13 agent-minutes per PR-opener run, unchanged. The plan's 25-40 minute wall-clock target
  for the PR stage is NOT measured here; agent-minutes and wall minutes are different quantities.

### Caveats

1. Load-bearing: waves 2 and 3 tickets were smaller by design (a port, two feature halves, three memos), so the
   drop from 2,588 to 1,443 agent-minutes and the wall-clock drop cannot be attributed mainly to the revised
   loop. A like-for-like test needs a full feature ticket under the revised loop; T3b, T12 and T7b in wave 4
   are the first candidates.
2. The 2-hour driver-run fix round and the 18 verifier agents on T3 were outside the workflow script; they are
   counted because they were real cost.
3. Wave 4 is appended when it lands; its running totals at 2026-09-05T18:41Z were T3b 235, T7b 150, T12 150
   and T15 83 agent-minutes.

## Grok 4.6 audit log

- Pass 1 (2026-09-05, `x-ai/grok-4.6` via OpenRouter): ACCEPT WITH CHANGES, ten required
  changes. Applied: round and severity tags for every demonstrated defect (table above); F1
  rewritten so the delta may raise new BLOCKs on fix diffs, with a severity rubric and
  file-triggered `xhigh`; F2 keeps an independent port check; F3 retests after a FAIL and treats
  untested crash paths as FAIL; F4 separates the auditor and compile semaphores and stops
  claiming machine time as agent time; F5 explains all 26 runs, drops the "2 runs at 20 minutes"
  claim, adds draft-PR CI gating, and replaces the standing rerun list with retry-then-quarantine;
  F6 corrected to four Sol passes and a 1 to 1.5 h estimate; savings recast as ranges with
  T5-class on its own line; new F8 (split cross-boundary tickets), F9 (overlap stages), F10
  (builder checklist) from Grok's missed-levers list. Rejected: Grok's "push storm" explanation
  for the CI count; agents never pushed mid-loop, the cancelled runs were my base re-merges,
  which the draft-PR gating and the overlap rule remove.
- Pass 2 (2026-09-05): ACCEPT WITH CHANGES, seven remaining, all applied in this revision:
  open BLOCKs never land and the 90-minute cap parks only WARNs; the rubric names the
  wrong-quantity gate class; deltas re-scan untrusted-input surfaces; F9 restated as 25 to 40
  minutes with the critic off the auditor semaphore; F8's pair stated as sequential 6 to 10 h;
  the BLOCK fraction corrected to four of six rows; F5 counts the `zs/main` push run and
  proposes dropping it. Settled by Grok: no discovery-free deltas, no port-only self-checklists,
  no standing rerun list, `xhigh` by files touched, separate semaphores, fast local gates only,
  FAIL means one fix and one retest, T5-class is the wave clock, the merge queue is done.
