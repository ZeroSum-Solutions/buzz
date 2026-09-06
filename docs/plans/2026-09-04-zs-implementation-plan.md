# ZeroSum Buzz fork — implementation plan

Date: 2026-09-04. Source: `2026-09-04-zs-feature-audit.md` (Sol pass 1 applied). This plan turns
the seven asks into branches, tickets, bars and evals, and fixes the loop every ticket runs
through. Revision 3 folds in Sol's passes 1 and 2 on this plan (log at the end).

## Branch and landing model

- `main` mirrors `upstream/main` and is never committed to. `zs/main` is the integration
  branch and the fork's GitHub default branch (set in step 0 so PRs and `zs-land` target it).
- One ticket = one branch `feat/<slug>`, `port/<pr>`, `fix/<slug>` or `spike/<slug>` off
  `zs/main` = one PR = one squash commit. DCO on every commit: `git commit -s`; for imported
  commits `git cherry-pick --signoff` and `git rebase --signoff`. Before push:
  `git log --format=%B origin/zs/main..HEAD | grep -c Signed-off-by` equals the commit count.
- Port procedure: `git fetch upstream pull/N/head:pr-N`, then `git cherry-pick --signoff $(git merge-base upstream/main pr-N)..pr-N` onto the ticket branch, resolving conflicts. This replays the PR's complete commit range with sign-off and is the same result as rebasing the PR head onto `zs/main`; the audit's "port, not cherry-pick" wording meant "not a clean pick". Pin the module path the PR actually uses at import time; a rename is a fork deviation.
- Port tickets carry upstream parity only. Fork deviations on a ported feature are their own
  ticket, so an upstream merge can retire the port without losing fork work (range-diff the
  upstream merge against the port, then drop the port commit).
