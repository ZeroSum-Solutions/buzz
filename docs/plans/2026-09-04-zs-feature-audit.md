# ZeroSum Buzz fork — feature audit (code, vision, upstream)

Date: 2026-09-04. Scope: the seven asks in `2026-09-03-zs-feature-backlog.md`, audited against
the fork at upstream `4b0744d7f` (main, 2026-09-04), Block's `VISION*.md`, upstream PRs and
issues from the last 90 days, and the two reference clones (`~/projects/third-party/open-seo`,
`~/projects/third-party/cal.com`). Constraint that shapes everything: the team runs on Block's
hosted relay, which only runs upstream code. Anything relay-side must land in `block/buzz` first.

Reviewed by GPT-5.6 Sol (pass 1, 13 findings). Every finding was checked against the code; the
corrections are folded into the sections below and the log is at the end.

## Summary table

| # | Ask | Where it lives | Relay change? | Upstream state (2026-09-04) | Fork decision | Effort |
|---|---|---|---|---|---|---|
| 1 | Agent hover cards + `@` picker description | `desktop/src/features/messages/ui/MentionAutocomplete.tsx`, `features/profile/ui/UserProfilePopover.tsx`, `src-tauri/src/nostr_convert.rs` | No | PR #2706 open, CONFLICTING with main | Port #2706 onto `zs/main`; hover card already works | S |
| 2 | Assets tab | new `desktop/src/features/files/` (upstream branch `feat/channel-files-tab`), `imeta` tags on messages | No for the list; relay quotas are Block's to add | PR #4316 open, CONFLICTING with main | Port #4316; add sort, filter, type facets; keep bulk upload off by default | M |
| 3 | Markdown docs as styled pages, PDF export | `desktop/src/shared/ui/markdown.tsx` (chat renderer, `interactive` flag), `src-tauri/src/commands/export_util.rs` (save dialog only) | No | PR #6731 open, CONFLICTING with main; no PDF work anywhere | Port #6731; PDF needs a spike before a backend is chosen | M-L |
| 4 | Shared editable business calendar | none shared today; `/reminders` route and `shared/ui/calendar.tsx` exist; CSP in `src-tauri/tauri.conf.json:39` | Yes for a native shared kind; no for an external-calendar view | PR #1382 (Google Calendar) closed for inactivity | Google Calendar as a scoped integration; authorization design is the first ticket | L |
| 5 | Plugins and MCP connections | `crates/buzz-acp/src/lib.rs:5730` (`build_mcp_servers`), `managed_agents/config_bridge/*`, `custom_harnesses.rs` | No | PR #6651 open, MERGEABLE; #6133 draft; #5321 open | Port #6651; build a registry with an explicit trust model | M-L |
| 6 | OpenSEO | its MCP at `/mcp` (Streamable HTTP, `oseo_` API key) + 9 curated skills in `plugins/openseo/skills/` | No | none | Blocked on DataForSEO vendor approval; then runtime-native config + smoke test | S |
| 7 | Prompt files as source of truth | `AgentDefinitionDialog.tsx:783-791` textarea; `definition_validation.rs` (64 KiB cap); `persona_events.rs` (prompt is published in kind 30175) | No | PR #584 merged (base prompt layer only) | Local "reload from file" action that writes through the normal update path | S |

All three CONFLICTING PRs need a port: replay the PR's full commit range onto `zs/main` with
`git cherry-pick --signoff`, resolve conflicts, re-run their tests. A clean pick will not apply.
Each port is judged against the PR's own diff and test files as the bar.

## 1. Agent hover cards and mention descriptions

- The hover card already exists and already shows a description: `UserProfilePopover.tsx:344-345, 421-428` renders the profile's kind:0 `about`. Nothing to build there.
- The `@` picker (`MentionAutocomplete.tsx:332-393`) shows badges but no description because the batch profile summary (`shared/api/types.ts:118-128`, `nostr_convert.rs:332-373`) drops `about`. Upstream PR #2706 adds exactly this: `about` as a one-line role under the name. It conflicts with current main, so the work is a port.
- Two remote-readable sources exist for the description. Kind:0 `about` is the interoperable one and the one the mention summary query can reach today. `AgentDefinition.description` is also public and travels on the kind:30175 persona event (`managed_agents/types.rs:20`, `persona_events.rs:95`); it is excluded from the persona hash and from the kind:30177 instance event. The picker should read `about`; the desktop already publishes the effective description into `about` (`agent_description.rs`, `commands/agents_profile.rs:104`).
- Action for Broken English today, no code: set a description on each of the nine agents so the `about` field carries the "what it is for" line from the canvas table.

