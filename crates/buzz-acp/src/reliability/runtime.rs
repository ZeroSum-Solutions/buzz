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
    observer: Option<crate::observer::ObserverHandle>,
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
            observer: None,
            in_flight_replays: HashMap::new(),
        })
    }

    /// Attach an observer handle to mirror health-relevant ledger records as live observer frames.
    pub fn with_observer(mut self, observer: impl IntoObserverHandle) -> Self {
        self.observer = observer.into_observer();
        self
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
        let body_for_observer = if self.observer.is_some() {
            Some(body.clone())
        } else {
            None
        };
        match self.ledger.append(now, body) {
            Ok(()) => {
                if let (Some(observer), Some(body)) = (&self.observer, body_for_observer) {
                    Self::emit_health_frame(observer, &self.agent, now, &body);
                }
                true
            }
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

    fn emit_health_frame(
        observer: &crate::observer::ObserverHandle,
        agent: &str,
        now: DateTime<Utc>,
        body: &LedgerBody,
    ) {
        let (emit_kind, is_turn_failed) = match body {
            LedgerBody::BatchParked(_) => ("batch_parked", false),
            LedgerBody::BatchReplayed(_) => ("batch_replayed", false),
            LedgerBody::BatchNeedsReview(_) => ("batch_needs_review", false),
            LedgerBody::AgentPaused(_) => ("agent_paused", false),
            LedgerBody::AgentResumed(_) => ("agent_resumed", false),
            LedgerBody::BreakerOpened(_) => ("breaker_opened", false),
            LedgerBody::BreakerClosed(_) => ("breaker_closed", false),
            LedgerBody::RelayReconnected(_) => ("relay_reconnected", false),
            LedgerBody::TurnFinished(finished) => {
                if matches!(finished.outcome, ledger::TurnOutcome::Error { .. }) {
                    ("turn_failed", true)
                } else {
                    return;
                }
            }
            _ => return,
        };

        let record = ledger::LedgerRecord {
            at: now,
            agent: agent.to_string(),
            body: body.clone(),
        };

        let mut payload = match serde_json::to_value(&record) {
            Ok(v) => v,
            Err(error) => {
                tracing::error!(
                    error = %error,
                    "failed to serialize ledger record for health observer frame"
                );
                return;
            }
        };

        if is_turn_failed {
            payload["kind"] = serde_json::Value::String("turn_failed".to_string());
            if let Some(outcome) = payload.get_mut("outcome").and_then(|v| v.as_object_mut()) {
                outcome.remove("raw");
                if let Some(class) = outcome.get("class").cloned() {
                    payload["class"] = class;
                }
            }
            if let Some(obj) = payload.as_object_mut() {
                obj.remove("raw");
            }
        }

        if let Some(batch_id) = payload.get("batch_id").cloned() {
            payload["batchId"] = batch_id;
        }
        if let Some(channel_id) = payload.get("channel_id").cloned() {
            payload["channelId"] = channel_id;
        }
        if let Some(replay_of) = payload.get("replay_of").cloned() {
            payload["replayOf"] = replay_of;
        }
        if let Some(after_secs) = payload.get("after_secs").cloned() {
            payload["afterSecs"] = after_secs;
        }

        let context = crate::observer::ObserverContext {
            channel_id: body.channel_id().map(|id| id.to_string()),
            session_id: None,
            turn_id: None,
            started_at: None,
        };

        observer.emit(emit_kind, None, &context, payload);
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

/// Helper trait allowing [`ReliabilityRuntime::with_observer`] to accept either an
/// [`ObserverHandle`](crate::observer::ObserverHandle) or an `Option<ObserverHandle>`.
pub trait IntoObserverHandle {
    fn into_observer(self) -> Option<crate::observer::ObserverHandle>;
}

impl IntoObserverHandle for crate::observer::ObserverHandle {
    fn into_observer(self) -> Option<crate::observer::ObserverHandle> {
        Some(self)
    }
}

impl IntoObserverHandle for Option<crate::observer::ObserverHandle> {
    fn into_observer(self) -> Option<crate::observer::ObserverHandle> {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observer::ObserverHandle;
    use chrono::Utc;
    use uuid::Uuid;

    #[test]
    fn record_mirrors_health_kinds_to_observer() {
        let temp = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let agent = "test_agent_pk";
        let observer = ObserverHandle::in_process();
        let mut rx = observer.subscribe();

        let mut runtime = ReliabilityRuntime::open_in(temp.path(), agent, now)
            .unwrap()
            .with_observer(observer);

        let batch_id = Uuid::new_v4();
        let channel_id = Uuid::new_v4();
        let body = LedgerBody::BatchParked(ledger::BatchParked {
            batch_id,
            channel_id,
            reason: "retries_exhausted".to_string(),
            started: false,
            events: 3,
        });

        assert!(runtime.record(now, body));

        let event = rx.try_recv().expect("should receive observer frame");
        assert_eq!(event.kind, "batch_parked");
        assert_eq!(event.channel_id, Some(channel_id.to_string()));
        assert_eq!(event.payload["batchId"], batch_id.to_string());
        assert_eq!(event.payload["events"], 3);
        assert_eq!(event.payload["at"], serde_json::to_value(now).unwrap());
        assert_eq!(event.payload["reason"], "retries_exhausted");
        assert_eq!(event.payload["started"], false);

        // batch_replayed
        let replay_id = Uuid::new_v4();
        let replayed = LedgerBody::BatchReplayed(ledger::BatchReplayed {
            batch_id,
            channel_id,
            replay_of: replay_id,
        });
        assert!(runtime.record(now, replayed));
        let event = rx.try_recv().expect("should receive batch_replayed frame");
        assert_eq!(event.kind, "batch_replayed");
        assert_eq!(event.payload["replayOf"], replay_id.to_string());

        // agent_paused
        let paused = LedgerBody::AgentPaused(ledger::AgentPaused {
            class: "capacity_exhausted".to_string(),
            until: now,
            waiting: 2,
        });
        assert!(runtime.record(now, paused));
        let event = rx.try_recv().expect("should receive agent_paused frame");
        assert_eq!(event.kind, "agent_paused");
        assert_eq!(event.payload["class"], "capacity_exhausted");
        assert_eq!(event.channel_id, None);

        // breaker_opened
        let breaker = LedgerBody::BreakerOpened(ledger::BreakerOpened {
            scope: "scope1".to_string(),
            consecutive: 3,
        });
        assert!(runtime.record(now, breaker));
        let event = rx.try_recv().expect("should receive breaker_opened frame");
        assert_eq!(event.kind, "breaker_opened");
        assert_eq!(event.payload["consecutive"], 3);
    }

    #[test]
    fn turn_failed_frame_carries_class_but_never_raw() {
        let temp = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let agent = "test_agent_pk";
        let observer = ObserverHandle::in_process();
        let mut rx = observer.subscribe();

        let mut runtime = ReliabilityRuntime::open_in(temp.path(), agent, now)
            .unwrap()
            .with_observer(observer);

        let batch_id = Uuid::new_v4();
        let channel_id = Uuid::new_v4();
        let raw_secret = "secret provider raw error stack trace";
        let body = LedgerBody::TurnFinished(ledger::TurnFinished {
            batch_id,
            channel_id,
            outcome: ledger::TurnOutcome::error("capacity_exhausted", raw_secret),
        });

        assert!(runtime.record(now, body));

        let event = rx.try_recv().expect("should receive observer frame");
        assert_eq!(event.kind, "turn_failed");
        assert_eq!(event.payload["class"], "capacity_exhausted");
        assert!(event.payload.get("raw").is_none());
        if let Some(outcome) = event.payload.get("outcome") {
            assert!(outcome.get("raw").is_none());
        }
        let serialized = event.payload.to_string();
        assert!(!serialized.contains(raw_secret));
        assert!(!serialized.contains("\"raw\""));
    }

    #[test]
    fn turn_ok_and_turn_started_emit_no_frame() {
        let temp = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let agent = "test_agent_pk";
        let observer = ObserverHandle::in_process();
        let mut rx = observer.subscribe();

        let mut runtime = ReliabilityRuntime::open_in(temp.path(), agent, now)
            .unwrap()
            .with_observer(observer);

        let batch_id = Uuid::new_v4();
        let channel_id = Uuid::new_v4();

        let started = LedgerBody::TurnStarted(ledger::TurnStarted::new(
            batch_id,
            channel_id,
            "test_scope",
            vec!["e1".to_string()],
            1,
        ));
        assert!(runtime.record(now, started));
        assert!(
            rx.try_recv().is_err(),
            "turn_started must not emit health frame"
        );

        let ok = LedgerBody::TurnFinished(ledger::TurnFinished {
            batch_id,
            channel_id,
            outcome: ledger::TurnOutcome::Ok,
        });
        assert!(runtime.record(now, ok));
        assert!(
            rx.try_recv().is_err(),
            "turn_finished Ok must not emit health frame"
        );

        let discarded = LedgerBody::BatchDiscarded(ledger::BatchDiscarded {
            batch_id,
            channel_id,
            by: "operator".to_string(),
        });
        assert!(runtime.record(now, discarded));
        assert!(
            rx.try_recv().is_err(),
            "batch_discarded must not emit health frame"
        );
    }
}
