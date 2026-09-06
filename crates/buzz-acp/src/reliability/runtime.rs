//! Glue that owns the state directory, the ledger, the park file and the
//! per-agent [`ReliabilityState`], and orders the writes so every prefix of
//! them is a consistent state.
//!
//! Ordering rules this module enforces:
//!
//! - A batch is written to the park file **before** the harness drops its
//!   in-memory copy. A failed park returns an error and the caller keeps the
//!   batch.
//! - `batch_replayed` is written to the ledger **before** the replay prompt is
//!   staged for sending. A crash between the two is visible at the next start
//!   and moves the batch to the review list rather than replaying it twice.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::queue::{BatchEvent, FlushBatch};
use crate::scope::SessionScope;

use super::ledger::{self, Ledger, LedgerBody, TruncateReport};
use super::park::{ParkError, ParkFile, ParkReason, ParkedBatch, ReconcileReport};
use super::state::ReliabilityState;
use super::state_dir;

/// One replay's worth of parked events for a single scope.
#[derive(Debug, Clone)]
pub struct ReplayPlan {
    /// The parked batches being replayed, oldest first.
    pub batch_ids: Vec<Uuid>,
    /// Their events, in the same order, ready for the prompt.
    pub events: Vec<BatchEvent>,
    /// The scope the events belong to.
    pub scope: SessionScope,
    /// Channel the scope belongs to.
    pub channel_id: Uuid,
}

/// The harness's reliability state for one agent.
pub struct ReliabilityRuntime {
    dir: PathBuf,
    agent: String,
    ledger: Ledger,
    park: ParkFile,
    state: ReliabilityState,
    /// Batches whose replay prompt has been staged but whose turn has not
    /// finished, keyed by the scope carrying them. Bounded by the number of
    /// scopes with a turn in flight, which the pool already caps.
    in_flight_replays: HashMap<SessionScope, Vec<Uuid>>,
}

impl ReliabilityRuntime {
    /// Open the state directory for `pubkey_hex` and load its ledger and park
    /// file.
    pub fn open(pubkey_hex: &str, now: DateTime<Utc>) -> io::Result<Self> {
        let dir = state_dir::resolve_state_dir(pubkey_hex)?;
        Self::open_in(&dir, pubkey_hex, now)
    }

