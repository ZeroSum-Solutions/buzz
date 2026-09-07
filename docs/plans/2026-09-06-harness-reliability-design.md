# Harness reliability: park, pause, breaker, replay

Ticket: T16 · `feat/harness-reliability` · crate `buzz-acp` only.
Status: design, approved for spec by Devin 2026-09-06 02:40. Audited by GPT-5.6 Sol (questions and recommendations) before the decisions below were put to Devin.

## Decisions already made (Devin, 2026-09-06)

1. A Claude session limit pauses the agent until the reset time. No automatic seat rotation. A manual `cswap switch` stays available and the pause notice names it.
2. Only a batch that never started running replays on its own. A batch that had started goes to a review list with Retry and Discard.
3. No message is ever discarded by the harness. The 19 messages dropped on 2026-09-02 and 09-03 are gone and cannot be recovered; this design stops the next ones from being lost.

## What goes wrong today

`queue.rs` retries a failed batch ten times with backoff (5 s doubling to 300 s, about 25 minutes in total), then dead-letters it: logs at ERROR, posts one warning to the channel, and drops the events. A session limit lasts hours, so every message in that window died after 25 minutes. The warning post itself fails when the relay is down. The retry budget is spent the same way on a provider that is simply broken for two hours (Critic, 2026-09-03). Nothing about a failed batch is written to disk, so no later process can see it.

## Terms

- **Batch**: `FlushBatch` (channel, scope, events). This design adds a stable `batch_id: Uuid` assigned when the batch is built. Event ids are the Nostr event ids and are already stable.
- **Started**: the harness saw agent output or a tool call for this batch's turn (the pool already tracks activity for `recently_active`). A batch that failed before any output is **not started**.
- **State dir**: `BUZZ_ACP_STATE_DIR`, set by the desktop to `<app support>/agents/state/<pubkey>/` at spawn. Fallback when unset: `~/.buzz/.state/<pubkey-prefix-16>/`. Permissions 0700 on the dir, 0600 on files.
- **Ledger**: `state/ledger.jsonl`, append-only, one JSON object per line. The harness owns it. Observer frames (T17) mirror it but are never the source of truth.
- **Park file**: `state/parked.jsonl`, one line per parked batch with the serialized events and prompt tags. The ledger refers to parked batches by `batch_id`.

## Error classes (`error_class.rs`, new)

`classify(err: &AcpError) -> ErrorClass`, pure, unit-tested on the real log lines:

| Class | Matches | Example from the logs |
|---|---|---|
| `CapacityExhausted { resets_at: Option<DateTime<Utc>> }` | "session limit", "rate limit", HTTP 429, "overloaded", "quota" | `Internal error: You've hit your session limit · resets 4:20am (America/Los_Angeles)` |
| `Auth` | the existing auth detection | unchanged |
| `ProviderInternal` | "Internal error" with no capacity marker, HTTP 5xx | `Agent reported error (code -32603): Internal error` (Critic, 2026-09-03) |
| `Unknown` | everything else | |

`resets_at` is parsed from `resets H:MM(am|pm) (IANA zone)` as the next occurrence of that wall time in that zone. When it cannot be parsed the pause is 30 minutes. A pause is never longer than 6 hours; a longer parsed value is clamped and logged.

## State machine

Per agent, not per scope, because a capacity limit belongs to the account:

```
Active --CapacityExhausted--> Paused{until}
Paused --timer--> Probing        (first queued batch is the probe; nothing else moves)
Probing --Ok--> Active            (then replay, see below)
Probing --CapacityExhausted--> Paused{new until}   (notice only if until moved by > 15 min)
Active --ProviderInternal or Unknown, 3 consecutive on one scope--> BreakerOpen{scope, next_probe}
BreakerOpen --every 10 min--> probe one batch; Ok closes the breaker; failure reschedules; open at most 6 h then Park
```

Rules:

- While Paused or BreakerOpen, retry counts are frozen. The existing backoff path is only for transient failures between probes.
- The existing `MAX_RETRIES` path no longer discards. Exhaustion parks the batch (`batch_parked`, reason `retries_exhausted`).
- A hard-cap timeout parks the batch. If the turn had started it is marked `needs_review`; if not, it is eligible for replay.
- An `Auth` error parks immediately with `needs_review` (a re-login fixes it; retrying does not).
- Cancelled and steered turns keep their existing merge behaviour. This design does not touch them.

## Ledger records

Every line has `at` (RFC 3339 UTC), `agent` (pubkey), `kind`, and `batch_id` where it applies.

