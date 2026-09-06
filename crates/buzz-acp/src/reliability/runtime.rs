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