    /// Open the state in an explicit directory. Used by tests and by any caller
    /// that resolved the directory itself.
    pub fn open_in(dir: &Path, pubkey_hex: &str, now: DateTime<Utc>) -> io::Result<Self> {
        let ledger = Ledger::open(dir, pubkey_hex, now)?;
        let park = ParkFile::open(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            agent: pubkey_hex.to_string(),
            ledger,
            park,
            state: ReliabilityState::default(),
            in_flight_replays: HashMap::new(),
        })
    }

    /// The state directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The agent public key this state belongs to.
    pub fn agent(&self) -> &str {
        &self.agent
    }

    /// The pause and breaker state machine.
    pub fn state(&mut self) -> &mut ReliabilityState {
        &mut self.state
    }

    /// Read-only view of the state machine.
    pub fn state_ref(&self) -> &ReliabilityState {
        &self.state
    }

    /// Read-only view of the park file.
    pub fn park(&self) -> &ParkFile {
        &self.park
    }

    /// Ledger writes that failed, plus park-file writes that failed. A non-zero
    /// total means the durable record is incomplete and the operator has to be
    /// told.
    pub fn write_failures(&self) -> (u64, u64) {
        (self.ledger.write_failures(), self.park.write_failures())
    }

    /// Append a ledger record.
    ///
    /// A failure is logged and counted, never swallowed silently: the count is
    /// readable through [`write_failures`](Self::write_failures) and surfaces in
    /// the next notice. The return value says whether the record landed.
    pub fn record(&mut self, now: DateTime<Utc>, body: LedgerBody) -> bool {
        let kind = body.kind();
        match self.ledger.append(now, body) {
            Ok(()) => true,
            Err(error) => {
                tracing::error!(
                    kind,
                    agent = %self.agent,
                    error = %error,
                    "ledger append failed — the durable record for this event is missing"
                );
                false
            }
        }
    }

    /// Park a batch: the park file is written and fsynced first, then the
    /// `batch_parked` ledger record.
    ///
    /// On failure nothing was written and the caller still owns the batch.
    pub fn park_batch(
        &mut self,
        batch: &FlushBatch,
        reason: ParkReason,
        started: bool,
        now: DateTime<Utc>,
    ) -> Result<(), ParkError> {
        let parked = ParkedBatch::from_batch(batch, reason, started, now)?;
        let events = parked.events.len();
        self.park.park(parked)?;
        self.record(
            now,
            LedgerBody::BatchParked(ledger::BatchParked {
                batch_id: batch.batch_id,
                channel_id: batch.channel_id,
                reason: reason.as_str().to_string(),
                started,
                events,
            }),
        );
        if started {
            self.record(
                now,
                LedgerBody::BatchNeedsReview(ledger::BatchNeedsReview {
                    batch_id: batch.batch_id,
                    channel_id: batch.channel_id,
                    reason: "interrupted after it had started".to_string(),
                }),
            );
        }
        Ok(())
    }

    /// The replay-eligible parked batches for `scope`, oldest first, merged
    /// into one plan. `None` when the scope has nothing to replay.
    ///
    /// This only reads. Commit the plan with
    /// [`commit_replay`](Self::commit_replay) once the caller is ready to stage
    /// the prompt.
    pub fn plan_replay(&self, scope: &SessionScope) -> Option<ReplayPlan> {
        let candidates = self.park.replay_candidates(scope);
        if candidates.is_empty() {
            return None;
        }
        let mut batch_ids = Vec::with_capacity(candidates.len());
        let mut events = Vec::new();
        for batch in candidates {
            batch_ids.push(batch.batch_id);
            events.extend(batch.to_batch_events());
        }
        Some(ReplayPlan {
            batch_ids,
            events,
            scope: scope.clone(),
            channel_id: scope.channel_id(),
        })
    }

    /// Write `batch_replayed` for every batch in the plan and stamp the park
    /// file, **before** the prompt is sent.
    ///
    /// `new_batch_id` identifies the turn that will carry the replayed events.
    /// Returns an error if the park file could not be stamped; the caller then
    /// does not send, so no batch is replayed without a durable record.
    pub fn commit_replay(
        &mut self,
        plan: &ReplayPlan,
        new_batch_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<(), ParkError> {
        for batch_id in &plan.batch_ids {
            self.park.mark_replayed(*batch_id, now)?;
        }
        for batch_id in &plan.batch_ids {
            self.record(
                now,
                LedgerBody::BatchReplayed(ledger::BatchReplayed {
                    batch_id: *batch_id,
                    channel_id: plan.channel_id,
                    replay_of: new_batch_id,
                }),
            );
        }
        Ok(())
    }

    /// Note that `plan`'s batches are riding on an in-flight turn for its scope.
    pub fn mark_replay_in_flight(&mut self, plan: &ReplayPlan) {
        self.in_flight_replays
            .insert(plan.scope.clone(), plan.batch_ids.clone());
    }

    /// A turn for `scope` finished successfully: any batches it was replaying
    /// leave the park file for good. Returns the batch ids released.
    pub fn finish_replay(&mut self, scope: &SessionScope) -> Result<Vec<Uuid>, ParkError> {
        let Some(batch_ids) = self.in_flight_replays.remove(scope) else {
            return Ok(Vec::new());
        };
        let mut released = Vec::new();
        for batch_id in batch_ids {
            if self.park.remove(batch_id)?.is_some() {
                released.push(batch_id);
            }
        }
        Ok(released)
    }

    /// A turn for `scope` failed: its replayed batches stay parked and go back
    /// to being eligible, so the next successful probe replays them again.
    /// At-least-once delivery, never at-most-once.
    pub fn abandon_replay(&mut self, scope: &SessionScope) {
        let Some(batch_ids) = self.in_flight_replays.remove(scope) else {
            return;
        };
        for batch_id in batch_ids {
            if let Err(error) = self.park.unmark_replayed(batch_id) {
                tracing::error!(
                    %batch_id,
                    error = %error,
                    "could not clear the replay stamp — the batch moves to needs_review at the next start"
                );
            }
        }
    }

    /// Operator control frame `discard_batch`.
    pub fn discard(
        &mut self,
        batch_id: Uuid,
        by: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, ParkError> {
        let Some(removed) = self.park.remove(batch_id)? else {
            return Ok(false);
        };
        self.record(
            now,
            LedgerBody::BatchDiscarded(ledger::BatchDiscarded {
                batch_id,
                channel_id: removed.channel_id,
                by: super::error_class::truncate_chars(by, ledger::MAX_LABEL_CHARS),
            }),
        );
        Ok(true)
    }

    /// Operator control frame `replay_batch`: make one parked batch eligible
    /// again whatever its `started` flag.
    pub fn force_replay(&mut self, batch_id: Uuid) -> Result<bool, ParkError> {
        if self.park.get(batch_id).is_none() {
            return Ok(false);
        }
        self.park.clear_review(batch_id)?;
        Ok(true)
    }

    /// Start-up reconciliation: a batch with `batch_replayed` and no
    /// `turn_finished` moves to the review list, never to a second automatic
    /// replay.
    pub fn reconcile_on_start(&mut self, now: DateTime<Utc>) -> Result<ReconcileReport, ParkError> {
        let crashed = self.ledger.replays_without_finish().unwrap_or_else(|error| {
            tracing::error!(error = %error, "could not read the ledger for start-up reconciliation");
            Vec::new()
        });
        let report = self.park.reconcile_on_start(&crashed, now)?;
        for batch_id in &crashed {
            if let Some(batch) = self.park.get(*batch_id) {
                let channel_id = batch.channel_id;
                self.record(
                    now,
                    LedgerBody::BatchNeedsReview(ledger::BatchNeedsReview {
                        batch_id: *batch_id,
                        channel_id,
                        reason: "replay was sent but the turn never finished".to_string(),
                    }),
                );
            }
        }
        Ok(report)
    }

    /// Periodic maintenance: truncate the ledger to its retention window every
    /// six hours.
    pub fn maintain(&mut self, now: DateTime<Utc>) -> TruncateReport {
        match self.ledger.maybe_truncate(now) {
            Ok(report) => report,
            Err(error) => {
                tracing::error!(error = %error, "ledger truncation failed");
                TruncateReport::default()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DedupMode;
    use crate::queue::{CancelReason, EventQueue, QueuedEvent};
    use nostr::{EventBuilder, Keys, Kind};
    use std::time::Instant;

    fn make_test_event(content: &str) -> (nostr::Event, nostr::EventId) {
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::Custom(9), content)
            .sign_with_keys(&keys)
            .unwrap();
        let id = event.id;
        (event, id)
    }

    fn make_flush_batch(
        channel_id: Uuid,
        scope: SessionScope,
        content: &str,
    ) -> (FlushBatch, nostr::EventId) {
        let (event, id) = make_test_event(content);
        (
            FlushBatch {
                batch_id: Uuid::new_v4(),
                channel_id,
                scope,
                events: vec![BatchEvent {
                    event,
                    prompt_tag: "test".into(),
                    received_at: Instant::now(),
                }],
                cancelled_events: vec![],
                cancel_reason: None,
            },
            id,
        )
    }

    // Fixture #5: after a successful probe, a parked batch with started=true is
    // NOT replayed and one with started=false IS, before newer events of the same scope.
    #[test]
    fn test_fixture_5_successful_probe_replays_not_started_before_newer_events() {
        let dir = tempfile::tempdir().unwrap();
        let pubkey = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let now = Utc::now();
        let mut runtime = ReliabilityRuntime::open_in(dir.path(), pubkey, now).unwrap();

        let channel_id = Uuid::new_v4();
        let scope = SessionScope::Conversation { channel_id };

        // 1. Parked batch with started = true
        let (batch_started, _) = make_flush_batch(channel_id, scope.clone(), "started msg");
        let batch_started_id = batch_started.batch_id;
        runtime
            .park_batch(&batch_started, ParkReason::HardTimeout, true, now)
            .unwrap();

        // 2. Parked batch with started = false
        let (batch_not_started, not_started_event_id) =
            make_flush_batch(channel_id, scope.clone(), "not started msg");
        let batch_not_started_id = batch_not_started.batch_id;
        runtime
            .park_batch(&batch_not_started, ParkReason::RetriesExhausted, false, now)
            .unwrap();

        // Verify initial parked state
        assert!(runtime.park().get(batch_started_id).unwrap().needs_review);
        assert!(!runtime
            .park()
            .get(batch_started_id)
            .unwrap()
            .replay_eligible());
        assert!(
            !runtime
                .park()
                .get(batch_not_started_id)
                .unwrap()
                .needs_review
        );
        assert!(runtime
            .park()
            .get(batch_not_started_id)
            .unwrap()
            .replay_eligible());

        // 3. A newer event arrives for the same scope in the queue
        let mut queue = EventQueue::new(DedupMode::Queue);
        let (newer_event, newer_event_id) = make_test_event("newer msg");
        queue.push(QueuedEvent {
            channel_id,
            scope: scope.clone(),
            event: newer_event,
            received_at: Instant::now(),
            prompt_tag: "newer".into(),
        });

        // 4. A probe succeeds! Bind the production function `replay_after_success`.
        crate::replay_after_success(&mut runtime, &mut queue, &scope, now);

        // Assert that started=true was NOT replayed
        let parked_started = runtime.park().get(batch_started_id).unwrap();
        assert!(
            parked_started.replayed_at.is_none(),
            "batch with started=true must NOT be marked replayed"
        );
        assert!(
            parked_started.needs_review,
            "batch with started=true must stay on needs_review list"
        );

        // Assert that started=false WAS replayed
        let parked_not_started = runtime.park().get(batch_not_started_id).unwrap();
        assert!(
            parked_not_started.replayed_at.is_some(),
            "batch with started=false IS replayed (replayed_at stamped)"
        );

        // Assert replay ordering: staged before newer events of the same scope
        let flushed = queue.flush_next().expect("flushed batch");
        assert_eq!(flushed.scope, scope);
        assert_eq!(
            flushed.cancel_reason,
            Some(CancelReason::DeliveredLate),
            "replayed events staged with DeliveredLate framing"
        );
        assert_eq!(flushed.cancelled_events.len(), 1);
        assert_eq!(
            flushed.cancelled_events[0].event.id, not_started_event_id,
            "replayed not-started event is in cancelled_events (preceding newer events)"
        );
        assert_eq!(flushed.events.len(), 1);
        assert_eq!(
            flushed.events[0].event.id, newer_event_id,
            "newer event is in events (after replayed events)"
        );
    }

    // Fixture #6: a `batch_replayed` ledger record with no matching `turn_finished`
    // at start moves the batch to needs_review (reconcile_on_start).
    #[test]
    fn test_fixture_6_batch_replayed_without_turn_finished_moves_to_needs_review_on_start() {
        let dir = tempfile::tempdir().unwrap();
        let pubkey = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let now = Utc::now();
        let mut runtime = ReliabilityRuntime::open_in(dir.path(), pubkey, now).unwrap();

        let channel_id = Uuid::new_v4();
        let scope = SessionScope::Conversation { channel_id };
        let (batch, _) = make_flush_batch(channel_id, scope.clone(), "crashed mid-replay");
        let batch_id = batch.batch_id;

        // Park the batch (not started -> replay-eligible)
        runtime
            .park_batch(&batch, ParkReason::RetriesExhausted, false, now)
            .unwrap();
        assert!(!runtime.park().get(batch_id).unwrap().needs_review);
        assert!(runtime.park().get(batch_id).unwrap().replay_eligible());

        // Stage and commit replay: this writes `batch_replayed` to the ledger and stamps the park file
        let plan = runtime.plan_replay(&scope).expect("replay plan");
        assert_eq!(plan.batch_ids, vec![batch_id]);
        runtime.commit_replay(&plan, Uuid::new_v4(), now).unwrap();

        // Simulate crash mid-replay: process exits WITHOUT writing `turn_finished`.
        drop(runtime);

        // Process restarts at a later time
        let restart_now = now + chrono::Duration::seconds(30);
        let mut restarted = ReliabilityRuntime::open_in(dir.path(), pubkey, restart_now).unwrap();

        // Run start-up reconciliation using the production function
        let report = restarted.reconcile_on_start(restart_now).unwrap();
        assert_eq!(
            report.crashed_mid_replay, 1,
            "reconcile_on_start must report the crashed mid-replay batch"
        );

        // The batch must now be in needs_review, never to be automatically replayed
        let parked = restarted.park().get(batch_id).expect("batch still parked");
        assert!(
            parked.needs_review,
            "crashed mid-replay batch must have needs_review = true"
        );
        assert_eq!(
            parked.needs_review_reason.as_deref(),
            Some("replay was sent but the turn never finished")
        );
        assert!(
            !parked.replay_eligible(),
            "batch in needs_review must not be replay-eligible"
        );

        // A BatchNeedsReview record must have been appended to the ledger
        let records = restarted.ledger.read_all().unwrap();
        assert!(
            records.iter().any(
                |r| matches!(&r.body, LedgerBody::BatchNeedsReview(nr) if nr.batch_id == batch_id)
            ),
            "ledger must contain a batch_needs_review record for the crashed batch"
        );
    }
}
