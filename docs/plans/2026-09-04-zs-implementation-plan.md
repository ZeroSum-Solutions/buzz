# ZeroSum Buzz fork — implementation plan

Date: 2026-09-04. Source: `2026-09-04-zs-feature-audit.md` (Sol pass 1 applied). This plan turns
the seven asks into branches, tickets, bars and evals, and fixes the loop every ticket runs
through. Revision 2 folds in Sol's pass 1 on this plan (27 findings; log at the end).

## Branch and landing model

- `main` mirrors `upstream/main` and is never committed to. `zs/main` is the integration
  branch and the fork's GitHub default branch (set in step 0 so PRs and `zs-land` target it).
- One ticket = one branch `feat/<slug>`, `port/<pr>`, `fix/<slug>` or `spike/<slug>` off
  `zs/main` = one PR = one squash commit. DCO on every commit: `git commit -s`; for imported
  commits `git cherry-pick --signoff` and `git rebase --signoff`. Before push:
  `git log --format=%B origin/zs/main..HEAD | grep -c Signed-off-by` equals the commit count.
- Port tickets carry upstream parity only. Fork deviations on a ported feature are their own
  ticket, so an upstream merge can retire the port without losing fork work (range-diff the
  upstream merge against the port, then drop the port commit).
- Landing is serialized, one ticket at a time:
  1. `git fetch origin && git rebase --signoff origin/zs/main`; record `BASE=$(git rev-parse origin/zs/main)`.
  2. Rerun the ticket's gates and the Sol audit on the rebased branch.
  3. Open the PR ourselves with the full body (`gh pr create --base zs/main --body-file`):
     gates run and their results, the tested base OID, the Gemini verdict, the Sol verdict.
  4. `~/bin/zs-land`. It reuses the open PR, requires green CI, squash-merges, syncs `zs/main`.
     If `origin/zs/main` moved since `BASE`, stop and go back to step 1.
  5. If a CI job fails on the fork for a reason outside the diff (missing secret, upstream-only
     runner), the fix is a fork-only edit to that job on `zs/main`. Never `--allow-no-ci`.
- Hooks and differential lanes compare against `origin/main` (`AGENTS.md:128`,
  `scripts/check-file-sizes-core.mjs:44`), so on the fork they see the cumulative fork delta.
  That is stricter, not wrong. If a lane flags an upstream-ported file, the PR notes it and the
  fix is a fork-only base override in that script, done as its own tiny ticket.
- Upstream sync: `git fetch upstream && git checkout main && git merge --ff-only upstream/main`,
  then `git checkout zs/main && git merge main`, then `just ci`.

## The loop every ticket runs

1. **Builder** (Claude subagent; Sonnet 5 for ports and UI, Opus 5 for Rust plumbing and the
   MCP registry) implements on the branch in its own worktree. Test first where the ticket
   names a test.
2. **Fast gates** on the branch, all must pass before anyone else looks:
   ```
   just fmt-check clippy desktop-check desktop-tauri-fmt-check desktop-tauri-clippy file-size-check
   just desktop-test
   just desktop-tauri-test
   ```
   plus the ticket's own eval commands. Every Rust filter is run twice: once with `-- --list`
   piped to a count that must be at least the ticket's stated number, then for real. A filter
   that matches nothing is a failed gate, not a pass.
3. **Tester** (Gemini 3.8 Flash): a disposable worktree of the branch, run as
   `agy -p --model gemini-3.8-flash-high --mode accept-edits --sandbox "<ticket brief>"` from
   the worktree root. It reads the diff and the acceptance checks, runs the fast gates itself,
   tries to break the feature (wrong input, empty state, missing file, hostile filename,
   concurrent arrival), and returns PASS or FAIL with a repro, plus any acceptance check that
   has no test. After it returns, `git status --porcelain` in that worktree must be empty; any
   change it made is discarded and reported. A FAIL or a missing test goes back to the builder.