| kind | fields |
|---|---|
| `turn_started` | `channel_id`, `scope`, `event_ids`, `attempt` |
| `turn_activity` | first output or tool call seen; written once per batch |
| `turn_finished` | `outcome`: `ok`, `error{class, raw}`, `timeout{kind, started}`, `cancelled`, `exited` |
| `batch_parked` | `reason` (`retries_exhausted`, `hard_timeout`, `auth`, `breaker_expired`), `started: bool`, `events` count |
| `batch_replayed` | `replay_of: batch_id` of the new turn; written **before** the prompt is sent |
| `batch_needs_review` | `reason` |
| `batch_discarded` | `by: operator`, via control frame |
| `agent_paused` | `class`, `until`, `waiting` count |
| `agent_resumed` | |
| `breaker_opened` / `breaker_closed` | `scope`, consecutive failures |
| `relay_reconnected` | `after_secs` |

Retention: the harness truncates the ledger to 30 days on start and every 6 hours. Parked batches with `needs_review` stay until acted on; replay-eligible parked batches older than 7 days move to `needs_review`. Hard cap 10 MB per file; beyond it the oldest replay-eligible batches move to `needs_review` and the operator is told in the next notice.

## Replay

- Replay starts only after a **successful live turn** (the probe). A process restart alone never replays anything, because a restart proves nothing about the provider.
- Order: for each scope, parked not-started batches replay oldest first, ahead of newer queued events for that scope, so the conversation stays in order. Several parked batches for one scope become one prompt.
- Framing: the events keep their original text. `format_prompt` adds a section header "Delivered late: these messages arrived while I was unavailable (first at HH:MM, last at HH:MM)". This uses the same annotated-section mechanism as the cancelled-events merge. The user's words are never edited.
- Delivery guarantee: at least once. `batch_replayed` is written before the send. On start, a batch with `batch_replayed` and no `turn_finished` is a crash mid-replay and moves to `needs_review`, never to a second automatic replay.
- Started batches never replay automatically. They wait for `replay_batch` from the operator.

## Operator control

The desktop already sends control frames to the harness over the relay (`switch_model`). This design adds:

| frame | effect |
|---|---|
| `replay_batch { batch_id }` | replay one parked batch now, whatever its `started` flag |
| `discard_batch { batch_id }` | remove the batch from the park file, write `batch_discarded` |
| `resume_now` | leave Paused or BreakerOpen and probe immediately |
| `keep_paused { until }` | extend a pause |

CLI, this ticket: `buzz agents parked [--json]` lists parked batches from the state dir (no relay needed), `buzz agents replay <batch_id>` and `buzz agents discard <batch_id>` send the frames. The desktop buttons come with T17.

## Notices in the channel

At most one notice per pause per channel, one per park, one per breaker open. The relay post is retried with the same backoff as any other post; if it still fails the ledger has the record and T17 raises the alert locally. Templates:

- Pause: "⏸️ PM is paused until 4:20 AM (Claude session limit). 6 messages are saved and will be answered in order when I am back. To switch seats now run `cswap switch` and restart the Claude agents."
- Park after retries: "⚠️ I could not process the last request after several attempts (reason). It is saved and will be retried as soon as I am back. Nothing is lost."
- Needs review: "⚠️ A request was interrupted after it had started, so it will not run again on its own. Devin can retry or discard it from the Agents screen."
- Breaker: "⚠️ Critic's provider is returning errors. I will try again every 10 minutes and answer in order when it recovers."

## Privacy

Parked batches hold client messages. They live only in the agent state dir with 0600 permissions, are removed on discard, and are never sent anywhere except back to the same agent. UI and CLI excerpts are cut to 120 characters. The raw provider error is stored beside its class so a misclassification can be diagnosed.

## Out of scope

Seat rotation, desktop UI (T17), the health database (T17), any change to the relay, any recovery of the 19 already-dropped messages.

## Tests first

Fixture tests are on branch `feat/harness-reliability-fixtures`, marked `#[ignore = "T16 fixture: fails until park/pause/breaker land"]`, and run with `cargo test -p buzz-acp reliability -- --ignored`. They fail today and must pass before this ticket's PR is ready:

1. `classify` on the four real log lines gives `CapacityExhausted{resets_at: Some(next 04:20 America/Los_Angeles)}`, `CapacityExhausted{resets_at: Some(next 00:40 …)}`, `ProviderInternal`, and `Unknown` for an unrelated message.
2. After `MAX_RETRIES + 1` requeues the batch is in the park file, the queue holds zero events, and no event was dropped.
3. A `CapacityExhausted` outcome moves the agent to Paused, does not increment the scope's retry count, and posts exactly one notice per channel.
4. Three consecutive `ProviderInternal` outcomes open the breaker for that scope; the fourth attempt is not made before the probe interval.
5. A parked batch with `started = true` is not replayed after a successful probe; one with `started = false` is, before newer events of the same scope.
6. A `batch_replayed` record with no `turn_finished` at start moves the batch to `needs_review`.

## Gates

`just fmt-check clippy`, `cargo test -p buzz-acp reliability` and `cargo test -p buzz-acp queue`, then the PR. The merge queue runs the full suite once. Sol reviews the diff before the PR is marked ready.