- Landing goes through GitHub's merge queue on `zs/main` (ruleset "zs/main merge queue",
  squash, one entry built at a time, CI on the `merge_group` event). The queue tests each pull
  request against the merged result in order, so the base cannot drift under a tested change and
  nobody re-merges by hand.
  1. Before enqueueing, run the ticket's gates (fast set plus its own tests, targeted; never a
     full suite, see the hard rule under step 1 of the gauntlet) and the Sol audit on the branch.
  2. Open the PR ourselves with the full body (`gh pr create --base zs/main --body-file`):
     gates run and their results, the tested base OID, the Gemini verdict, the Sol verdict.
  3. Overlap rule instead of blanket re-testing: if `origin/zs/main` moved since the branch's
     base, compute `git diff --name-only <base>..origin/zs/main` and intersect it with the
     branch's own file list. Empty intersection: enqueue as is. Non-empty: `git merge --signoff
     origin/zs/main`, rerun the gates that cover the overlapping files, then enqueue.
  4. Enqueue with `gh pr merge <n> --squash --auto`. The queue runs CI on the merge group and
     merges on green; the repository deletes the head branch on merge. `zs-land` is not used in
     this repository because it expects an immediate merge, which a queue does not give. The
     enqueue command is on this machine's guarded list, so it runs only with Devin's go-ahead.
  5. If a CI job fails on the fork for a reason outside the diff (missing secret, upstream-only
     runner, a flake on the recorded list), re-run only the failed jobs once; if it fails the same
     way, fix the job on `zs/main` as a fork-only edit. Never `--allow-no-ci`.
  6. Direct pushes to `zs/main` are for fork-only CI and docs commits by the repository admin
     (the ruleset's bypass actor); feature work always goes through the queue.
- Linux CI jobs run on Blacksmith (`runs-on: blacksmith-4vcpu-ubuntu-2404`, fork-only label swap
  on `ci.yml`, `_ci-*.yml`, `mesh-lifecycle.yml`); macOS and Windows jobs stay on GitHub's runners.
- Hooks and differential lanes compare against `origin/main` (`AGENTS.md:128`,
  `scripts/check-file-sizes-core.mjs:44`), so on the fork they see the cumulative fork delta.
  That is stricter, not wrong. If a lane flags an upstream-ported file, the PR notes it and the
  fix is a fork-only base override in that script, done as its own tiny ticket.
- Upstream sync: `git fetch upstream && git checkout main && git merge --ff-only upstream/main`,
  then `git checkout zs/main && git merge main`, then `just ci`.

## The loop every ticket runs

Revised 2026-09-05 per `2026-09-05-zs-throughput-plan.md` (accepted by Devin). The original
serial loop and its measurements are in that document.

1. **Builder** (Claude subagent; Sonnet 5 for ports and UI, Opus 5 for Rust plumbing) in its
   own worktree, with the defect checklist: cap every relay-sourced string at the DTO; order
   writes so every prefix is consistent; give every child process or external server an
   explicit environment; bound every gate by the quantity that costs (nodes, not bytes); write
   the test that fails when the guard is removed. Tests first where the ticket names a test.
   Local gates are the fast set only, in a shell that ran `. ./bin/activate-hermit`:
   ```
   just fmt-check clippy desktop-check desktop-tauri-fmt-check desktop-tauri-clippy file-size-check
   ```
   plus the ticket's own test files with enforced counts (`n=$(cargo test <filter> -- --list | grep -c ': test'); test "$n" -ge <N>`).
   No local `just ci`; CI on Blacksmith is the full gate. **Hard rule (Devin, 2026-09-05): no
   full suite runs twice.** The merge queue runs the whole smoke project in eight shards, the
   whole `desktop-test` and every `cargo test`; a builder, tester, critic, fix agent or driver
   never runs any of those in full before the PR, on the Mac or on the AWS box. The ticket's own
   test files run targeted: `cargo test <filter>`, the node runner on the file,
   `cd desktop && pnpm build:e2e && pnpm exec playwright test --project=smoke <spec>.ts`. A
   queue ejection is read, the one failing spec is rerun locally, fixed, re-enqueued. At most
   four builders at once, on a heavy-command semaphore of two for targeted `cargo test`,
   `desktop-test` and E2E runs.
2. **In parallel after the build**, on the auditor semaphore of two external processes:
   - **Tester** (Gemini 3.8 Flash, `agy -p --model gemini-3.8-flash-high --mode accept-edits`,
     disposable worktree, clean-tree assertion after): reads the diff and acceptance checks,
     runs the fast gates, tries to break the feature. Returns PASS or FAIL with a repro. A
     missing-test note that describes an untested crash or race is a FAIL; other notes are
     follow-ups.
   - **Sol full pass** (`codex exec -s read-only -c model="gpt-5.6-sol" -c model_reasoning_effort=<effort> -o <report> "<adversarial prompt naming the scope git diff origin/zs/main...HEAD>"`).
     Effort is `xhigh` when the diff touches `crates/buzz-agent/src/mcp.rs`, `crates/buzz-acp/`,
     `managed_agents/runtime.rs`, spawn or env code, `secret_store`, keychain, relay crates, or a
     DTO carrying relay-sourced data; `high` otherwise. The driver verifies every BLOCK and WARN
     against the code.
   - **Critic** (in-process Claude agent, features only, once): blind comparison against the
     bar plus the checklist. Ports get a single non-blind check instead: PR tests present and
     passing, eval counts met, every deviation named against the port diff. Memos get neither.
3. **Severity rubric.** BLOCK: unbounded untrusted input, missing containment or credential
   exposure, an unbounded resource, loop or process tree, a swallowed failure, torn multi-write
   state, a guard whose removal fails no test, a complexity or resource gate that bounds the
   wrong quantity. Verified WARNs outside that list are follow-ups in the PR body and issues on
   the fork; they do not loop.
4. **One consolidated fix round** for every BLOCK, every FAIL, and every failed checklist item.
   A fix agent that needs more than 90 minutes stops, commits what passes, and reports; only
   WARN and out-of-scope work may be parked, and an unfixed BLOCK keeps the ticket open (the
   driver splits the ticket, never the BLOCK).
5. **Sol delta pass** on the fix diff, the prior findings, and a re-scan of every
   untrusted-input surface in the whole diff. It may raise new BLOCKs; if it does, one more fix
   and one more delta, then the driver. Gemini retests once if it had FAILed. Three Sol runs at
   most.
6. **PR.** Opened as a draft with the full body (gates and exit codes, tested base OID, Gemini
   verdict, critic result, Sol rounds and final state, follow-ups, test plan); marked ready when
   the loop is done, which runs CI once; then enqueued. A ticket with an open verified BLOCK is
   never enqueued.

Memos: hard length in the builder prompt (one page), one Sol pass, the driver decides which
findings change a decision and lists the rest as risks. Tickets that cross the Rust and UI
boundary are split into a backend half and a UI half, each with its own loop.

## Bars (what "better" means)

| Ticket class | Bar the critic fetches | Measurable half |
|---|---|---|
| Port of an upstream PR | The PR's own diff and test files (`gh pr diff N -R block/buzz`) | The PR's tests pass on our branch; no existing test regresses |
| New fork feature | The nearest upstream feature of the same shape, named per ticket | Gates green; named eval commands pass with the stated counts |
| Spike or memo | The written question it answers | The memo cites measured results or answers every checklist item |

## Step 0: setup (no feature code)

- Set the fork's default branch to `zs/main` (`gh repo edit ZeroSum-Solutions/buzz --default-branch zs/main`).
- Commit the audit and this plan on `zs/main`, push.
- Sync `main` to upstream (1 commit behind at time of writing) and merge into `zs/main`.
- `just ci` green on `zs/main` before any branch starts. Baseline already recorded on
  2026-09-04: `desktop-test` 6131 pass, `desktop-tauri-test` all suites 0 failed; the full
  `just ci` result is recorded in the review log.

## Tickets

Sizes: S under half a day of agent time, M one to two days, L more. IDs are stable for the
GitHub issue list. Rust commands run from `desktop/src-tauri` unless a crate is named, because
the root workspace excludes that manifest (`Cargo.toml:35`).

### T1 · port/2706 — agent role in the `@` picker (S)

- Branch `port/2706` from `zs/main`. Import PR #2706 (`webdevtodayjason:feat/mention-selector-agent-about`) with `git cherry-pick --signoff`, resolve conflicts, keep its tests.
- Tests that must exist and pass after the port: `desktop/src/features/messages/lib/mentionSuggestionMapping.test.mjs`, `desktop/tests/e2e/mention-descriptions-screenshots.spec.ts` (add it to the smoke `testMatch` allow-list in `desktop/playwright.config.ts:18` so the runner shows it counted).
- Eval:
  ```
  cd desktop && node --import ./test-loader.mjs --experimental-strip-types --test src/features/messages/lib/mentionSuggestionMapping.test.mjs src/features/messages/ui/MentionAutocomplete.test.mjs
  just desktop-test
  cd desktop && pnpm build:e2e && pnpm exec playwright test --project=smoke mention-descriptions-screenshots.spec.ts   # the ticket's spec only; the queue runs the full smoke
  ```
- Acceptance: typing `@` in a channel shows each agent's `about` under its name; agents with no `about` show none; the hover card still shows `about`.
- Bar: PR #2706's diff.

### R1 · runbook: set descriptions on the nine Broken English agents (operational, not code)

- Runs after T1 lands, with Devin's go-ahead in the session. Records before and after values per agent, the target relay, the emitted kind:0 event ids, and the rollback (re-publish the previous kind:0). Lives at `~/projects/clients/broken-english/buzz/runbooks/2026-09-xx-agent-descriptions.md`.

### T2 · port/6731 — markdown attachments open in a viewer panel (S)

- Branch `port/6731` (`block:fizz/md-viewer`). Cherry-pick with sign-off, resolve, keep tests.
- Tests that must exist and pass: `desktop/src-tauri/src/commands/media_download_tests.rs`, `desktop/src/features/channels/ui/ChannelPane.helpers.test.mjs`, `channelPaneAuxiliaryLayout.test.mjs`, `markdownDocFocus.test.mjs`, `desktop/src/shared/ui/markdown/markdownDocFile.test.mjs`, `desktop/tests/e2e/markdown-doc-viewer.spec.ts` (added to the smoke allow-list).
- The imported `media_download_tests.rs` must be registered in `commands/mod.rs` (the existing `media_download` module already has inline tests, so a broad filter proves nothing). Eval:
  ```
  cd desktop/src-tauri && n=$(cargo test commands::media_download_tests -- --list | grep -c ': test'); test "$n" -ge 1
  cd desktop/src-tauri && cargo test commands::media_download_tests
  just desktop-test
  cd desktop && pnpm build:e2e && pnpm exec playwright test --project=smoke markdown-doc-viewer.spec.ts   # the ticket's spec only; the queue runs the full smoke
  ```
- Acceptance: clicking an `.md` attachment opens the panel; GFM tables and code render; a fixed 500 KB fixture (`desktop/tests/fixtures/long-doc.md`, hash recorded in the PR) reaches panel-ready in under 1.0 s with no main-thread task over 200 ms, measured three times with the Performance API from the e2e spec.
- Bar: PR #6731's diff.

### T3 · port/4316 — channel Files tab, parity (M)

- Branch `port/4316` (`mismai-li:feat/channel-files-tab`). Cherry-pick with sign-off, resolve. The PR ships no tests; the port adds them.
- The PR's files live under `desktop/src/features/channel-files/` (`ChannelFilesTab.tsx`, `FileCard.tsx`, `useChannelFiles.ts`, `useFileFolders.ts`); keep that path. Tests first: `desktop/src/features/channel-files/useChannelFiles.test.mjs` (imeta parsing, a Markdown attachment labeled by `filename` not `.bin`, newest-first order), `desktop/src/features/channel-files/useFileFolders.test.mjs` (folder assignment round-trip), `desktop/tests/e2e/channel-files-tab.spec.ts` registered in the smoke allow-list.
- Eval:
  ```
  cd desktop && node --import ./test-loader.mjs --experimental-strip-types --test src/features/channel-files/useChannelFiles.test.mjs src/features/channel-files/useFileFolders.test.mjs
  just desktop-test
  cd desktop && pnpm build:e2e && pnpm exec playwright test --project=smoke channel-files-tab.spec.ts   # the ticket's spec only; the queue runs the full smoke
  ```
- Acceptance: the tab lists the attachments in the loaded window with folders, bulk select and drag-drop as in the PR. Bulk upload stays as the PR ships it in this ticket; T3b changes the default.
- Bar: PR #4316's diff.

### T3b · feat/files-index — dedicated attachment index and bulk-upload default (M)

- After T3 has landed. `useChannelFiles` reads the loaded window (`useChannelMessagesQuery`), which is top-level only and 200 per page (`commands/channel_window.rs:28, 48`). This ticket adds an attachment index: open the live subscription first, then paginate the channel's history including reply kinds, keep only events with `imeta`, overlay edits and deletions, expose deterministic pagination. Bulk drag-drop moves behind a setting that defaults off with a batch cap of 20.
- Tests first: more than 250 entries paginate without loss, a reply attachment appears, an arrival during backfill appears once, a deleted message's file disappears, an interrupted backfill resumes without duplicates, the setting gates bulk drop.
- Eval: `desktop/src/features/channel-files/useChannelFilesIndex.test.mjs` through the node runner with the six named cases; `just desktop-test`; `desktop/tests/e2e/channel-files-index.spec.ts` (smoke allow-list) seeds 250 files, asserts all are listed and render in under 500 ms over three runs.
- Bar: PR #4316's tab as the baseline it must not regress; issue #4428 as the failure it must close.

### T4 · port/6651 — extra MCP servers via env (S)

- Branch `port/6651` (`BradGroux:agent/extra-mcp-servers`, mergeable today). Cherry-pick with sign-off. The PR adds a `trusted` flag and withholds `BUZZ_PRIVATE_KEY`, `NOSTR_PRIVATE_KEY`, `BUZZ_RELAY_URL` and `BUZZ_AUTH_TAG` in `buzz-agent`'s `spawn_one` for untrusted servers.
- Tests that must exist and pass: the PR's `extra_mcp_commands_*` tests in `crates/buzz-acp/src/lib.rs` and `desktop/src-tauri/src/managed_agents/env_vars/tests.rs`.
- Test added by the port: extend the existing `fake-mcp` test binary (`crates/buzz-agent/Cargo.toml:21-25`) to report its environment variable names in a tool result; an end-to-end spawn test in `crates/buzz-agent` registers it as an untrusted server and asserts none of the four identity names appear, and that a trusted spec still receives them.
- Eval:
  ```
  cargo test -p buzz-acp extra_mcp -- --list | grep -c ': test'    # >= 4
  cargo test -p buzz-acp extra_mcp
  cargo test -p buzz-agent untrusted_mcp_env -- --list | grep -c ': test'   # >= 2
  cargo test -p buzz-agent untrusted_mcp_env
  cd desktop/src-tauri && cargo test env_vars
  ```
- Acceptance: with `BUZZ_ACP_EXTRA_MCP_COMMANDS` set on a buzz-agent instance, the agent lists the extra server's tools.
- Bar: PR #6651's diff.

### T5 · feat/prompt-source — reload an agent prompt from a file (S)

- Branch `feat/prompt-source`. Machine-local sidecar `<app-data>/agents/prompt-sources.json` (definition id to absolute path). One backend command `set_prompt_source_and_reload(definition_id, path: Option<String>)`: `None` clears the mapping; `Some` validates the path (inside the user's home after symlink resolution, UTF-8, at most 64 KiB), reads the file, submits through the existing update request (`types/requests.rs:103`) so validation, the kind:30175 publish and the persona hash follow the normal path, and only after the persona save succeeds (`commands/personas/update.rs:192-215`) writes the mapping. Order is validate, read, persona save, then mapping, so a failure at any step leaves the mapping and the effective prompt agreeing; a test injects a failure at each boundary. It returns `{ local_updated: bool, publish: "published" | "queued" | "failed:<reason>" }` because publication is a later step that can queue (`commands/personas/sharing.rs:66-130`). The dialog gets a path field, Reload and Clear.
- Runtime capability fact: prompt-from-file is a desktop feature, not a harness capability, so `KnownAcpRuntime` is unchanged; the PR says so in `desktop/src/features/agents/AGENTS.md` per its rule at line 382.
- Tests first (Rust, module `managed_agents::prompt_source`): missing file, symlink outside home, over-limit, invalid UTF-8, clear removes the mapping, happy path updates `system_prompt` and changes `persona_content_hash`, and the emitted kind:30175 event deserializes to `PersonaEventContent` whose content contains the prompt text and no path string. Frontend test: Reload disabled when no path is set.
- Eval:
  ```
  cd desktop/src-tauri && cargo test prompt_source -- --list | grep -c ': test'   # >= 7
  cd desktop/src-tauri && cargo test prompt_source
  just desktop-test
  ```
- Acceptance: edit `agent-prompts/pm.md`, click Reload, restart the agent, and the prompt the adapter receives equals the file bytes at the delivery seam: the ACP `session/new` request's system prompt (`crates/buzz-acp/src/acp.rs:632-676`), captured with the harness's request logging; the env variable alone is not proof.
- Bar: `custom_harnesses.rs` for the file-input validation posture; upstream's `AgentDefinitionDialog.tsx` for the dialog pattern.

### T6 · OpenSEO through runtime config (S, blocked on vendor approval)

- Precondition: Devin approves DataForSEO as a metered vendor and sets a monthly spend limit in the DataForSEO dashboard, and the key is added with `zsvault add`. Before that, nothing of OpenSEO runs. What may run before approval: config generation validated against a fake MCP server (the T4 fixture) to prove the discovery path.
- Config placement, verified against the config bridge: Claude reads `mcpServers` from `<CLAUDE_CONFIG_DIR>/.claude.json` (`config_bridge/claude.rs:3-18`, `reader.rs:270`) or a project `.mcp.json` in the spawned working directory (`~/.buzz`, `runtime.rs:563`); Codex reads `<CODEX_HOME>/config.toml`. A custom `CLAUDE_CONFIG_DIR` creates a fresh keychain namespace and leaves Claude logged out unless `CLAUDE_SECURESTORAGE_CONFIG_DIR` is also managed (`config_bridge/types.rs:191-197`, `AgentConfigPanel.tsx:292-310`), and Codex has the same shape with `CODEX_HOME`. So the ticket uses the project `.mcp.json` in the agent working directory for Claude and a per-agent `CODEX_HOME` only after its login behavior is proven from a Dock launch; the operator's own config is untouched either way. Skills: Claude discovers `.claude/skills`, Codex `.codex/skills` (`discovery/catalog.rs:50-101`); the eval asserts each runtime lists one named pinned skill.
- Steps after approval: run OpenSEO self-hosted (`docker`, `AUTH_MODE=local_noauth`, bound to localhost); add the MCP to the Copy and Audit agents' runtime config; install `plugins/openseo/skills/` at a pinned revision into the agent skill path.
- Eval, scripted as `scripts/zs/openseo-smoke.sh <runtime>`: before approval, spawn a Claude runtime agent from a Dock launch with the fake server in its config and assert the reply lists the fake tool. After approval: same with OpenSEO; the agent runs one site-audit tool (locally computed) on brokenenglishjewelry.com and the reply names the audit id; DataForSEO traffic is pointed at a local counting sentinel (`DATAFORSEO_BASE_URL` override in the container) that rejects every request, and the script asserts its counter is zero.
- Acceptance: both smoke tests pass for the Claude and Codex runtimes from a Dock launch.
- Bar: OpenSEO's documented `plugins/openseo` install path.

### T7 · feat/mcp-registry — MCP registry with trust model (L)

- Branch `feat/mcp-registry`, after T4. Depends on the T4 fake-server fixture and a fake HTTP MCP fixture (a tiny Streamable HTTP server in the test tree). OpenSEO is not a dependency; the T6 post-approval run is a separate integration check.
- Design memo first, `docs/plans/2026-09-xx-mcp-registry-design.md`, one page, reviewed by Sol before code. It must answer: the two server classes and the env each receives; the runtime capability matrix (buzz-agent: stdio only, since `McpServer` is the ACP `McpServerStdio` shape at `buzz-acp/src/acp.rs:25` and `buzz-agent/src/types.rs:536`; Claude and Codex: stdio and HTTP through native config); the process boundary for stdio servers under Claude and Codex, where the adapter inherits the whole harness environment including provider keys and user-defined values (`runtime.rs:563, 692-701, 753-757`, `buzz-acp/src/acp.rs:454-517`), solved by a launcher `buzz-mcp-launch` that builds the child environment from empty with only platform essentials and the server's approved values, following the `env_clear` plus allow-list pattern in `buzz-agent/src/mcp.rs:733-754`, and on Windows supervises the child rather than exec; HTTP credentials, which no launcher can inject, handled by a local credential-resolving stdio proxy in front of Streamable HTTP upstreams (the same binary in proxy mode), so no secret is ever written to JSON or TOML; where secrets live (a shared read-only secret-store crate extracted from `desktop/src-tauri/src/secret_store.rs`, since a workspace binary cannot call the private Tauri module); the launcher's crate path, workspace membership (`Cargo.toml:2-34`), sidecar stubs (`justfile:167-180`), release build list (`justfile:306-309`), `scripts/bundle-sidecars.sh` and `tauri.conf.json:52-62` plus the Windows manifest, with generated config naming the bundled launcher by absolute path; name collision rule with built-ins; per-agent toggle storage; config roots per agent with the login caveat from T6.
- Per-server environment, carried over from T4's Sol audit: `BUZZ_ACP_EXTRA_MCP_COMMANDS` can only carry a server's key in its argv, where `ps` and any crash dump can read it, so the T4 docs tell operators not to put one there and the registry has to give them somewhere else — a per-server `env` block whose values are keychain references resolved at spawn, which the design memo above already owns for Claude and Codex. Extend it to the buzz-agent path.
- Capability facts added to `KnownAcpRuntime` (`mcp_transports`, `mcp_config_root_env`) and projected through core to the UI per `desktop/src/features/agents/AGENTS.md:13, 34`; the guide is updated in the same PR.
- Then: `mcp_servers.json` schema and loader with `custom_harnesses`-style structure validation; Settings panel to add stdio and HTTP servers with an approve step that shows the exact command or URL; per-agent toggles in the definition dialog; generation of `BUZZ_ACP_EXTRA_MCP_COMMANDS` for buzz-acp and of native config for Claude and Codex under per-agent `CLAUDE_CONFIG_DIR` and `CODEX_HOME`.
- Tests first (Rust, module `managed_agents::mcp_registry`, and the launcher crate): loader rejects a server named like a built-in, rejects an inline secret value, resolves a keychain reference; HTTP entry is refused for buzz-agent and accepted for Claude and Codex; generated Claude and Codex config is asserted structurally (command, args, URL, env references) by parsing the written files, because the config bridge readers keep only name, kind and enabled (`config_bridge/types.rs:226-233`) and cannot detect a wrong command; launcher end-to-end: spawned with the identity variables and two unrelated sentinel secrets set, the fake server it starts reports none of them; proxy end-to-end: a fake authenticated HTTP MCP fixture is invoked through Claude and Codex via the proxy and the credential appears in no generated file; the toggle changes only the named agent's generated config. Frontend test: approve step required before save.
- Eval:
  ```
  cd desktop/src-tauri && cargo test mcp_registry -- --list | grep -c ': test'   # >= 8
  cd desktop/src-tauri && cargo test mcp_registry
  cargo test -p buzz-mcp-launch
  just desktop-test
  ```
- Acceptance: add the fake stdio server from Settings, toggle it on for Copy only; Copy lists its tool and Brand does not; add the fake HTTP server and invoke its tool from a Claude runtime and a Codex runtime through the proxy; on a buzz-agent runtime the HTTP entry shows as unsupported; delete a server and the agent stops seeing it after restart.
- Bar: PR #5321 for the schema shape; `custom_harnesses.rs` for the loader.

### T8 · spike/pdf — PDF route decision (S)

- Branch `spike/pdf`, throwaway. Render one real Broken English document (an approval page from `brand/render-approval.py`, with a table, a code block, a remote image, and enough text for three pages; fixture hash recorded) through (a) webview print-to-PDF behind a Tauri command and (b) one Rust HTML-to-PDF crate.
- Validation per output, scripted in `scripts/zs/pdf-validate.sh <pdf>`: `pdftotext` extracts the three headings, the table cells and the code line; `pdftoppm` renders every page to PNG without error and the page count is 3; size and wall time recorded. The offline case runs the render with the fixture's remote image pointed at a local sentinel HTTP server that logs and refuses every request; the sentinel log must show the attempt and the PDF must show the placeholder. No firewall rules are changed.
- Deliverable: memo in `docs/plans/2026-09-xx-pdf-route.md` with the measurements, hashes of the PDFs and PNGs, and the pick. No production code lands.
- Bar: the memo answers the question with numbers.

### T9 · feat/pdf-export — export a document as PDF (M)

- Branch `feat/pdf-export`, after T2 and T8. A document mode of `markdown.tsx` (links kept, attachments as links, code never collapsed, print CSS), a Tauri command that produces bytes via the route T8 picked, output through `export_util.rs`.
- Tests first: renderer snapshot in document mode; the Rust command's output is parsed (page count from `pdftotext`-style extraction in a test helper or a Rust PDF parser), contains the fixture's headings, table cells and code line, and every page renders with `pdftoppm` in the e2e spec; the save dialog cancel path returns without writing.
- Eval:
  ```
  cd desktop/src-tauri && cargo test pdf_export -- --list | grep -c ': test'   # >= 3
  cd desktop/src-tauri && cargo test pdf_export
  just desktop-test
  cd desktop && pnpm build:e2e && pnpm exec playwright test --project=smoke pdf-export.spec.ts   # the ticket's spec only; the queue runs the full smoke
  ```
- Acceptance: from the T2 viewer, Export PDF saves a file that opens in Preview with headings, table and code intact; hash of the output for the fixture recorded in the PR.
- Bar: the same document printed from the macOS Chrome print dialog, compared page by page.

### T10 · feat/assets-facets — sort, filter and documents facet on the Files tab (M)

- Branch `feat/assets-facets`, after T3b. Sort by date, name, size, author; filter by type; a Documents facet (md, pdf, html, docx, csv); the channel canvas pinned at the top, opening the existing Canvas surface (`features/canvas/ChannelCanvas.tsx`), not the attachment viewer, so editing keeps working.
- Tests first: facet classification from `imeta` `filename` and MIME (md-as-octet-stream case), sort stability, pinned canvas present only when the channel has one, pinned row opens the Canvas surface and an edit saves.
- Eval: `desktop/src/features/channel-files/fileFacets.test.mjs` through the node runner, `just desktop-test`, `pnpm exec playwright test --project=smoke channel-files-facets.spec.ts` (the ticket's spec only; the queue runs the full smoke); sort or filter over the 250-file seed completes in under 100 ms measured in the spec, three runs.
- Bar: the T3b tab as the baseline it must not regress.

### T11 · docs/calendar-authz — calendar authorization contract (S)

- Memo at `docs/plans/2026-09-xx-calendar-authorization.md`. Checklist it must answer, each with a decision and a reason: OAuth scopes requested; which Google account binds to which Buzz identity and how the binding is stored; who owns the business calendar and how sharing is granted; which channels show it and who chooses; disconnect behavior; cached event data on disconnect and on membership loss; revocation propagation timing; refresh-token failure UX; what an agent may read or write.
- Eval: every checklist item has a decision; Sol reviews the memo; nothing above NIT remains.
- Bar: RFC #3227's scoped-integration shape.

### T12a · docs/calendar-view-design — event model and view design (S)

- After T11. `shared/ui/calendar.tsx` is a `react-day-picker` wrapper with no event layout. Memo at `docs/plans/2026-09-xx-calendar-view-design.md`: event model (all-day, multi-day, timezone, recurrence expansion window), month and agenda rendering, paging, keyboard operation and screen-reader semantics, create and edit conflict handling, and the component or library chosen.
- Eval: every item above has a decision; Sol reviews the memo; nothing above NIT remains.

### T12 · feat/google-calendar — Google Calendar as a scoped integration (L)

- Branch `feat/google-calendar`, after T11 and T12a. Revive PR #1382's approach: per-user OAuth through desktop commands, tokens in the keychain (`secret_store` pattern), the T12a view, create and edit for events the account can edit, a sidebar entry gated by the T11 contract.
- Tests first (Rust): token storage and refresh; refresh failure surfaces a reconnect state; no token in logs. A mock Google Calendar server in the test tree with two principals, one calendar shared to both, and an ACL-loss case; frontend tests render both principals' views and disable edit when write ACL is absent. Traceability: the PR carries a table mapping every T11 decision to a named test, including two Buzz principals, Buzz membership removal hiding the calendar in that channel, cached events purged on disconnect and on membership loss, revocation propagation within the bounded window, and agent read and write authority checked separately.
- Eval:
  ```
  cd desktop/src-tauri && cargo test google_calendar -- --list | grep -c ': test'   # >= 5
  cd desktop/src-tauri && cargo test google_calendar
  just desktop-test
  ```
  plus a live two-account checklist, run by Devin or with his go-ahead: two team members connect their own accounts in EGO Lite, both see and edit the shared calendar, one is removed in Google Workspace and loses it on next refresh.
- Acceptance: mock tests green and the live checklist signed off. Not done with only one of the two.
- Bar: PR #1382's diff for the OAuth and storage half; Google Calendar's own month view for the rendering half.

### T13 · fix/harness-discovery-bundle — prefer the bundle's harness binaries (S)

- Branch `fix/harness-discovery-bundle`. When the exe path is inside an `.app`, search the bundle before any workspace `target/` dir (`discovery.rs:363-370`).
- Test first, in `managed_agents::discovery::tests`, named `bundle_exe_prefers_bundle_over_workspace_target`: a bundle-shaped path resolves to the bundle's binary even when a workspace `target/release` binary exists; existing discovery tests unchanged.
- Eval:
  ```
  cd desktop/src-tauri && n=$(cargo test managed_agents::discovery::tests::bundle_exe_prefers_bundle_over_workspace_target -- --list | grep -c ': test'); test "$n" -eq 1
  cd desktop/src-tauri && cargo test managed_agents::discovery::tests::bundle_exe_prefers_bundle_over_workspace_target -- --exact
  cd desktop/src-tauri && cargo test managed_agents::discovery
  just desktop-tauri-clippy
  ```
- Bar: upstream's `command_search_dirs` semantics for dev builds unchanged.

### T14 · feat/path-links — local file paths in messages open on this Mac (S)

- Branch `feat/path-links`, after wave 4. Agents post report paths as text (`audit/verify/...md`, `buzz/approvals/<item>.html`). T15 attaches new files; this covers every path already in the history and any agent that forgets. Render a path token inside inline code (backticks) as a link when it resolves to an existing regular file under the sending agent's working directory or `$HOME/projects`. Click opens it with the OS default handler through Tauri's opener (macOS `open`), never executes it, never follows a symlink outside those roots, never asks the relay. Bare words never qualify. Nothing is resolved while a channel renders; resolution happens on hover or click. A `.md` under 2 MiB opens in the T2 viewer panel instead of the OS.
- Tests: a tokenizer test file `pathLinks.test.mjs` next to the tokenizer (relative and absolute paths, `..` traversal rejected, symlink escape rejected, a path to a missing file stays text), a Rust test for the resolver's root containment (`commands/path_links_tests.rs`, registered in `commands/mod.rs`), and one e2e spec in the smoke allow-list that clicks a backticked `.md` path and asserts the viewer panel opens.
- Eval:
  ```
  just desktop-test
  cd desktop/src-tauri && cargo test commands::path_links
  cd desktop && pnpm build:e2e && pnpm exec playwright test --project=smoke path-links.spec.ts   # the ticket's spec only; the queue runs the full smoke
  ```
- Acceptance: in the Broken English approvals channel, clicking the backticked `.html` path in an existing approval message opens the page in the browser; clicking a `.md` path opens the viewer; a path to a file that does not exist renders as plain text.
- Bar: T2's viewer contract and Tauri opener scoping.

### T15 · feat/cli-attach-documents — `buzz messages send --file` accepts documents (S)

- Branch `feat/cli-attach-documents`. Today `crates/buzz-cli/src/client.rs:64` (`ALLOWED_MIMES`) admits only images and mp4, and `client.rs:1178` refuses everything else, so an agent that runs `buzz messages send --file report.md` gets "unsupported file type" and attaches nothing. The relay already accepts documents: `crates/buzz-relay/src/api/media.rs:362-420` routes a non-image body on `/upload` to `buzz_media::process_file_upload`, which checks the deny list at `crates/buzz-media/src/validation.rs:87` and the `max_file_bytes` cap (`crates/buzz-media/src/config.rs:78`). The desktop keys its markdown viewer off the imeta filename field, never the blob URL or MIME (`desktop/src/shared/ui/markdown/markdownDocFile.ts:5-9`; verify the exact field name in `desktop/src/shared/ui/markdown/parseImeta.ts` and match it).
- Scope, CLI only, no relay or desktop change: (1) `upload_file` accepts any file whose sniffed type is an image, a video, or absent from the relay deny list; for a file with no magic bytes, declare the Content-Type from the extension (`.md`/`.markdown` text/markdown, `.html` text/html, `.pdf` application/pdf, `.csv` text/csv, `.json` application/json, `.txt` text/plain, otherwise application/octet-stream) and send it on `/upload` only; the legacy `/media/upload` fallback stays image-only. (2) Documents are capped client-side at the relay's `max_file_bytes` default with the same error shape as images. (3) `build_imeta_tag` (`client.rs:40`) gains the filename field for every attachment so the viewer and the Files tab classify it. (4) `messages send` (`crates/buzz-cli/src/commands/messages.rs:650-665`) appends `![image](url)` only for images and `![video](url)` for video; a document adds nothing to the content, the imeta tag carries it. (5) `buzz upload file` keeps printing the descriptor.
- Tests that must exist and pass in `cargo nextest run -p buzz-cli`: content type by extension (each mapping, a `.bin` fallback, an uppercase extension); a `.md` body with no magic bytes is accepted while `.svg` and `.exe` bodies are refused with the deny-list reason before any request; the imeta tag for a document carries the filename field, `m text/markdown`, `x` and `size`, and no `dim` or `blurhash`; a document over the cap is refused client-side; `send` content for a document attachment has no `![image]` line. One test against the in-repo relay (the harness the `Backend Integration (relay e2e)` job uses, or a `buzz-relay` integration test that boots the router) uploads a real `.md` through `upload_file` and reads the blob back byte for byte; if no such harness can run from the crate, say so in the PR and record the manual command run against a local relay.
- Eval:
  ```
  cargo nextest run -p buzz-cli
  cargo clippy -p buzz-cli --all-targets -- -D warnings
  just file-size-check
  ```
  After landing and the next app install, Devin runs from a shell with an agent's `BUZZ_PRIVATE_KEY`: `buzz messages send --channel <test channel> --content "T15 check" --file docs/plans/2026-09-04-zs-implementation-plan.md`, and the `.md` opens in the T2 viewer and lists in the T3 Files tab. The builder never posts to the live relay.
- Acceptance: agents can attach markdown, HTML, PDF, CSV and JSON reports with `--file`; images and video behave exactly as before; the desktop opens the `.md` from the imeta filename; nothing on the deny list leaves the machine.
- Bar: the desktop's own upload path in `desktop/src-tauri/src/commands/media.rs:425-475` (declared MIME, `/upload` first, legacy fallback only on 404 or 405) and the relay's `process_file_upload` contract.

## Order and parallelism

Wave 1 (parallel build; serialized landing): T1 (landed, PR #6), T2 (landed, PR #5), T4 (landed,
PR #4), T5 (landed, PR #7), T13 (landed, PR #1), T8 (landed, PR #3), T11 (landed, PR #2).
Wave 2: T3 (landed, PR #11), then T3b after T3 lands (not started); T7 memo (landed, PR #9) then
T7a (open, PR #14, not yet enqueued); T12a (open, PR #15, not yet enqueued); T6 config-generation
half (fake server; landed, PR #8); the OpenSEO live half still waits for DataForSEO approval.
Wave 3: T9 (landed, PR #13), T10 (after T3b, not started), T12 (after T12a, not started).

## Ticket list for GitHub issues

Created on the fork only when Devin asks. Titles:

1. port/2706: agent role in the @ picker
2. port/6731: markdown viewer panel
3. port/4316: channel Files tab (parity)
4. feat: dedicated attachment index; bulk upload off by default
5. port/6651: extra MCP servers via BUZZ_ACP_EXTRA_MCP_COMMANDS, with spawn-boundary test
6. feat: agent prompt source file (set, reload, clear)
7. feat: OpenSEO via runtime MCP config (blocked: DataForSEO approval)
8. docs: MCP registry design memo
9. feat: MCP server registry, launcher, capability matrix
10. spike: PDF export route
11. feat: export document as PDF
12. feat: Files tab sort, filter, Documents facet, pinned canvas
13. docs: calendar authorization contract
14. docs: calendar event model and view design
15. feat: Google Calendar scoped integration
16. fix: harness discovery prefers the app bundle
17. runbook: descriptions on the nine Broken English agents (operational)

## Operational notes from wave 1 (2026-09-04)

- The fork had Actions disabled (GitHub disables workflows on a fork that carried them). Enabled
  once through the repository's Actions page; PR CI runs from then on.
- Image and release workflows (`docker.yml`, `sprig-image.yml`, both helm charts,
  `auto-tag-on-release-pr-merge.yml`, `desktop-release-candidate.yml`, `benchmark-harbor.yml`)
  push to Block's registries and fail on the fork with `permission_denied`. They are disabled as a
  repository setting, not by editing the files, so the fork keeps upstream parity.
- `ci.yml` runs push CI on `zs/main` (fork-only edit) because the relay artifact cache is written
  only on push events; without it every PR built the relay cold. The relay job's own limit is 75
  minutes on the fork for the same reason.
- Two flaky specs seen under load and green on rerun: `desktop/tests/e2e/empty-edit-delete.spec.ts`
  and the buzz-agent `cancelled_turn_with_usage_emits_notification_before_response` test
  (15 of 15 locally on both the branch and the baseline). Rerun before treating either as a
  regression, and say so in the PR.
- Desktop Smoke E2E shard 3 is chronically red on the fork's runners, with a rotating cast of
  specs, and it is not a port regression. Same shard, same 4 workflows, different casualties each
  run: run 33902000903 (push, `zs/main`, e56a75f - no port code) failed
  `messaging.spec.ts` "sends a thread message to its parent channel with a root-thread link" on all
  three attempts plus five flakes; run 33878282349 (`spike/pdf`) failed the same spec plus three
  flakes; runs 33909701431 and its rerun (`port/6731`) failed that spec, then
  `profile-custom-emoji-status.spec.ts:196`, with `navigation.spec.ts:448` flaky in every one of
  them. Almost every failure is a 5 s `expect` timeout on an untouched spec. The `smoke` project
  takes Playwright's 5 s default while the `integration` project already raises its own to 15 s on
  CI (`desktop/playwright.config.ts`), so the fork-only fix for this - on `zs/main`, per rule 5
  above, never inside a port branch - is to give `smoke` the same CI-only expect budget. A full
  local `just desktop-e2e-smoke` on `port/6731` shows the same shape: 1344 of 1354 passed, the 9
  failures were a ninth different set in untouched specs, 7 of them that same 5 s timeout, and all
  four of the run's CI casualties passed. Until the budget changes, rerun the shard and say so in
  the PR.
- `zs-land` can report "PR MERGED, but cleanup incomplete". The merge is done; finish by hand and
  do not re-run it.
- Scratch import branches from ports (`pr-<N>`) are left in place; deleting a branch needs Devin's
  explicit go-ahead on this machine.

## Operational notes from waves 2 to 4 (2026-09-05)

- Long pre-push hooks under load can outlast the SSH idle window. Git opens the SSH session
  before the hooks run, so a slow hook run kills the push with exit 141 and nothing transfers.
  Push with `GIT_SSH_COMMAND="ssh -o ServerAliveInterval=30 -o ServerAliveCountMax=40"` and check
  the exit code explicitly. A killed push can leave a zero-byte `.git/index.lock`; remove it only
  after confirming no git process is still running.
- Merge `origin/zs/main` into the branch before marking a PR ready. `dorny/paths-filter` run
  against a stale base counts `zs/main`'s own changes since the branch forked, so it runs the
  Rust and Postgres lanes even on a docs-only PR.
- Local Playwright: `playwright.config.ts` sets `reuseExistingServer` when not on CI, so two
  worktrees both running e2e on port 4173 silently test each other's build. Run e2e through a
  scratch config on a free port with `reuseExistingServer: false`.
- The Claude session limit can stop a workflow mid-run. Resume with the same run id so the
  agents that already finished replay from cache instead of rerunning.
- Merge-queue ejections seen so far: flaky `cluster_global_probe` postgres tests (now retried,
  PR #10); apt mirror dropouts during the Playwright `install-deps` step (now retried, PR #10);
  `inbox-live-update.spec.ts` scroll baseline (fixed in PR #12); `persistent-agent-audience.spec.ts`
  and shard-3 five-second timeouts (still flaky; rerun and say so in the PR).
- Merge-gate audit (2026-09-05, 15 queue runs, 3 ejections): both 01:30 and 02:00 ejections were
  the apt dropout above, before PR #10 landed; the 05:42 ejection was the hermit bootstrap download
  hanging 13 minutes and then `curl: (56)` (no retry, no cache) plus a hard
  `persistent-agent-audience.spec.ts:537` failure. Post-merge push runs also failed on
  `cancelled_turn_with_usage_emits_notification_before_response` (now retried in nextest),
  `huddle-transcription.spec.ts` (voice menu, 15 s) and `messaging.spec.ts:2439` (root-thread link),
  each three of three attempts. The wall clock per queue entry was 17 to 28 minutes with Desktop
  Core (28 min, of which the compiled-flag verification was 15) and the four smoke shards (15 to
  21 min on one worker) as the long poles; the macOS and Windows jobs were never on the critical
  path, so they stay. PR `ci/merge-gate-time` caches the hermit packages, lifts the compiled-flag
  verification into its own job, and runs eight smoke shards. See
  `docs/plans/2026-09-05-ci-merge-gate-and-runner.md` for the measurement gate and the next step.
- **No full suite runs twice (Devin, 2026-09-05).** `just desktop-e2e-smoke`, `just desktop-test`,
  a bare `cargo test` and `just ci` are merge-queue gates and are never run in full before the PR,
  locally or on the box. Only the ticket's own test files run before the PR, targeted. The
  wave-4 and wave-5 gauntlets ran the 45-minute smoke once per agent stage and then again in the
  queue; that cost more than an hour per ticket for no coverage.
- Heavy local gates (targeted heavy tests only, per the rule above) go through
  `scripts/zs/with-gate-lock.sh` (one machine-wide lock, waits up to 45 minutes): `scripts/zs/with-gate-lock.sh just desktop-test`, likewise for
  `just desktop-tauri-test`, `just desktop-tauri-test-compiled-flags` and every `cargo test`
  or `cargo nextest run`. Three worktrees running those at once is what made `buzz-agent`'s
  cancellation test flake locally. Fast gates (fmt, clippy, checks) do not take the lock.
- Once `scripts/zs/remote-ci/box.env` exists, a builder may run the heavy gates on the
  on-demand AWS Linux box instead of taking the machine-wide gate lock:
  `scripts/zs/remote-ci.sh <branch> [just targets...]` (default target `ci`; under the no-full-suite
  rule pass targeted targets, never bare `ci` or a full smoke, before a PR). It starts the
  stopped instance, checks the branch out there, streams the log to the terminal and to
  `~/Inbox/notes/`, copies any Playwright report to `~/Inbox/misc/`, stops the instance again
  and exits with the gate's own exit code — so a green run there means the same thing a green
  local gate means, without holding the lock or heating the Mac. Use `--push-local <branch>`
  for a branch that is not pushed yet (it sends the committed tree, never uncommitted work) and
  `--status` to check the box. There is one box, so one run at a time: a run holds an exclusive
  lock on it and a second run refuses and prints who holds it, rather than checking a second
  commit into the same tree. If `box.env` is absent the script says so and does nothing; fall
  back to `with-gate-lock.sh`. Blacksmith stays the merge gate either way. Setup and cost
  guardrails: `docs/plans/2026-09-05-ci-merge-gate-and-runner.md`, Step 3.
- A header-box change moved `chromeWrapperRef` and shifted the channel column by 42 px without
  failing any existing test, until the file-drop overlay no longer lined up with the drop zone.
  The geometry test in `channel-files-tab.spec.ts` now binds the measured chrome height to the
  header box so a future shift fails that test directly.
- The T3 Files tab replaced upstream PR 4316's per-folder plaintext kind:30078 events with one
  encrypted event per user and channel (folders v2). The relay is Block-hosted for this fork, so
  relay-side scoping for the plaintext folder events cannot be deployed here.
- Windows-only code paths (Job Objects, `launcher_windows` tests) run for the first time on CI,
  not locally. Two harness bugs surfaced there: `Command::output` closing stdin, and a fixture
  that assumed a Unix-absolute path.

### Follow-up tickets filed

- T2b: render markdown documents in a worker and bound the hast tree (from the T9 critic's
  findings F7 and F8).
- T3c: relay-side channel scoping for folder events, if the team ever self-hosts a relay.
- T7a follow-ups carried in PR 14: unsafe acceptance for the Windows launcher, the
  desktop-standalone sidecar list, a memo amendment for the Windows containment shape, and the
  ten run-3 WARNs.
- T6 live half still needs DataForSEO approval before it can run.
- R1 (agent descriptions) is blocked on the "Agent-managed profiles" setting decision.
- T14 and T15 filed 2026-09-05: the `buzz` CLI refuses non-image attachments, so agents post
  report paths as text and nothing in a channel is clickable. T15 fixes the CLI; T14 makes the
  existing paths open locally. The Broken English posting rule changes only after T15 is installed.

## Review log

- Sol pass 1 on this plan (2026-09-04, gpt-5.6-sol, xhigh): verdict BLOCK, 18 BLOCK and 8 WARN findings, one provenance NIT. All checked against the code.
  - Accepted and applied: `just ci` as the full gate (1); tester in a writable worktree with a clean-tree assertion (2); serialized landing with rebase, re-gate and recorded base OID (3); PR body written by us before `zs-land` (4); exact node test invocation (5); relay mutation moved to runbook R1 (6); Rust commands from `desktop/src-tauri` (7); named test files and smoke allow-list entries (8); dedicated attachment index as T3b (9); spawn-boundary end-to-end test (10); explicit set, reload and clear with publish status (11); Claude config file names (12); nothing of OpenSEO runs before approval (13); runtime capability matrix, HTTP not on buzz-agent (14); launcher as the process boundary (15); T7 depends on fixtures, not OpenSEO (16); PDF parse and render checks (17); T11 checklist and T12 mock plus live checklist (18); parity and deviation split (19); `--signoff` on imports (20); canvas opens its own surface (21); T12a design ticket (22); list-and-count on every Rust filter (23); hook base noted as stricter cumulative (24); latency thresholds with fixtures and three runs (25); finite critic exit and corrected provenance (26); capability facts in the runtime catalog (27).
  - Corrected on verification: finding 10 described the tree before the port. PR #6651 patches `buzz-agent`'s `spawn_one` with a `trusted` flag and withholds the four identity variables from untrusted servers. The end-to-end test Sol asked for is kept because it binds that seam.
- Sol pass 2 (2026-09-04): 12 BLOCK, 8 WARN, 1 NIT. Applied in revision 3: base-SHA check before `zs-land` and merge-not-rebase after push (1); every eval a path and a command, sentinel servers instead of firewall rules (2); `media_download_tests` registration and module-qualified filter (3); crate-root qualified T13 test name (4); T5 write order and failure-injection tests (5); T5 acceptance at the `session/new` seam (6); Claude config-dir login caveat, project `.mcp.json` instead (7); launcher builds env from empty with sentinel-secret test (8); credential-resolving stdio proxy for HTTP (9); launcher build, bundle and secret-store crate listed (10); structural config assertions instead of the lossy bridge round-trip (11); T11-to-T12 traceability table (12); enforced counts (13); Hermit in every gate shell (14); full Sol command (15); `fake-mcp` binary (16); skill discovery and counting sentinel (17); T3b after T3 lands (18); port procedure stated once (19); `channel-files` path pinned (20); line cites (21).
- Execution started after pass 2 was folded in; wave 1 builders had already begun on revision 2 text, so the critic and audit stages carry the revision 3 evals (Devin, 2026-09-04: if the plan looks solid, troubleshoot during implementation).
- Step 0 baseline: `just ci` exit 0 on `zs/main` at 63a169e52 (2026-09-04).