4. **Critic** (fresh-context Claude subagent, Opus 5, high effort) does the gauntlet
   comparison against the ticket's bar with labels stripped and reports one of: ours is better,
   the bar is better with the single biggest gap named, or parity. The exit checklist is finite:
   source parity with the bar (for ports: the PR's tests pass on our branch), every fork
   deviation named in the PR body, every acceptance check measurable and met, nothing above NIT
   open. Parity with an upstream PR counts as passing for a port. Otherwise back to the builder.
5. **Full gate** `just ci` on the rebased branch (it adds `test-unit`, `desktop-build`,
   `desktop-tauri-check`, `web-build`, `mobile-test`). Integration tests (`just test`) when
   relay, auth or db code changed.
6. **Audit** (GPT-5.6 Sol, `codex exec review -c model="gpt-5.6-sol" -c
   model_reasoning_effort="xhigh"`, adversarial prompt, scope "this branch against zs/main").
   Every BLOCK and WARN is verified against the code by the driver before it is fixed or
   discarded with a written reason. Re-run until nothing above NIT.
7. **Land** as above.

Concurrency: builders and critics are in-process agents (cap 10). Gemini and Sol are external
processes (cap 4, shared with anything else on the machine); at most two tickets sit in the
tester or audit stage at once. Fan out is by ticket, never by file inside a ticket.

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
  just desktop-e2e-smoke   # output lists mention-descriptions-screenshots
  ```
- Acceptance: typing `@` in a channel shows each agent's `about` under its name; agents with no `about` show none; the hover card still shows `about`.
- Bar: PR #2706's diff.

### R1 · runbook: set descriptions on the nine Broken English agents (operational, not code)

- Runs after T1 lands, with Devin's go-ahead in the session. Records before and after values per agent, the target relay, the emitted kind:0 event ids, and the rollback (re-publish the previous kind:0). Lives at `~/projects/clients/broken-english/buzz/runbooks/2026-09-xx-agent-descriptions.md`.

### T2 · port/6731 — markdown attachments open in a viewer panel (S)

- Branch `port/6731` (`block:fizz/md-viewer`). Cherry-pick with sign-off, resolve, keep tests.
- Tests that must exist and pass: `desktop/src-tauri/src/commands/media_download_tests.rs`, `desktop/src/features/channels/ui/ChannelPane.helpers.test.mjs`, `channelPaneAuxiliaryLayout.test.mjs`, `markdownDocFocus.test.mjs`, `desktop/src/shared/ui/markdown/markdownDocFile.test.mjs`, `desktop/tests/e2e/markdown-doc-viewer.spec.ts` (added to the smoke allow-list).
- Eval:
  ```
  cd desktop/src-tauri && cargo test media_download -- --list | grep -c ': test'   # >= 1
  cd desktop/src-tauri && cargo test media_download
  just desktop-test
  just desktop-e2e-smoke   # output lists markdown-doc-viewer
  ```
- Acceptance: clicking an `.md` attachment opens the panel; GFM tables and code render; a fixed 500 KB fixture (`desktop/tests/fixtures/long-doc.md`, hash recorded in the PR) reaches panel-ready in under 1.0 s with no main-thread task over 200 ms, measured three times with the Performance API from the e2e spec.
- Bar: PR #6731's diff.

### T3 · port/4316 — channel Files tab, parity (M)

- Branch `port/4316` (`mismai-li:feat/channel-files-tab`). Cherry-pick with sign-off, resolve. The PR ships no tests; the port adds them.
- Tests first: `desktop/src/features/channel-files/useChannelFiles.test.mjs` (imeta parsing, a Markdown attachment labeled by `filename` not `.bin`, newest-first order), `useFileFolders.test.mjs` (folder assignment round-trip). A smoke e2e spec `channel-files-tab.spec.ts` added to the allow-list.
- Eval: those files through the node runner, `just desktop-test`, `just desktop-e2e-smoke` listing the new spec.
- Acceptance: the tab lists the attachments in the loaded window with folders, bulk select and drag-drop as in the PR. Bulk upload stays as the PR ships it in this ticket; T3b changes the default.
- Bar: PR #4316's diff.

### T3b · feat/files-index — dedicated attachment index and bulk-upload default (M)

- After T3. `useChannelFiles` reads the loaded window (`useChannelMessagesQuery`), which is top-level only and 200 per page (`commands/channel_window.rs:19, 38`). This ticket adds an attachment index: open the live subscription first, then paginate the channel's history including reply kinds, keep only events with `imeta`, overlay edits and deletions, expose deterministic pagination. Bulk drag-drop moves behind a setting that defaults off with a batch cap of 20.
- Tests first: more than 250 entries paginate without loss, a reply attachment appears, an arrival during backfill appears once, a deleted message's file disappears, an interrupted backfill resumes without duplicates, the setting gates bulk drop.
- Eval: the index test file through the node runner with the stated six cases; `just desktop-test`; the Files tab with the 250-file e2e seed lists all files and renders in under 500 ms, three runs, measured in the spec.
- Bar: PR #4316's tab as the baseline it must not regress; issue #4428 as the failure it must close.

### T4 · port/6651 — extra MCP servers via env (S)

- Branch `port/6651` (`BradGroux:agent/extra-mcp-servers`, mergeable today). Cherry-pick with sign-off. The PR adds a `trusted` flag and withholds `BUZZ_PRIVATE_KEY`, `NOSTR_PRIVATE_KEY`, `BUZZ_RELAY_URL` and `BUZZ_AUTH_TAG` in `buzz-agent`'s `spawn_one` for untrusted servers.
- Tests that must exist and pass: the PR's `extra_mcp_commands_*` tests in `crates/buzz-acp/src/lib.rs` and `desktop/src-tauri/src/managed_agents/env_vars/tests.rs`.
- Test added by the port: an end-to-end spawn test in `crates/buzz-agent` that registers a fake MCP executable (a script that prints its environment variable names as its first tool result) as an untrusted server and asserts none of the four identity names appear, and that the trusted built-in still receives them.
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

- Branch `feat/prompt-source`. Machine-local sidecar `<app-data>/agents/prompt-sources.json` (definition id to absolute path). One backend command `set_prompt_source_and_reload(definition_id, path: Option<String>)`: `None` clears the mapping; `Some` validates the path (inside the user's home after symlink resolution, UTF-8, at most 64 KiB), stores the mapping, reads the file, and submits through the existing update request (`types/requests.rs:103`) so validation, the kind:30175 publish and the persona hash follow the normal path. It returns `{ local_updated: bool, publish: "published" | "queued" | "failed:<reason>" }` because the update path can return after queueing (`commands/personas/update.rs:124`). The dialog gets a path field, Reload and Clear.
- Runtime capability fact: prompt-from-file is a desktop feature, not a harness capability, so `KnownAcpRuntime` is unchanged; the PR says so in `desktop/src/features/agents/AGENTS.md` per its rule at line 382.
- Tests first (Rust, module `managed_agents::prompt_source`): missing file, symlink outside home, over-limit, invalid UTF-8, clear removes the mapping, happy path updates `system_prompt` and changes `persona_content_hash`, and the emitted kind:30175 event deserializes to `PersonaEventContent` whose content contains the prompt text and no path string. Frontend test: Reload disabled when no path is set.
- Eval:
  ```
  cd desktop/src-tauri && cargo test prompt_source -- --list | grep -c ': test'   # >= 7
  cd desktop/src-tauri && cargo test prompt_source
  just desktop-test
  ```
- Acceptance: edit `agent-prompts/pm.md`, click Reload, restart the agent, and `BUZZ_ACP_SYSTEM_PROMPT` in the agent's process environment equals the file.
- Bar: `custom_harnesses.rs` for the file-input validation posture; upstream's `AgentDefinitionDialog.tsx` for the dialog pattern.

### T6 · OpenSEO through runtime config (S, blocked on vendor approval)

- Precondition: Devin approves DataForSEO as a metered vendor and sets a monthly spend limit in the DataForSEO dashboard, and the key is added with `zsvault add`. Before that, nothing of OpenSEO runs. What may run before approval: config generation validated against a fake MCP server (the T4 fixture) to prove the discovery path.
- Config placement, verified against the config bridge: Claude reads `mcpServers` from `<CLAUDE_CONFIG_DIR>/.claude.json` (`config_bridge/claude.rs:3-18`, `reader.rs:270`) or a project `.mcp.json` in the spawned working directory (`~/.buzz`, `runtime.rs:563`); Codex reads `<CODEX_HOME>/config.toml`. The ticket uses per-agent `CLAUDE_CONFIG_DIR` and `CODEX_HOME` set on the child so the operator's own config is untouched.
- Steps after approval: run OpenSEO self-hosted (`docker`, `AUTH_MODE=local_noauth`, bound to localhost); add the MCP to the Copy and Audit agents' runtime config; install `plugins/openseo/skills/` at a pinned revision into the agent skill path.
- Eval, before approval: spawn a Claude runtime agent from a Dock launch with the fake server in its config; the agent lists the fake tool. After approval: same with OpenSEO; the agent runs one site-audit tool (locally computed, no DataForSEO call) on brokenenglishjewelry.com and the reply names the audit id; the smoke script asserts no DataForSEO-billed tool was called by reading the OpenSEO container log.
- Acceptance: both smoke tests pass for the Claude and Codex runtimes from a Dock launch.
- Bar: OpenSEO's documented `plugins/openseo` install path.

### T7 · feat/mcp-registry — MCP registry with trust model (L)

- Branch `feat/mcp-registry`, after T4. Depends on the T4 fake-server fixture and a fake HTTP MCP fixture (a tiny Streamable HTTP server in the test tree). OpenSEO is not a dependency; the T6 post-approval run is a separate integration check.
- Design memo first, `docs/plans/2026-09-xx-mcp-registry-design.md`, half a page, reviewed by Sol before code. It must answer: the two server classes and the env each receives; the runtime capability matrix (buzz-agent: stdio only, since `McpServer` is the ACP `McpServerStdio` shape at `buzz-acp/src/acp.rs:25` and `buzz-agent/src/types.rs:536`; Claude and Codex: stdio and HTTP through native config); the process boundary for stdio servers under Claude and Codex, where the adapter inherits Buzz identity variables (`runtime.rs:563`, `buzz-acp/src/acp.rs:454`) and its MCP children inherit them again, solved by a small launcher binary `buzz-mcp-launch` that the generated config names as the command, which removes the four identity variables and execs the real server; where secrets live (keychain references resolved by the launcher at spawn, never written into JSON or TOML); name collision rule with built-ins; per-agent toggle storage; isolated config roots per agent.
- Capability facts added to `KnownAcpRuntime` (`mcp_transports`, `mcp_config_root_env`) and projected through core to the UI per `desktop/src/features/agents/AGENTS.md:13, 34`; the guide is updated in the same PR.
- Then: `mcp_servers.json` schema and loader with `custom_harnesses`-style structure validation; Settings panel to add stdio and HTTP servers with an approve step that shows the exact command or URL; per-agent toggles in the definition dialog; generation of `BUZZ_ACP_EXTRA_MCP_COMMANDS` for buzz-acp and of native config for Claude and Codex under per-agent `CLAUDE_CONFIG_DIR` and `CODEX_HOME`.
- Tests first (Rust, module `managed_agents::mcp_registry`, and the launcher crate): loader rejects a server named like a built-in, rejects an inline secret value, resolves a keychain reference; HTTP entry is refused for buzz-agent and accepted for Claude and Codex; generated Claude and Codex config round-trips through the config bridge readers; launcher end-to-end: spawned with the identity variables set, the fake server it execs reports none of them; the toggle changes only the named agent's generated config. Frontend test: approve step required before save.
- Eval:
  ```
  cd desktop/src-tauri && cargo test mcp_registry -- --list | grep -c ': test'   # >= 8
  cd desktop/src-tauri && cargo test mcp_registry
  cargo test -p buzz-mcp-launch
  just desktop-test
  ```
- Acceptance: add the fake stdio server from Settings, toggle it on for Copy only; Copy lists its tool and Brand does not; add the fake HTTP server, it is enabled only on runtimes whose matrix allows it; delete a server and the agent stops seeing it after restart.
- Bar: PR #5321 for the schema shape; `custom_harnesses.rs` for the loader.

### T8 · spike/pdf — PDF route decision (S)

- Branch `spike/pdf`, throwaway. Render one real Broken English document (an approval page from `brand/render-approval.py`, with a table, a code block, a remote image, and enough text for three pages; fixture hash recorded) through (a) webview print-to-PDF behind a Tauri command and (b) one Rust HTML-to-PDF crate.
- Validation per output, scripted: `pdftotext` extracts the three headings, the table cells and the code line; `pdftoppm` renders every page to PNG without error and the page count is 3; size and wall time recorded; the offline run executes with outbound network blocked (`pfctl` rule or the process launched with no network entitlement) and the remote image is reported as placeholder or fetched-from-cache.
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
  just desktop-e2e-smoke   # pdf-export.spec.ts listed
  ```
- Acceptance: from the T2 viewer, Export PDF saves a file that opens in Preview with headings, table and code intact; hash of the output for the fixture recorded in the PR.
- Bar: the same document printed from the macOS Chrome print dialog, compared page by page.

### T10 · feat/assets-facets — sort, filter and documents facet on the Files tab (M)

- Branch `feat/assets-facets`, after T3b. Sort by date, name, size, author; filter by type; a Documents facet (md, pdf, html, docx, csv); the channel canvas pinned at the top, opening the existing Canvas surface (`features/canvas/ChannelCanvas.tsx`), not the attachment viewer, so editing keeps working.
- Tests first: facet classification from `imeta` `filename` and MIME (md-as-octet-stream case), sort stability, pinned canvas present only when the channel has one, pinned row opens the Canvas surface and an edit saves.
- Eval: the facet test file through the node runner, `just desktop-test`, `just desktop-e2e-smoke` listing the spec; sort or filter over the 250-file seed completes in under 100 ms measured in the spec, three runs.
- Bar: the T3b tab as the baseline it must not regress.

### T11 · docs/calendar-authz — calendar authorization contract (S)

- Memo at `docs/plans/2026-09-xx-calendar-authorization.md`. Checklist it must answer, each with a decision and a reason: OAuth scopes requested; which Google account binds to which Buzz identity and how the binding is stored; who owns the business calendar and how sharing is granted; which channels show it and who chooses; disconnect behavior; cached event data on disconnect and on membership loss; revocation propagation timing; refresh-token failure UX; what an agent may read or write.
- Eval: every checklist item has a decision; Sol reviews the memo; nothing above NIT remains.
- Bar: RFC #3227's scoped-integration shape.

### T12a · docs/calendar-view-design — event model and view design (S)

- After T11. `shared/ui/calendar.tsx` is a `react-day-picker` wrapper with no event layout. Memo: event model (all-day, multi-day, timezone, recurrence expansion window), month and agenda rendering, paging, keyboard operation and screen-reader semantics, create and edit conflict handling, and the component or library chosen. Reviewed by Sol.

### T12 · feat/google-calendar — Google Calendar as a scoped integration (L)

- Branch `feat/google-calendar`, after T11 and T12a. Revive PR #1382's approach: per-user OAuth through desktop commands, tokens in the keychain (`secret_store` pattern), the T12a view, create and edit for events the account can edit, a sidebar entry gated by the T11 contract.
- Tests first (Rust): token storage and refresh; refresh failure surfaces a reconnect state; no token in logs. A mock Google Calendar server in the test tree with two principals, one calendar shared to both, and an ACL-loss case; frontend tests render both principals' views and disable edit when write ACL is absent.
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
  cd desktop/src-tauri && cargo test discovery::tests::bundle_exe_prefers_bundle_over_workspace_target -- --exact
  cd desktop/src-tauri && cargo test discovery
  just desktop-tauri-clippy
  ```
- Bar: upstream's `command_search_dirs` semantics for dev builds unchanged.

## Order and parallelism

Wave 1 (parallel build; serialized landing): T1, T2, T4, T5, T13, T8, T11.
Wave 2: T3 and T3b (after wave 1 lands), T7 memo then T7 (after T4), T12a (after T11), T6 config-generation half (fake server; the OpenSEO half waits for approval).
Wave 3: T9 (after T2 and T8), T10 (after T3b), T12 (after T12a).

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

## Review log

- Sol pass 1 on this plan (2026-09-04, gpt-5.6-sol, xhigh): verdict BLOCK, 18 BLOCK and 8 WARN findings, one provenance NIT. All checked against the code.
  - Accepted and applied: `just ci` as the full gate (1); tester in a writable worktree with a clean-tree assertion (2); serialized landing with rebase, re-gate and recorded base OID (3); PR body written by us before `zs-land` (4); exact node test invocation (5); relay mutation moved to runbook R1 (6); Rust commands from `desktop/src-tauri` (7); named test files and smoke allow-list entries (8); dedicated attachment index as T3b (9); spawn-boundary end-to-end test (10); explicit set, reload and clear with publish status (11); Claude config file names (12); nothing of OpenSEO runs before approval (13); runtime capability matrix, HTTP not on buzz-agent (14); launcher as the process boundary (15); T7 depends on fixtures, not OpenSEO (16); PDF parse and render checks (17); T11 checklist and T12 mock plus live checklist (18); parity and deviation split (19); `--signoff` on imports (20); canvas opens its own surface (21); T12a design ticket (22); list-and-count on every Rust filter (23); hook base noted as stricter cumulative (24); latency thresholds with fixtures and three runs (25); finite critic exit and corrected provenance (26); capability facts in the runtime catalog (27).
  - Corrected on verification: finding 10 described the tree before the port. PR #6651 patches `buzz-agent`'s `spawn_one` with a `trusted` flag and withholds the four identity variables from untrusted servers. The end-to-end test Sol asked for is kept because it binds that seam.
- Sol pass 2: running at commit time; findings are folded in during execution rather than blocking wave 1 (Devin, 2026-09-04: if the plan looks solid, troubleshoot during implementation).
- Step 0 baseline: `just ci` exit 0 on `zs/main` at 63a169e52 (2026-09-04).
