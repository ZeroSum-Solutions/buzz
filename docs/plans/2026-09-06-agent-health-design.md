# Agent health: ledger sync, Health tab, alerts, CLI

Ticket: T17 · `feat/agent-health` · desktop (Rust commands + React), `buzz-acp` observer frames, `buzz-cli`.
Depends on T16 (the harness ledger is the source of truth).
Status: design, 2026-09-06. Audited by GPT-5.6 Sol before the decisions were put to Devin.

## Goal

Answer "how are my agents doing" without reading raw logs: an aggregated failed-task list, per-agent counters, and an alert when something needs Devin. Today the only records are per-agent text logs, a `last_error` field that clears on restart, and observer frames that drive the working indicator.

## Source of truth

The T16 ledger files in each agent's state dir. Observer frames can be dropped in a crash and the desktop database can be deleted, so both are rebuilt from the ledger, never the other way round.

## Components

### 1. Observer frames (`buzz-acp`)

The harness emits one observer event per ledger record kind that matters live: `turn_failed` (with `class`), `batch_parked`, `batch_replayed`, `batch_needs_review`, `agent_paused`, `agent_resumed`, `breaker_opened`, `breaker_closed`, `relay_reconnected`. Same envelope as today's frames (`ObserverEvent`), same batching, same `#p` addressing to the owner. Payloads carry ids and counts, never message text.

### 2. Health store (desktop, Rust)

`agent_health.rs` beside `observed_unread.rs`: SQLite file `agent-health.db` in the app data dir, one table `health_events` (agent, at, kind, batch_id, channel_id, class, payload JSON) with indexes on (agent, at) and (kind, at), 30-day retention pruned on open and daily.

Two writers, one rule: a live observer frame inserts by (agent, seq) with `INSERT OR IGNORE`; `sync_health_ledger(agent)` reads the ledger file for a locally managed agent and inserts every record the table lacks. Sync runs on app start, on agent start and stop, and when the Health tab opens. A remote-owned agent (no local ledger) only gets frames, and the tab says so.

Commands: `get_agent_health_summary(since)`, `get_agent_health_events(agent, kinds, since, limit)`, `get_parked_batches(agent)` (reads the park file directly, excerpt cut to 120 chars), `send_agent_control(agent, frame)` for Retry, Discard, Resume now, Keep paused.

### 3. Health tab (desktop, React)

On the Agents screen, a Health tab next to the existing list:

- Table, one row per agent: state (active, paused until, breaker open, offline), turns 24h / 7d, failed 24h / 7d, parked, needs review, last error class and time, reconnects 24h.
- Row click opens a drawer: failed-task list (time, channel link, class, excerpt, Retry / Discard), pause card (until, waiting count, Resume now / Keep paused), last 50 health events.
- A red badge on the Agents entry in the sidebar when any agent has a needs-review batch or an open breaker.
- No polling loop beyond the existing agents query; frames update the store, the tab re-reads on focus like the workflow lists.

### 4. Alerts

Rules, evaluated in the desktop when a frame or sync arrives:

| Condition | Alert |
|---|---|
| a parked batch is older than 15 minutes | "PM has 3 saved messages waiting for 20 minutes" |
| a batch entered needs review | "A Critic request needs your decision" |
| a breaker opened | "Critic's provider is failing; probing every 10 min" |
| a pause longer than 1 hour started | "PM is paused until 4:20 AM" |
| an agent process exited with a non-zero code | existing `last_error`, now also an alert |

Delivery order: macOS notification first (local, works with the relay down), then a Buzz DM from the agent to Devin when the relay is up. One alert per condition per agent per hour. Alerts never claim a mention was missed; the client cannot know what it did not receive.

### 5. CLI

`buzz agents health [--since 24h|7d] [--json]` prints the same table from the ledger files, no relay needed. `buzz agents parked`, `replay`, `discard` come from T16. This is what scripts and Claude use.

## Data and privacy

Frames and the database carry ids, classes, counts and timestamps. Message text stays in the T16 park file and is read on demand for the excerpt. Retention 30 days. Deleting `agent-health.db` is safe; the next sync rebuilds it.

## Tests

- Rust: store insert-or-ignore on duplicate (agent, seq); ledger sync is idempotent; retention prunes by date; alert rule evaluation on fixture event sets (each rule fires once per hour).
- Node: summary reducer from events to the table row; badge condition.
- One smoke spec: with a fixture ledger, the Health tab shows one paused agent and one needs-review batch, and Retry sends the control frame.

## Gates

Fast set plus `cargo test agent_health`, `pnpm exec playwright test --project=smoke agent-health.spec.ts`. The queue runs the full suite once. Sol reviews before ready.

## Out of scope

Relay changes, dashboards outside the app, any change to how agents are spawned.