## 2. Assets tab

- Correction to the backlog: the relay accepts PDFs, DOCX, HTML, CSV and plain text on `PUT /upload` (`crates/buzz-media/src/validation.rs:87-171`, deny-list of SVG, JS and executables). Only the legacy `/media/upload` alias is images-only.
- Filenames survive. The client sends `x-buzz-filename` (`shared/api/tauriMedia.ts:15`), Rust sanitizes and keeps it on the descriptor (`commands/media.rs:783`), and the outbound `imeta` tag carries `filename` (`features/messages/lib/imetaMediaMarkdown.ts:89, 273`). Only the content-addressed blob URL and the sniffed MIME fall back to `.bin` and octet-stream for Markdown (`validation.rs:193`). Classify documents from the `imeta` filename; there is no upload fix to make.
- Non-image files are served as attachments, never inline (`validation.rs:228`).
- No files view exists in the fork's base. Upstream PR #4316 adds a channel Files tab with folders, bulk select and drag-drop; issue #4428 documents its 250-entry truncation. Attachments live as `imeta` tags on messages, so a client-only index is a REQ over synced history.
- Storage governance: the relay has a per-file size cap but no durable per-pubkey quota (`crates/buzz-relay/src/api/media.rs:318` TODO, `validation.rs:175`). Bulk drag-drop widens that surface. The team runs on Block's hosted relay, so the quota belongs upstream; the fork keeps bulk upload behind a setting that defaults off and caps a batch client-side.
- Notes (kind 30023) are per-author and never rendered in the desktop; canvas (kind 40100) is per-channel and editable by any channel member. Neither is a document library on its own.
- Fork plan: port #4316, add sort and filter by type, author and date, a "documents" facet (md, pdf, html, docx), and read the canvas as the pinned document at the top of the tab.

## 3. Documents: styled pages and PDF

- Rendering in chat is solved: `shared/ui/markdown.tsx` runs react-markdown with GFM. Upstream PR #6731 opens markdown attachments in a viewer panel; port it.
- The chat renderer is not a document renderer. In non-interactive mode links become spans (`markdown.tsx:1236`), attachments become file cards (`:1283`), and code blocks can collapse (`:1546`). `export_util.rs` only picks a save path and writes bytes it is handed (`:4, :40`); nothing in the tree produces a PDF.
- PDF therefore starts with a spike, not a backend choice. The spike renders one real Broken English document (headings, tables, code, remote images, a long page) through the two candidate routes, webview print-to-PDF behind a Tauri command and a Rust HTML-to-PDF crate, and records pagination, fonts, offline behavior and output on macOS. Output still goes through `export_util.rs`, the established save pattern.
- Vision fit: canvases as living documents is explicit (`VISION_PROJECTS.md:191-193`); PDF is unmentioned, neutral.

## 4. Shared business calendar

- No shared calendar data model exists. What does exist: a `/reminders` route (`app/routes.ts:7`) backed by `KIND_EVENT_REMINDER` (30300, author-only, encrypted), scheduled messages (40006, delivery mechanics), and a DayPicker component (`shared/ui/calendar.tsx`).
- A native shared calendar kind is the idiomatic Buzz answer and is ruled out for now by the hosted relay. The relay cost is smaller than first stated: new kinds must be allow-listed for scope (`crates/buzz-relay/src/handlers/ingest.rs:434`) and generic parameterized-event storage already exists (`ingest.rs:3147`); canvas shows a channel-scoped kind on the generic path (`ingest.rs:518, 706`). A dedicated table or side effects are needed only if recurrence, conflict detection or date-range queries demand them. Still upstream-first.
- Cal.com does not fit the ask, for a narrower reason than "booking only": the community edition in the clone does expose unified-calendar CRUD over connected calendars (`apps/api/v2/src/modules/cal-unified-calendars/controllers/cal-unified-calendars.controller.ts:44-167`), but every endpoint is scoped to one user's connection, and team features are stripped from the edition. It adds a second service to host without giving a shared calendar. Keep Cal.com for bookable slots only.
- Google Calendar fits the "business calendar" as the business already runs on Google Workspace. PR #1382 built per-user OAuth through desktop commands, tokens in the keychain, events in the sidebar; it was auto-closed for inactivity, not rejected on design.
- Authorization is the first design decision, not a footnote. Buzz's only access gate is channel membership (`VISION.md:35`, `h`-tagged kinds at `ingest.rs:706`). A Google calendar has its own ACL. The fork does not try to reconcile the two: the calendar's owner and sharing live in Google Workspace, Buzz shows each signed-in user exactly what their Google account can see and edit, and removing someone from the business means removing them in Google. The plan writes that contract down, covers token-refresh failure and disconnect, and shows the calendar only in channels the owner chooses.
- Iframes are blocked by CSP (`default-src 'self'`, no `frame-src`), and Google Calendar refuses framing anyway. The view must be native.
- Vision tension: `VISION.md:9` argues against stitching in outside services. RFC #3227 (app-integration agents with scoped credentials) is upstream's sanctioned shape for outside data. Build the calendar as a scoped integration, not a platform primitive.

