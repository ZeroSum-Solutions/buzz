//! Test-only barriers for NIP-FI B2 witness tests.
//!
//! Each function is a named production hook that is inert in production
//! (`#[cfg(test)]` guards ensure zero-cost at runtime) but acts as a
//! deterministic barrier in tests. A test arms the gate, dispatches work,
//! waits for the arrived notification, fires expiry, then releases the gate.
//!
//! Pattern (same as `publish_test_hooks` in `side_effects.rs`):
//! - `arm(community)` → `(arrived_rx, release_notify)`
//! - Production code calls `before_X(community).await`
//! - Test awaits `arrived_rx.await` → knows production reached the hook
//! - Test fires expiry
//! - Test calls `release_notify.notify_one()` → production proceeds
//!
//! Only one gate per slot is supported at a time (static Mutex). Tests are
//! sequential per community; concurrent tests use different communities.
//!
//! # Per-witness mutation-red table
//!
//! Every witness listed below follows the same structure:
//!
//! | Witness | Hook location (production file:line) | One-line mutation | Failing assertion |
//! |---------|--------------------------------------|-------------------|-------------------|
//! | **W1** (auth barrier) | `handlers/auth.rs:319` — immediately before `acquire_effect()` in AUTH commit path | Delete `before_auth_commit(...)` call | `arrived_rx` times out → test panics |
//! | **W1** (auth barrier) | same | Remove `acquire_effect()` from auth.rs | `auth_state is NOT Authenticated` → assertion panics |
//! | **W1** (auth barrier) | same | Change gate to `off_mode` | same as above |
//! | **W2** (event barrier) | `handlers/event.rs:784` — immediately before `acquire_effect()` in event ingest path | Delete `before_event_ingest(...)` call | `arrived_rx` times out → test panics |
//! | **W2** (event barrier) | same | Remove `acquire_effect()` from event.rs | "session expired" OK(false) not sent → first `try_recv` panics |
//! | **W2** (event barrier) | same | Change gate to `off_mode` | same as above |
//! | **W3** (REQ barrier) | `handlers/req.rs:280` — immediately before `acquire_effect()` in REQ path | Delete `before_req_registration(...)` call | `arrived_rx` times out → test panics |
//! | **W3** (REQ barrier) | same | Remove `acquire_effect()` from req.rs | subscription IS inserted → `subs.is_empty()` panics |
//! | **W3** (REQ barrier) | same | Change gate to `off_mode` | same as above |
//! | **W4** (COUNT barrier) | `handlers/count.rs:112` — immediately before `acquire_effect()` in COUNT path | Delete `before_count_query(...)` call | `arrived_rx` times out → test panics |
//! | **W4** (COUNT barrier) | same | Remove `acquire_effect()` from count.rs | CLOSED message changes from "session expired" → assertion panics |
//! | **W4** (COUNT barrier) | same | Change gate to `off_mode` | no CLOSED sent → `try_recv` returns `Err` → assertion panics |
//! | **W5** (audio B1 expired-at-pairing) | `audio/handler.rs`, B1 deadline check after NIP-42 auth | Remove the already-expired deadline check | frame text changes to "not a relay member" → byte assertion panics |
//! | **W6** (audio B1 mid-admission) | `audio/handler.rs`, biased `cancel.cancelled()` in auth select | Remove `_ = cancel.cancelled() => return` | handler proceeds to auth exchange; close assertion fires on 3s timeout |
//! | **W7** (audio B3 expiry writer) | `nip_fi_session::spawn_nip_fi_expiry_task`, audio enqueue | Delete the audio denial enqueue | `frames[0]` is not the expected restricted JSON → assertion panics |
//! | **W8** (audio membership barrier) | `audio/handler.rs:1572` — entry of `check_membership_for_admission` | Delete `before_membership_check(...)` call | `arrived_rx` times out → test panics |
//! | **W8** (audio membership barrier) | same | Move hook to after `state.db.get_channel()` | DB error fires before hook on lazy pool → `arrived_rx` times out |
//! | **W9** (audio participant-commit barrier) | `audio/handler.rs:1784` — before `acquire_effect()` in `commit_participant_join` | (DB-integration blocker — see below) | — |
//! | **W10** (audio concurrent-reaffirm) | same as W9 | (DB-integration blocker — see below) | — |
//!
//! **W9/W10 blocker**: `commit_participant_join` requires a real DB transaction
//! (`state.db.begin_event_write_transaction()`) and seeded channel/membership state
//! to reach the `before_participant_commit` hook. The lazy pool at port 1 errors at
//! transaction start. These witnesses belong in the `buzz-relay-integration` test suite
//! once that suite provides a seeded channel fixture. The hook is in place and will fire
//! correctly when the DB fixture is available.
//!
//! **W9 what it proves**: expiry between an uncommitted 48101 insert and `acquire_effect()`
//! rolls back the transaction — no committed row, no membership write, `JoinCommitError::Expired`
//! returned to caller, caller removes peer from room.
//!
//! **W10 concurrent-reaffirm what it proves**: two concurrent callers for the same pubkey;
//! the second observes the first committed; expiry fires during the second's commit; the
//! second rolls back without affecting the first's committed row.
//!
//! # Teardown ordering (quiescence citations)
//!
//! The quiescence requirement from the contract (e5bc0382): the expiry task must complete
//! (i.e., acquire and release the write guard after cancellation) before subscription/peer
//! cleanup runs. This prevents post-`remove_connection` subscription leaks.
//!
//! **Root WS** (`connection.rs:449-453`):
//! ```text
//! if let Some(task) = nip_fi_expiry_task { let _ = task.await; }  // line 449
//! for removed in state.sub_registry.remove_connection(...)  // line 453 — after expiry
//! ```
//!
//! **Audio WS** (`audio/handler.rs:1128-1138`):
//! ```text
//! if let Some(expiry_task) = nip_fi_audio_expiry_task { let _ = expiry_task.await; }  // line 1128
//! room.remove_peer_and_check_ended(peer_id)  // line 1138 — after expiry
//! ```
//!
//! **Pre-existing cleanup helpers** (audio expiry path):
//! - `send_clean_close` (`audio/join.rs`) — sends WS close frame for remote session path
//! - `cleanup_if_empty` (`audio/rooms.rs`) — removes room when peer count drops to zero
//! - `room.remove_peer` (`audio/room.rs`) — removes peer from in-memory room roster

