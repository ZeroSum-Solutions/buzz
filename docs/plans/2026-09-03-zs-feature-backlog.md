# ZeroSum Buzz fork — feature backlog

Status: captured 2026-09-03, not yet audited against the code. Next step is an audit pass
per idea (where it lives in the codebase, upstream vision fit, effort), then an
implementation plan with evals and tickets. Nothing here is scheduled.

## Fork layout

- `origin` = `ZeroSum-Solutions/buzz`, `upstream` = `block/buzz` (Apache 2.0).
- `main` mirrors upstream `main`. Never commit to it. Sync: `git fetch upstream && git checkout main && git merge --ff-only upstream/main && git push origin main`.
- `zs/main` is the integration branch: upstream `main` plus every ZeroSum feature and this doc. Rebase or merge upstream into it on each release we want.
- Feature branches come from `zs/main` as `feat/<slug>`. Keep each feature in its own files where possible so rebases stay cheap. Anything upstream would take goes to them as a PR (DCO sign-off, conventional commit title, issue first).
- Dev: `. ./bin/activate-hermit && BUZZ_RELAY_URL=wss://mishmash.communities.buzz.xyz just desktop-standalone`. Dev build = "Buzz Dev", identifier `xyz.block.buzz.app.dev`, own data dir and Keychain entry. Sign in with the existing key via the onboarding import form.
- Hermit pins Flutter too (`bin/flutter`), so the mobile app builds from the same checkout.

## Goal

Alex and Meagan adopt the agents. Everything the team needs is visible inside the app,
not on Devin's disk.

## Ideas (unaudited)

| # | Idea | First guess at where it lives | Notes |
|---|---|---|---|
| 1 | **Agent hover cards.** Hovering an agent name, and the `@` mention picker in the composer, shows a short description of what the agent is for. | `desktop/src/features/agents/` (persona description), composer mention picker under `desktop/src/features/` | Description text exists per agent in Buzz already (quick-glance line); check whether the persona kind carries it and whether the mention picker can read it. Likely upstreamable. |
| 2 | **Assets tab.** A channel-level (or community-level) view that lists every document and file, sortable and filterable, viewable in-app. | New feature folder; data from relay media store + notes (kind for notes/canvas) | Relay file store accepts images only today (verified 2026-09-02); markdown lives as Buzz notes. Needs a decision: extend the relay's accepted types (server change, hosted relay will not run it) or render markdown notes to HTML/PDF client-side. |
| 3 | **Document rendering.** Documents authored as markdown, shown as a styled page (HTML) and exportable as PDF, all inside the app. | Client-side renderer over notes; export via Tauri | Pairs with #2. The Broken English design kit (`brand/`) already renders md → HTML → PNG; reuse the approach. |
| 4 | **Shared business calendar.** Everyone can open and edit the team calendar in-app. | New feature; either a calendar event kind on the relay or a Google Calendar integration through an agent/MCP | A relay kind means a server change. A client-side Google Calendar integration keeps the hosted relay untouched. Decide at audit. |
| 5 | **Plugins and MCP connections.** An easy way to add MCP servers and plugins, "everything is a plugin". | `buzz-dev-mcp` crate, agent env/config in `desktop/src/features/agents/`, CONTRIBUTING §How to Add a New MCP Tool | Check what upstream already has for per-agent MCP config and the VISION_AGENT.md direction before designing. |
| 6 | **OpenSEO integration.** `every-app/open-seo` (MIT, TypeScript, 16.7k stars): keyword research, rank tracking, competitor insights, backlinks, site audits, AI visibility. Exposes an MCP server and agent skills. Needs a DataForSEO key; self-hosts on Docker or Cloudflare. | Via #5: register the OpenSEO MCP server for the Audit/Copy agents; optionally embed its UI as a tab | Cheapest first step is MCP only, no UI work. Direct fit for the Broken English SEO lanes. |
| 7 | **Prompt files as source of truth.** Agents load instructions from a file on disk, with a reload-all action. | `desktop/src/features/agents/ui/AgentInstanceEditDialog.tsx` and the Tauri managed-agent commands | Fixes the 2026-09-02/03 pain: instructions can only be pasted through the UI. Likely upstreamable. |

## Also to set up

- Mobile app (Flutter) build from this checkout, paired to the same relay.
- Signed release build for the team: Apple Developer membership, or teammates open an unsigned app once. Updater endpoint is empty upstream; decide hand-delivered DMG vs a self-hosted manifest.

## Process from here

1. Audit each idea against the code and the `VISION_*.md` docs (repo `CLAUDE.md` requires it).
2. Implementation plan with evals per feature and tickets.
3. Execute feature by feature on `feat/*` branches off `zs/main`.

## Findings from the first install (2026-09-03, 23:45 PT)

- **Installed build = fork.** `/Applications/Buzz.app` is the fork's 0.5.21 release bundle (ad-hoc signed; one-time Keychain "Always Allow" on first launch). Block's 0.5.20 is at `~/Backups/buzz/Buzz-0.5.20-block-release.app`. Rebuild: `cargo build --release` for the six sidecars, copy into `desktop/src-tauri/binaries/`, then `pnpm tauri build --features mesh-llm --target aarch64-apple-darwin` in `desktop/`; quit Buzz, `ditto` the bundle over `/Applications/Buzz.app`, relaunch from the Dock.
- **Harness discovery order** (`desktop/src-tauri/src/managed_agents/discovery.rs`, `command_search_dirs`): the app looks in the compile-time workspace's `target/release` before its own bundle, so on this machine the installed app runs whatever `~/projects/buzz/target/release/buzz-acp` is. Candidate fork fix: prefer the bundle dir when `current_exe` is inside an `.app`.
- **Launch from the Dock, never `open -a Buzz` from an agent shell.** `open` propagates the caller's environment, and every agent child inherits it (seen once: the whole Claude Code shell env, API keys included). A clean relaunch is `env -i HOME=… PATH=/usr/bin:/bin:/usr/sbin:/sbin /usr/bin/open -a /Applications/Buzz.app`.
- **Agent prompts from files works.** Definitions in `managed-agents.json` are read at boot and re-pinned onto each instance on spawn (`commands/agents.rs`), so editing the store while Buzz is quit applies cleanly. Script: `~/projects/clients/broken-english/buzz/apply-agent-prompts.py`. Backlog item 7 becomes "do this inside the app".