## 5. Plugins and MCP connections

- Today one MCP server, hardcoded: `build_mcp_servers` (`buzz-acp/src/lib.rs:5730`) returns at most one `McpServer` from a single command string, no args, and forwards `BUZZ_RELAY_URL`, `BUZZ_PRIVATE_KEY` and two more variables (`:5743`). That server is trusted: it holds the agent's identity. Only the Codex and buzz-agent runtimes receive it (`discovery/catalog.rs`).
- Runtime-native configuration is global, not per nest. `~/.buzz` is only the spawned process's working directory (`managed_agents/runtime.rs:563`). The config bridge reads Claude from `~/.claude/settings.json` and `~/.claude.json` unless `CLAUDE_CONFIG_DIR` is set (`config_bridge/claude.rs:3-18`), Codex from `~/.codex/config.toml` unless `CODEX_HOME` (`codex.rs:125`), Goose from `~/.config/goose/config.yaml` unless `GOOSE_PATH_ROOT` (`goose.rs:157`). So "add a server to the runtime's own config" means the operator's global config, or a project-level `.mcp.json` in the working directory for Claude, and either needs a spawned-runtime smoke test before it counts as working.
- No registry UI or storage exists. `custom_harnesses.rs` is a JSON-drop registry for whole runtimes; its validation is structural (non-empty command, id shape, no install commands or remote icons, `:42, :158`). It is a pattern for file layout, not a security boundary.
- Upstream is converging: PR #6651 (mergeable) adds `BUZZ_ACP_EXTRA_MCP_COMMANDS`, shell-split entries with optional `name=` prefix, and, on the buzz-agent spawn path, passes extra servers an empty env so they never see `BUZZ_PRIVATE_KEY`. Its own docs note that third-party ACP adapters (claude-agent-acp, codex-acp) inherit the full parent env, so their MCP children can read Buzz credentials unless the adapter isolates them. #6133 makes buzz-acp multi-server; #5321 proposes a unified config model; RFC #5012 proposes community-level MCP announcements; issues #6023 and #6117 are the user-facing bugs.
- Trust model for the fork registry: two classes. Built-in servers (buzz-dev-mcp) keep the identity env. External servers get no Buzz variables, only per-server env references resolved from the keychain at spawn, an explicit executable or URL that the user approved in the dialog, and a name that cannot collide with a built-in. Registry JSON never stores a secret value. Per-agent toggles are the unit of enablement. Process bounds and revocation are the same as for any managed-agent child.
- Fork plan: port #6651 as the transport, add `mcp_servers.json` with the layout rules of `custom_harnesses`, a Settings panel to add HTTP and stdio servers, per-agent toggles, and generation of the env for buzz-acp plus native config for Claude and Codex runtimes under an isolated config root per agent (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`).
- Vision fit: strongest of the seven (`VISION_AGENT.md:32`, protocols not imports).

## 6. OpenSEO

