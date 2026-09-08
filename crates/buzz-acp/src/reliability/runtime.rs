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
use super::transaction::{self, Operation, OperationKind};

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

/// The result of [`ReliabilityRuntime::discard`], distinguishing "there was
/// nothing to discard" from "the batch was destroyed but its ledger record
/// failed" — the two collapsed into the same `false` under the old `bool`
/// return, which made the caller report a successful destructive discard as
/// `unknown_batch` (T16 delta 1, finding 12 / prior #14a).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscardOutcome {
    /// No parked batch had this id; nothing was touched.
    NotFound,
    /// The batch was removed and the ledger record landed.
    Discarded,
    /// The batch was durably removed, but the ledger append failed — the
    /// discard happened and is irreversible, it just has no audit record.
    DiscardedUnrecorded,
}

/// The result of [`ReliabilityRuntime::finish_replay`].
#[derive(Debug, Default)]
pub struct FinishReplayReport {
    /// Batch ids actually removed from the park file, even when a later id
    /// in the same call failed to remove.
    pub released: Vec<Uuid>,
    /// The first removal failure encountered, if any. `released` still holds
    /// whatever succeeded before it.
    pub error: Option<ParkError>,
}

/// The harness's reliability state for one agent.
pub struct ReliabilityRuntime {
    _lock: std::fs::File,
    replay_floor: u64,
    channel_access: Option<tokio::sync::watch::Receiver<crate::relay::ChannelAccessState>>,
    started_event_ids: std::collections::HashSet<String>,
    transaction_failed: bool,
    transaction_failures: u64,
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
        state_dir::ensure_dir(dir)?;
        let lock = state_dir::open_append(&dir.join("runtime.lock"))?;
        fs2::FileExt::try_lock_exclusive(&lock)?;
        transaction::recover(dir, pubkey_hex)?;
        let replay_floor = transaction::replay_floor(dir, now.timestamp().max(0) as u64)?;
        let started_event_ids = transaction::read_receipts(dir)?;
        let ledger = Ledger::open(dir, pubkey_hex, now)?;
        let park = ParkFile::open(dir)?;
        Ok(Self {
            _lock: lock,
            replay_floor,
            channel_access: None,
            started_event_ids,
            transaction_failed: false,
            transaction_failures: 0,
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

    /// Whether all custody transitions are durably committed.
    pub fn is_ready(&self) -> bool {
        !self.transaction_failed
            && !self.dir.join(transaction::PENDING_FILE).exists()
            && self
                .channel_access
                .as_ref()
                .is_none_or(|rx| !rx.borrow().overflowed)
    }

    /// Whether every new parked batch has a durable notice outbox entry.
    pub fn notices_ready(&self) -> bool {
        self.park
            .batches()
            .iter()
            .all(|batch| !batch.notice_pending)
    }

    /// Finish a pending custody transition before accepting new work. Replays
    /// recovered outside their original dispatch are marked for operator review.
    pub fn recover_pending(&mut self, now: DateTime<Utc>) -> io::Result<()> {
        let recovered = self.recover_operation(now)?;
        if matches!(recovered, Some(OperationKind::Replay { .. })) {
            self.reconcile_on_start(now).map_err(io::Error::other)?;
        }
        Ok(())
    }

    fn recover_operation(&mut self, now: DateTime<Utc>) -> io::Result<Option<OperationKind>> {
        let recovered = transaction::recover(&self.dir, &self.agent)?;
        if recovered.is_some() || self.transaction_failed {
            self.started_event_ids = transaction::read_receipts(&self.dir)?;
            self.park = ParkFile::open(&self.dir)?;
            self.ledger = Ledger::open(&self.dir, &self.agent, now)?;
        }
        self.transaction_failed = false;
        Ok(recovered)
    }

    fn transact(
        &mut self,
        kind: OperationKind,
        after: Vec<ParkedBatch>,
        bodies: Vec<LedgerBody>,
        now: DateTime<Utc>,
    ) -> Result<(), ParkError> {
        self.transact_with_receipts(kind, after, bodies, None, now)
    }

    fn transact_with_receipts(
        &mut self,
        kind: OperationKind,
        after: Vec<ParkedBatch>,
        bodies: Vec<LedgerBody>,
        receipts: Option<Vec<String>>,
        now: DateTime<Utc>,
    ) -> Result<(), ParkError> {
        // Pure custody updates do not rewrite the audit file. The optional
        // ledger image is still atomic with park changes when records exist.
        let records = if bodies.is_empty() {
            None
        } else {
            let mut records = self.ledger.read_all().inspect_err(|_error| {
                self.transaction_failed = true;
                self.transaction_failures = self.transaction_failures.saturating_add(1);
            })?;
            records.extend(bodies.iter().cloned().map(|body| ledger::LedgerRecord {
                at: now,
                agent: self.agent.clone(),
                body,
            }));
            let (records, dropped) = ledger::fit_to_cap(records)?;
            if dropped > 0 {
                tracing::warn!(dropped, "ledger retention cap removed oldest audit records; durable event receipts remain intact");
            }
            Some(records)
        };
        let op = Operation {
            id: Uuid::new_v4(),
            agent: self.agent.clone(),
            kind,
            before: self.park.batches().to_vec(),
            after,
            ledger: records,
            receipts,
        };
        self.transaction_failed = true;
        let result = transaction::prepare(&self.dir, &op)
            .and_then(|()| transaction::commit_prepared(&self.dir, &op))
            .and_then(|()| {
                self.park.accept_committed(op.after.clone());
                if op.ledger.is_some() {
                    self.ledger = Ledger::open(&self.dir, &self.agent, now)?;
                }
                Ok(())
            });
        if let Err(error) = result {
            self.transaction_failures = self.transaction_failures.saturating_add(1);
            return Err(ParkError::Io(error));
        }
        self.transaction_failed = false;
        if let Some(ids) = op.receipts {
            self.started_event_ids = ids.into_iter().collect();
        }
        if let Some(observer) = &self.observer {
            for body in bodies {
                Self::emit_health_frame(observer, &self.agent, now, &body);
            }
        }
        Ok(())
    }

    pub(crate) fn with_channel_access(
        mut self,
        access: tokio::sync::watch::Receiver<crate::relay::ChannelAccessState>,
    ) -> Self {
        self.channel_access = Some(access);
        self
    }

    fn channel_allowed(&self, channel_id: Uuid) -> bool {
        self.channel_access.as_ref().is_none_or(|rx| {
            let access = rx.borrow();
            !access.overflowed
                && access.permitted.contains(&channel_id)
                && !access.denied.contains(&channel_id)
        })
    }

    /// Retain revoked channel inputs visibly for review, including pending
    /// notice intent which cannot be posted into a channel without access.
    pub fn retain_revoked_channel(
        &mut self,
        channel_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<(), ParkError> {
        self.recover_pending(now)?;
        let mut next = self.park.batches().to_vec();
        let mut bodies = vec![];
        for batch in &mut next {
            if batch.channel_id == channel_id
                && (batch.needs_review_reason.as_deref() != Some("channel access revoked")
                    || batch.forced
                    || batch.notice_pending)
            {
                batch.needs_review = true;
                batch.forced = false;
                batch.notice_pending = false;
                batch.needs_review_reason = Some("channel access revoked".into());
                bodies.push(LedgerBody::BatchNeedsReview(ledger::BatchNeedsReview {
                    batch_id: batch.batch_id,
                    channel_id,
                    reason: "channel access revoked".into(),
                }));
            }
        }
        if bodies.is_empty() {
            return Ok(());
        }
        self.transact(OperationKind::Update, next, bodies, now)
    }

    /// Fixed epoch boundary: never advanced from untrusted event timestamps.
    pub fn replay_floor(&self) -> u64 {
        self.replay_floor
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

    /// Release any unconsumed probe permits for pause and scope breaker.
    pub fn release_probe(&mut self, scope: Option<&SessionScope>) {
        self.state.release_probe(scope);
    }

    /// Reissue probe permit alias for `release_probe`.
    pub fn reissue_probe(&mut self, scope: Option<&SessionScope>) {
        self.state.reissue_probe(scope);
    }

    /// Read-only view of the park file.
    pub fn park(&self) -> &ParkFile {
        &self.park
    }

    /// Ledger writes that failed, plus park-file writes that failed. A non-zero
    /// total means the durable record is incomplete and the operator has to be
    /// told.
    pub fn write_failures(&self) -> (u64, u64) {
        (
            self.ledger
                .write_failures()
                .saturating_add(self.transaction_failures),
            self.park.write_failures(),
        )
    }

    /// Append a ledger record.
    ///
    /// A failure is logged and counted, never swallowed silently: the count is
    /// readable through [`write_failures`](Self::write_failures) and surfaces in
    /// the next notice. The return value says whether the record landed.
    pub fn record(&mut self, now: DateTime<Utc>, body: LedgerBody) -> bool {
        if !self.is_ready() {
            tracing::error!("ledger mutation refused while a custody operation is pending");
            return false;
        }
        let kind = body.kind();
        match self.transact(
            OperationKind::Update,
            self.park.batches().to_vec(),
            vec![body],
            now,
        ) {
            Ok(()) => true,
            Err(error) => {
                tracing::error!(kind, error = %error, "ledger transaction failed; dispatch is blocked until recovery");
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

    /// Persist listener input before admitting it to the live queue.
    pub fn admit_event(
        &mut self,
        event: &crate::queue::QueuedEvent,
        now: DateTime<Utc>,
    ) -> Result<bool, ParkError> {
        if event.event.created_at.as_secs()
            < self
                .replay_floor
                .saturating_sub(crate::relay::SINCE_SKEW_SECS)
        {
            // Apply the same epoch boundary as the relay REQ even if a relay
            // sends an out-of-filter event. Such input was never admitted.
            return Ok(false);
        }
        self.recover_pending(now)?;
        if self.started_event_ids.contains(&event.event.id.to_hex())
            || self.park.batches().iter().any(|batch| {
                batch
                    .events
                    .iter()
                    .any(|stored| stored.event.id == event.event.id)
            })
        {
            return Ok(false);
        }
        if self.started_event_ids.len() >= transaction::MAX_RECEIPTS {
            return Err(ParkError::Io(io::Error::other("ingress epoch receipt capacity reached; admission paused; reconcile and archive the current epoch before an explicit operator rollover")));
        }
        let batch = FlushBatch {
            batch_id: Uuid::new_v4(),
            channel_id: event.channel_id,
            scope: event.scope.clone(),
            events: vec![BatchEvent {
                event: event.event.clone(),
                prompt_tag: event.prompt_tag.clone(),
                received_at: event.received_at,
            }],
            cancelled_events: vec![],
            cancel_reason: None,
            started: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        if !self.channel_allowed(event.channel_id) {
            let mut parked = ParkedBatch::from_batch(&batch, ParkReason::Ingress, false, now)?;
            parked.needs_review = true;
            parked.notice_pending = false;
            parked.needs_review_reason = Some("channel access revoked".into());
            let mut next = self.park.batches().to_vec();
            next.push(parked);
            self.transact(
                OperationKind::Park(batch.batch_id),
                next,
                vec![LedgerBody::BatchNeedsReview(ledger::BatchNeedsReview {
                    batch_id: batch.batch_id,
                    channel_id: batch.channel_id,
                    reason: "channel access revoked".into(),
                })],
                now,
            )?;
            return Ok(false);
        }
        self.park_batch(&batch, ParkReason::Ingress, false, now)?;
        Ok(true)
    }

    /// Restore admitted, never-dispatched input without evicting live work.
    pub fn refill_ingress(&self, queue: &mut crate::queue::EventQueue) {
        let channels = self
            .park
            .batches()
            .iter()
            .map(|batch| batch.channel_id)
            .collect();
        self.refill_ingress_for(queue, &channels);
    }

    pub(crate) fn refill_ingress_for(
        &self,
        queue: &mut crate::queue::EventQueue,
        allowed_channels: &std::collections::HashSet<Uuid>,
    ) {
        for batch in self.park.batches().iter().filter(|batch| {
            batch.reason == ParkReason::Ingress
                && !batch.started
                && !batch.needs_review
                && self.channel_allowed(batch.channel_id)
                && allowed_channels.contains(&batch.channel_id)
        }) {
            for event in batch.to_batch_events() {
                if queue.can_admit(&batch.scope()) && !queue.contains_event(&event.event.id) {
                    queue.push(crate::queue::QueuedEvent {
                        channel_id: batch.channel_id,
                        scope: batch.scope(),
                        event: event.event,
                        prompt_tag: event.prompt_tag,
                        received_at: event.received_at,
                    });
                }
            }
        }
    }

    /// Persist uncertain-start custody and the event IDs before spawning work.
    pub fn prepare_dispatch(
        &mut self,
        batch: &FlushBatch,
        attempt: u32,
        now: DateTime<Utc>,
    ) -> Result<(), ParkError> {
        if !self.channel_allowed(batch.channel_id) {
            return Err(ParkError::Io(io::Error::other(
                "channel access revoked; dispatch custody retained for review",
            )));
        }
        self.recover_pending(now)?;
        let mut parked = ParkedBatch::from_batch(batch, ParkReason::Ingress, true, now)?;
        parked.notice_pending = false;
        let ids: Vec<_> = parked.events.iter().map(|event| event.event.id).collect();
        let mut next: Vec<_> = self
            .park
            .batches()
            .iter()
            .filter(|existing| {
                existing.batch_id != batch.batch_id
                    && !(existing.reason == ParkReason::Ingress
                        && existing
                            .events
                            .iter()
                            .any(|event| ids.contains(&event.event.id)))
            })
            .cloned()
            .collect();
        next.push(parked);
        let body = LedgerBody::TurnStarted(ledger::TurnStarted::new(
            batch.batch_id,
            batch.channel_id,
            &batch.scope.telemetry_label(),
            ids.iter().map(|id| id.to_hex()),
            attempt,
        ));
        let mut receipts = self.started_event_ids.clone();
        receipts.extend(ids.iter().map(|id| id.to_hex()));
        if receipts.len() > transaction::MAX_RECEIPTS {
            return Err(ParkError::Io(io::Error::other("ingress epoch receipt capacity reached; dispatch paused; reconcile and archive the current epoch before an explicit operator rollover")));
        }
        let mut receipts: Vec<_> = receipts.into_iter().collect();
        receipts.sort_unstable();
        self.transact_with_receipts(OperationKind::Update, next, vec![body], Some(receipts), now)?;
        let replay_ids = self
            .ledger
            .read_all()?
            .iter()
            .filter_map(|record| match &record.body {
                LedgerBody::BatchReplayed(replay) if replay.replay_of == batch.batch_id => {
                    Some(replay.batch_id)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        if !replay_ids.is_empty() {
            self.in_flight_replays
                .insert(batch.scope.clone(), replay_ids);
        }
        Ok(())
    }

    /// Fence a native steer before its transport can observe the input.
    pub fn prepare_steer(
        &mut self,
        event_id: nostr::EventId,
        now: DateTime<Utc>,
    ) -> Result<(), ParkError> {
        self.recover_pending(now)?;
        let parked = self
            .park
            .batches()
            .iter()
            .find(|batch| {
                batch.reason == ParkReason::Ingress
                    && batch.events.iter().any(|event| event.event.id == event_id)
            })
            .ok_or_else(|| {
                ParkError::Io(io::Error::other("native steer has no durable admission"))
            })?;
        let batch = FlushBatch {
            batch_id: parked.batch_id,
            channel_id: parked.channel_id,
            scope: parked.scope(),
            events: parked.to_batch_events(),
            cancelled_events: vec![],
            cancel_reason: None,
            started: Default::default(),
        };
        self.prepare_dispatch(&batch, 1, now)
    }

    /// A transport-proven rejection permits normal queued delivery again.
    pub fn reject_steer(&mut self, event_id: &str, now: DateTime<Utc>) -> Result<(), ParkError> {
        self.recover_pending(now)?;
        let mut next = self.park.batches().to_vec();
        for batch in &mut next {
            if batch.reason == ParkReason::Ingress
                && batch
                    .events
                    .iter()
                    .any(|event| event.event.id.to_hex() == event_id)
            {
                batch.started = false;
                batch.needs_review = false;
                batch.needs_review_reason = None;
            }
        }
        self.transact(OperationKind::Update, next, vec![], now)
    }

    /// A successful native injection is durably delivered; an uncertain ack
    /// leaves the pre-send review record intact and must not auto-repeat it.
    pub fn finish_steer(&mut self, event_id: &str, now: DateTime<Utc>) -> Result<(), ParkError> {
        self.recover_pending(now)?;
        let mut bodies = vec![];
        let next = self
            .park
            .batches()
            .iter()
            .filter(|batch| {
                if batch.reason == ParkReason::Ingress
                    && batch
                        .events
                        .iter()
                        .any(|event| event.event.id.to_hex() == event_id)
                {
                    bodies.push(LedgerBody::TurnFinished(ledger::TurnFinished {
                        batch_id: batch.batch_id,
                        channel_id: batch.channel_id,
                        outcome: ledger::TurnOutcome::Ok,
                    }));
                    false
                } else {
                    true
                }
            })
            .cloned()
            .collect();
        self.transact(OperationKind::Update, next, bodies, now)
    }

    /// Release a successful live batch and its admission records with its audit.
    pub fn finish_live(&mut self, batch: &FlushBatch, now: DateTime<Utc>) -> Result<(), ParkError> {
        self.recover_pending(now)?;
        let ids: Vec<_> = batch
            .cancelled_events
            .iter()
            .chain(&batch.events)
            .map(|event| event.event.id)
            .collect();
        let next = self
            .park
            .batches()
            .iter()
            .filter(|existing| {
                !(existing.reason == ParkReason::Ingress
                    && existing
                        .events
                        .iter()
                        .any(|event| ids.contains(&event.event.id)))
            })
            .cloned()
            .collect();
        let body = LedgerBody::TurnFinished(ledger::TurnFinished {
            batch_id: batch.batch_id,
            channel_id: batch.channel_id,
            outcome: ledger::TurnOutcome::Ok,
        });
        self.transact(OperationKind::Update, next, vec![body], now)
    }

    /// Park a batch: the park file is written and fsynced first, then the
    /// `batch_parked` ledger record.
    ///
    /// On failure the caller retains the batch; a prepared operation may also
    /// own durable custody until recovery confirms all destination writes.
    pub fn park_batch(
        &mut self,
        batch: &FlushBatch,
        reason: ParkReason,
        started: bool,
        now: DateTime<Utc>,
    ) -> Result<(), ParkError> {
        self.recover_pending(now)?;
        if self
            .park
            .get(batch.batch_id)
            .is_some_and(|parked| parked.reason != ParkReason::Ingress)
        {
            return Ok(());
        }
        let mut parked = ParkedBatch::from_batch(batch, reason, started, now)?;
        parked.notice_pending = reason != ParkReason::Ingress;
        let events = parked.events.len();
        let ids: Vec<_> = parked.events.iter().map(|event| event.event.id).collect();
        let mut next: Vec<_> = self
            .park
            .batches()
            .iter()
            .filter(|existing| {
                existing.batch_id != batch.batch_id
                    && !(existing.reason == ParkReason::Ingress
                        && existing
                            .events
                            .iter()
                            .any(|event| ids.contains(&event.event.id)))
            })
            .cloned()
            .collect();
        next.push(parked);
        super::park::apply_scope_cap(&mut next);
        let mut bodies = vec![LedgerBody::BatchParked(ledger::BatchParked {
            batch_id: batch.batch_id,
            channel_id: batch.channel_id,
            reason: reason.as_str().to_string(),
            started,
            events,
        })];
        if started {
            bodies.push(LedgerBody::BatchNeedsReview(ledger::BatchNeedsReview {
                batch_id: batch.batch_id,
                channel_id: batch.channel_id,
                reason: "interrupted after it had started".to_string(),
            }));
        }
        if reason == ParkReason::Ingress {
            bodies.clear();
        }
        self.transact(OperationKind::Park(batch.batch_id), next, bodies, now)
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
        let mut batch_ids = Vec::new();
        let mut events = Vec::new();
        for batch in candidates {
            let batch_events = batch.to_batch_events();
            if !events.is_empty()
                && events.len() + batch_events.len() > crate::queue::MAX_BATCH_EVENTS
            {
                break;
            }
            if events.is_empty() && batch_events.len() > crate::queue::MAX_BATCH_EVENTS {
                batch_ids.push(batch.batch_id);
                events.extend(batch_events);
                break;
            }
            batch_ids.push(batch.batch_id);
            events.extend(batch_events);
        }
        if batch_ids.is_empty() {
            return None;
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
    ) -> Result<Uuid, ParkError> {
        if let Some(OperationKind::Replay { batches, replay_id }) = self.recover_operation(now)? {
            if batches == plan.batch_ids {
                return Ok(replay_id);
            }
            self.reconcile_on_start(now)?;
        }
        let mut next = self.park.batches().to_vec();
        for id in &plan.batch_ids {
            let batch = next
                .iter_mut()
                .find(|batch| batch.batch_id == *id)
                .ok_or_else(|| ParkError::Io(io::Error::other("replay batch no longer exists")))?;
            if batch.notice_pending {
                return Err(ParkError::Io(io::Error::other(
                    "replay awaits durable failure notice",
                )));
            }
            batch.replayed_at = Some(now);
        }
        let bodies = plan
            .batch_ids
            .iter()
            .map(|id| {
                LedgerBody::BatchReplayed(ledger::BatchReplayed {
                    batch_id: *id,
                    channel_id: plan.channel_id,
                    replay_of: new_batch_id,
                })
            })
            .collect();
        self.transact(
            OperationKind::Replay {
                batches: plan.batch_ids.clone(),
                replay_id: new_batch_id,
            },
            next,
            bodies,
            now,
        )?;
        Ok(new_batch_id)
    }

    /// Note that `plan`'s batches are riding on an in-flight turn for its scope.
    pub fn mark_replay_in_flight(&mut self, plan: &ReplayPlan) {
        self.in_flight_replays
            .insert(plan.scope.clone(), plan.batch_ids.clone());
    }

    /// A turn for `scope` finished successfully: any batches it was replaying
    /// leave the park file for good.
    ///
    /// In-flight ownership for `scope` is only cleared once every batch is
    /// actually removed. A batch that fails to remove stays recorded as
    /// in-flight for the scope so a later call (the next successful turn, or
    /// an explicit retry) can still find and finish it — dropping ownership
    /// on a partial failure would leave that batch stamped `replayed_at`
    /// forever with nothing left that knows to clean it up (T16 delta 1,
    /// finding 4b). The report carries every id actually released even when
    /// a later one in the same plan failed, so the caller can still write
    /// `turn_finished` for the ones that did land.
    pub fn finish_replay(&mut self, scope: &SessionScope) -> FinishReplayReport {
        let Some(batch_ids) = self.in_flight_replays.get(scope).cloned() else {
            return FinishReplayReport {
                released: Vec::new(),
                error: None,
            };
        };
        let next = self
            .park
            .batches()
            .iter()
            .filter(|batch| !batch_ids.contains(&batch.batch_id))
            .cloned()
            .collect();
        let bodies = batch_ids
            .iter()
            .map(|id| {
                LedgerBody::TurnFinished(ledger::TurnFinished {
                    batch_id: *id,
                    channel_id: scope.channel_id(),
                    outcome: ledger::TurnOutcome::Ok,
                })
            })
            .collect();
        match self.transact(OperationKind::Update, next, bodies, Utc::now()) {
            Ok(()) => {
                self.in_flight_replays.remove(scope);
                FinishReplayReport {
                    released: batch_ids,
                    error: None,
                }
            }
            Err(error) => FinishReplayReport {
                released: vec![],
                error: Some(error),
            },
        }
    }

    /// A dispatched replay failed: retain its batches for operator review,
    /// because side effects may already have started.
    pub fn abandon_replay(&mut self, scope: &SessionScope) {
        let Some(batch_ids) = self.in_flight_replays.get(scope).cloned() else {
            return;
        };
        let mut next = self.park.batches().to_vec();
        for batch in &mut next {
            if batch_ids.contains(&batch.batch_id) {
                // A failed dispatched replay may already have performed side
                // effects. Keep it review-only until an explicit operator retry.
                batch.needs_review = true;
                batch.needs_review_reason = Some("replay failed after dispatch".into());
                batch.replayed_at = None;
            }
        }
        if let Err(error) = self.transact(OperationKind::Update, next, vec![], Utc::now()) {
            tracing::error!(error = %error, "replay recovery remains pending");
        } else {
            self.in_flight_replays.remove(scope);
        }
    }

    /// Operator control frame `discard_batch`.
    ///
    /// [`DiscardOutcome::NotFound`] and a failed ledger record after a real
    /// destructive removal must never collapse into the same signal — an
    /// operator who sees "unknown batch" for a discard that actually
    /// happened has no way to tell it landed, and might discard-retry a
    /// batch id that no longer exists for a completely different reason
    /// (T16 delta 1, finding 12 / prior #14a).
    pub fn discard(
        &mut self,
        batch_id: Uuid,
        by: &str,
        now: DateTime<Utc>,
    ) -> Result<DiscardOutcome, ParkError> {
        if self.recover_operation(now)? == Some(OperationKind::Discard(batch_id)) {
            return Ok(DiscardOutcome::Discarded);
        }
        let Some(removed) = self.park.get(batch_id) else {
            return Ok(DiscardOutcome::NotFound);
        };
        if removed.notice_pending {
            return Err(ParkError::Io(io::Error::other(
                "discard awaits durable failure notice",
            )));
        }
        let body = LedgerBody::BatchDiscarded(ledger::BatchDiscarded {
            batch_id,
            channel_id: removed.channel_id,
            by: super::error_class::truncate_chars(by, ledger::MAX_LABEL_CHARS),
        });
        let next = self
            .park
            .batches()
            .iter()
            .filter(|batch| batch.batch_id != batch_id)
            .cloned()
            .collect();
        self.transact(OperationKind::Discard(batch_id), next, vec![body], now)?;
        Ok(DiscardOutcome::Discarded)
    }

    /// Clear a park's notice intent only after the durable outbox accepted it.
    pub fn mark_notice_enqueued(&mut self, batch_id: Uuid) -> Result<(), ParkError> {
        self.recover_pending(Utc::now())?;
        let mut next = self.park.batches().to_vec();
        let Some(batch) = next.iter_mut().find(|batch| batch.batch_id == batch_id) else {
            return Err(ParkError::Io(io::Error::other(
                "notice custody batch not found",
            )));
        };
        if !batch.notice_pending {
            return Ok(());
        }
        batch.notice_pending = false;
        self.transact(OperationKind::Update, next, vec![], Utc::now())
    }

    /// Operator control frame `replay_batch`: make one parked batch eligible
    /// again whatever its `started` flag.
    pub fn force_replay(&mut self, batch_id: Uuid) -> Result<bool, ParkError> {
        self.recover_pending(Utc::now())?;
        if self.park.get(batch_id).is_none() {
            return Ok(false);
        }
        if self
            .park
            .get(batch_id)
            .is_some_and(|batch| !self.channel_allowed(batch.channel_id))
        {
            return Err(ParkError::Io(io::Error::other(
                "channel access revoked; operator replay refused",
            )));
        }
        let mut next = self.park.batches().to_vec();
        if let Some(batch) = next.iter_mut().find(|batch| batch.batch_id == batch_id) {
            batch.needs_review = false;
            batch.needs_review_reason = None;
            batch.replayed_at = None;
            batch.forced = true;
        }
        self.transact(OperationKind::Update, next, vec![], Utc::now())?;
        Ok(true)
    }

    /// Start-up reconciliation: a batch with `batch_replayed` and no
    /// `turn_finished` moves to the review list, never to a second automatic
    /// replay.
    pub fn reconcile_on_start(&mut self, now: DateTime<Utc>) -> Result<ReconcileReport, ParkError> {
        let crashed = self.ledger.replays_without_finish()?;
        let (next, report) = self.park.preview_reconcile(&crashed, now);
        if !report.is_empty() {
            let bodies = next
                .iter()
                .filter(|batch| batch.needs_review)
                .map(|batch| {
                    LedgerBody::BatchNeedsReview(ledger::BatchNeedsReview {
                        batch_id: batch.batch_id,
                        channel_id: batch.channel_id,
                        reason: batch
                            .needs_review_reason
                            .clone()
                            .unwrap_or_else(|| "operator review required".into()),
                    })
                })
                .collect();
            self.transact(OperationKind::Update, next, bodies, now)?;
        }
        Ok(report)
    }

    /// A timer may probe using never-started parked input even with no newer
    /// live traffic. Selecting input does not consume a dispatch probe lease.
    pub fn stage_due_probes(
        &mut self,
        queue: &mut crate::queue::EventQueue,
        allowed_channels: &std::collections::HashSet<Uuid>,
        now: DateTime<Utc>,
    ) -> Result<(), ParkError> {
        if !self.is_ready() || !self.notices_ready() {
            return Ok(());
        }
        let candidates: Vec<_> = self
            .park
            .batches()
            .iter()
            .filter(|batch| {
                matches!(batch.reason, ParkReason::Pause | ParkReason::BreakerOpen)
                    && batch.replay_eligible()
                    && allowed_channels.contains(&batch.channel_id)
                    && self.channel_allowed(batch.channel_id)
                    && self.state.parked_probe_due(&batch.scope(), now)
            })
            .cloned()
            .collect();
        for batch in candidates {
            let scope = batch.scope();
            if !queue.can_stage_replay(&scope) {
                continue;
            }
            let plan = ReplayPlan {
                batch_ids: vec![batch.batch_id],
                channel_id: batch.channel_id,
                scope: scope.clone(),
                events: batch.to_batch_events(),
            };
            let id = self.commit_replay(&plan, Uuid::new_v4(), now)?;
            if !queue.stage_replay_with_id(scope, plan.events, id) {
                return Err(ParkError::Io(io::Error::other(
                    "probe staging refused; custody remains durable",
                )));
            }
            if self.state.paused_until().is_some() {
                break;
            }
        }
        Ok(())
    }

    /// Retry explicit replay requests after transient notice/queue blockage.
    /// This does not treat scheduling as provider success or lift containment.
    pub fn stage_forced_replays(
        &mut self,
        queue: &mut crate::queue::EventQueue,
        allowed_channels: &std::collections::HashSet<Uuid>,
        now: DateTime<Utc>,
    ) -> Result<(), ParkError> {
        if !self.is_ready() || !self.notices_ready() {
            return Ok(());
        }
        let candidates: Vec<_> = self
            .park
            .batches()
            .iter()
            .filter(|batch| {
                batch.forced
                    && batch.replay_eligible()
                    && allowed_channels.contains(&batch.channel_id)
                    && self.channel_allowed(batch.channel_id)
            })
            .cloned()
            .collect();
        for batch in candidates {
            let scope = batch.scope();
            if !queue.can_stage_replay(&scope) {
                continue;
            }
            let plan = ReplayPlan {
                batch_ids: vec![batch.batch_id],
                channel_id: batch.channel_id,
                scope: scope.clone(),
                events: batch.to_batch_events(),
            };
            let id = self.commit_replay(&plan, Uuid::new_v4(), now)?;
            if !queue.stage_replay_with_id(scope, plan.events, id) {
                return Err(ParkError::Io(io::Error::other(
                    "forced replay staging refused; durable custody remains",
                )));
            }
        }
        Ok(())
    }

    /// Periodic maintenance: truncate the ledger to its retention window every
    /// six hours.
    pub fn maintain(&mut self, now: DateTime<Utc>) -> TruncateReport {
        if let Err(error) = self.recover_pending(now) {
            self.transaction_failed = true;
            tracing::error!(error = %error, "custody recovery failed; dispatch remains blocked");
            return TruncateReport::default();
        }
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
mod tests;