use buzz_core::CommunityId;
use std::sync::{Arc, Mutex};
use tokio::sync::{oneshot, Notify};

struct Gate {
    community: CommunityId,
    arrived: oneshot::Sender<()>,
    release: Arc<Notify>,
}

macro_rules! make_hook {
    ($mod_name:ident, $fn_name:ident) => {
        pub(crate) mod $mod_name {
            use super::*;

            static GATE: Mutex<Option<Gate>> = Mutex::new(None);

            /// Arm a one-shot barrier for `community`.
            ///
            /// Returns `(arrived_rx, release)`. Await `arrived_rx` to know when
            /// the production code has reached this hook; call `release.notify_one()`
            /// to let it continue.
            pub(crate) fn arm(community: CommunityId) -> (oneshot::Receiver<()>, Arc<Notify>) {
                let (tx, rx) = oneshot::channel();
                let release = Arc::new(Notify::new());
                *GATE.lock().unwrap() = Some(Gate {
                    community,
                    arrived: tx,
                    release: release.clone(),
                });
                (rx, release)
            }

            pub(crate) async fn trigger(community: CommunityId) {
                let gate = {
                    let mut slot = GATE.lock().unwrap();
                    match slot.as_ref() {
                        Some(g) if g.community == community => slot.take(),
                        _ => None,
                    }
                };
                if let Some(g) = gate {
                    let _ = g.arrived.send(());
                    g.release.notified().await;
                }
            }
        }

        pub(crate) async fn $fn_name(community: CommunityId) {
            $mod_name::trigger(community).await;
        }
    };
}

make_hook!(auth_commit_hook, before_auth_commit);
make_hook!(event_ingest_hook, before_event_ingest);
make_hook!(req_registration_hook, before_req_registration);
make_hook!(count_query_hook, before_count_query);

// ── Audio B1 hooks ─────────────────────────────────────────────────────────
// `before_membership_check`: fires between NIP-42 pairing and the membership
// DB read inside `check_membership_for_admission`. Arms expiry here → proves
// that a cancellation before membership check produces zero DB side effects.
//
// `before_participant_commit`: fires between the 48101 insert and the
// `acquire_effect()` + `tx.commit()` inside `commit_participant_join`. Arms
// expiry here → proves that a cancellation before the permit acquisition
// rolls back the transaction and produces zero post-expiry 48101/membership
// writes.
make_hook!(audio_membership_check_hook, before_membership_check);
make_hook!(audio_participant_commit_hook, before_participant_commit);