- MIT, TypeScript on Cloudflare Workers. The integration surface is the MCP server at `/mcp` (Streamable HTTP; API key `oseo_…` as `x-api-key` or bearer; OAuth also offered) with about 45 tools: projects, keyword research and metrics, domain overview, backlinks, SERP, rank tracking, local SEO, Search Console, ten GA4 tools, site audits.
- Skills: `.agents/skills/` holds 20 skills including repository-internal ones. The curated set for agents is the nine copied by `scripts/sync-plugin-skills.mjs` into `plugins/openseo/skills/`. Install that tree, pinned to a reviewed revision; never copy `.agents/skills/` wholesale into agent context.
- No public REST API; the web app is a TanStack Start SPA with no framing headers, embeddable only from a local self-host in `local_noauth` mode.
- Hosting: hosted plan is ten dollars a month plus metered DataForSEO with a 28 percent markup; self-host is one Docker container bound to localhost, needing only a DataForSEO key. Site audits are computed locally at no per-call cost; keywords, backlinks, SERP, rank tracking and AI visibility are DataForSEO calls.
- Billing gate: DataForSEO is a metered vendor that is not on the approved list in `~/.claude/billing-lanes.md`. The house rule requires explicit approval before the first call. The MCP server's own instruction only asks before batches above 2,000 credits (`src/server/mcp/server.ts:151`), which is neither first-call approval nor a cap. Activation is blocked until Devin approves the vendor and a monthly limit is set on the DataForSEO account itself (their dashboard supports spend limits); a prompt-level budget line is a reminder, not a control.
- Fork plan: step one is zero fork code. After approval, register the OpenSEO MCP in the Claude and Codex runtime config for the Copy and Audit agents, install the nine skills, run the spawned-runtime smoke test (agent lists the tools, runs one free site-audit tool). Step two is the same server through the #5 registry.

## 7. Prompt files as source of truth

- `system_prompt` is not a local field. It rides the create and update APIs (`types/requests.rs:74`), is published on the kind:30175 persona event so teammates can adopt the agent (`persona_events.rs:68, 523`), feeds the persona content hash (`:509`), and is validated at 64 KiB with an invisible-character screen (`definition_validation.rs:21`). A path stored in the definition would either leak a machine path to the team or leave the published prompt stale.
- `resolve_effective_config` is synchronous with no file I/O failure state (`effective_config/mod.rs:74`); reading a file there would add a failure mode on every spawn.
- Fork plan, smaller than a resolver change: a machine-local sidecar (`<app-data>/agents/prompt-sources.json`, definition id to path) plus a "Reload from file" button in `AgentDefinitionDialog.tsx`. Reload reads the file, rejects non-UTF-8, symlink-outside-home and over-limit content, and submits through the normal update command, so validation, the persona event and the hash all stay correct. Spawn re-pins from the definition as it does today. Live watching is a later step.
- Import never updates in place (`snapshot/import.rs:559` mints a new id), so snapshots are not a reload path.

## Cross-cutting findings

- Harness discovery prefers the developer workspace `target/release` over the bundle (`discovery.rs:363-370`, `command_search_dirs`). Fix in the fork: prefer the bundle when the exe is inside an `.app`.
- `CONTRIBUTING.md` lists "How to Add a New MCP Tool" in its table of contents but the section does not exist. Upstream doc bug; note it in the PR that adds the registry.
- Three of the four upstream PRs to adopt conflict with main as of today. Budget each as a port with the PR's tests as the acceptance bar.

## Order of work (proposed, for the plan)

1. Port upstream PRs #2706, #6731, #4316, #6651 onto `zs/main`, one branch each.
2. Prompt reload from file (#7). Small, unblocks the Broken English workflow.
3. OpenSEO through runtime config (#6, step one). Blocked on vendor approval; the smoke test is the deliverable.
4. MCP registry (#5) on top of #6651, trust model first.
5. PDF spike, then PDF export (#3); Assets tab facets (#2).
6. Google Calendar (#4): authorization contract, then the integration reviving #1382.

## Review log

- Sol pass 1 (2026-09-04, `codex exec`, gpt-5.6-sol, xhigh): verdict BLOCK, 13 findings (6 BLOCK, 6 WARN, 1 NIT). All checked against the code.
  - Accepted as written: 1 (30175 carries `description`), 2 (filenames preserved in `imeta`), 4 (PDF needs a spike), 5 (`/reminders` and calendar component exist; generic relay storage), 6 (Cal.com CE has per-user calendar CRUD), 7 (calendar authorization contract), 8 (runtime config is global, `~/.buzz` is cwd only), 10 (nine curated skills, not `.agents/skills/`), 11 (DataForSEO needs vendor approval and an account-level cap), 12 (prompt path must not enter the shared definition), 13 (log).
  - Accepted with lower severity: 3 (relay quotas). The TODO is real, but the relay is Block's hosted service; the fork mitigates client-side and leaves the quota upstream.
  - Accepted with lower severity: 9 (MCP identity leak). #6651 already passes extra servers an empty env on the buzz-agent path; the residual risk is the ACP-adapter inheritance, which the registry's trust model addresses.
  - Under-weighted risks Sol named (identity via MCP, calendar ACL drift, resource limits as prose) are now explicit in §4, §5 and §6.
