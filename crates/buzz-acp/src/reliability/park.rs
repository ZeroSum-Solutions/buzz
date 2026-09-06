//! The park file, `state/parked.jsonl`.
//!
//! One line per parked batch, carrying the serialized events and their prompt
//! tags, keyed by `batch_id`. Nothing the harness parks is ever discarded: a
//! batch leaves this file only when it has been replayed and finished, or when
//! an operator discarded it through a control frame.
//!
//! Parked batches hold client messages. The file lives only in the agent state
//! directory at 0600 and is never sent anywhere except back to the same agent.
//!
//! Design: `docs/plans/2026-09-06-harness-reliability-design.md`, "Park file".

use std::io::{self, BufRead, Read};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::queue::{BatchEvent, FlushBatch};
use crate::scope::SessionScope;

use super::error_class::truncate_chars;
use super::state_dir;

/// File name inside the state directory.
pub const PARK_FILE: &str = "parked.jsonl";

/// Hard cap on the park file. A park that would exceed it fails rather than
/// evicting a client message: the caller keeps the batch in memory, logs and
/// counts the failure, and the operator is told in the next notice.
pub const MAX_PARK_BYTES: u64 = 10 * 1024 * 1024;

/// Most parked batches held for a single scope. Beyond it the oldest
/// replay-eligible batches for that scope move to `needs_review`, so one broken
/// conversation cannot fill the automatic-replay path.
pub const MAX_PARKED_PER_SCOPE: usize = 100;

/// Most parked batches held in total.
pub const MAX_PARKED_TOTAL: usize = 1_000;

/// Most events kept from one batch. Matches the queue's per-batch cap.
pub const MAX_PARKED_EVENTS: usize = 50;

/// Longest single line read back.
pub const MAX_LINE_BYTES: usize = 512 * 1024;

/// A replay-eligible batch older than this moves to `needs_review`: answering a
/// week-old message unprompted is worse than asking the operator.
pub const REPLAY_MAX_AGE_DAYS: i64 = 7;

/// Longest message excerpt shown in the CLI or a notice.
pub const EXCERPT_CHARS: usize = 120;

/// Longest prompt tag stored.
pub const MAX_TAG_CHARS: usize = 64;

/// Longest reason string stored.
pub const MAX_REASON_CHARS: usize = 128;

/// Why a batch was parked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParkReason {
    /// The retry budget ran out.
    RetriesExhausted,
    /// The turn hit the hard wall-clock cap.
    HardTimeout,
    /// The provider rejected the credentials.
    Auth,
    /// A scope breaker stayed open for its whole six-hour budget.
    BreakerExpired,
}

impl ParkReason {
    /// The `reason` string written to the ledger.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RetriesExhausted => "retries_exhausted",
            Self::HardTimeout => "hard_timeout",
            Self::Auth => "auth",
            Self::BreakerExpired => "breaker_expired",
        }
    }
}

/// A session scope in a form that survives a round trip through JSON.
///
/// `SessionScope` is the in-memory key; this is its serialized shape. The
/// thread root id is validated on the way back in: it reaches the harness from
/// the relay and only a 64-character lowercase hex string is a real one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeRef {
    pub channel_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_event_id: Option<String>,
}

impl ScopeRef {
    /// Project a live scope into its serialized shape.
    pub fn from_scope(scope: &SessionScope) -> Self {
        match scope {
            SessionScope::Conversation { channel_id } => Self {
                channel_id: *channel_id,
                root_event_id: None,
            },
            SessionScope::Thread {
                channel_id,
                root_event_id,
            } => Self {
                channel_id: *channel_id,
                root_event_id: Some(truncate_chars(root_event_id, 64)),
            },
        }
    }

    /// Rebuild the live scope. A root id that is not 64 lowercase hex
    /// characters is not a thread root, so the batch belongs to the
    /// conversation scope rather than to a scope no session will ever match.
    pub fn to_scope(&self) -> SessionScope {
        match &self.root_event_id {
            Some(root)
                if root.len() == 64
                    && root
                        .chars()
                        .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)) =>
            {
                SessionScope::Thread {
                    channel_id: self.channel_id,
                    root_event_id: root.clone(),
                }
            }
            _ => SessionScope::Conversation {
                channel_id: self.channel_id,
            },
        }
    }
}

/// One event inside a parked batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParkedEvent {
    /// The original signed event. Its text is never edited.
    pub event: nostr::Event,
    /// Which prompt rule matched it.
    pub prompt_tag: String,
    /// When the harness admitted it, in wall-clock terms so a replay can say
    /// "first at HH:MM, last at HH:MM" after a restart.
    pub received_at: DateTime<Utc>,
}

impl ParkedEvent {
    /// A bounded excerpt of the event text, for the CLI and for notices.
    pub fn excerpt(&self) -> String {
        truncate_chars(self.event.content.trim(), EXCERPT_CHARS)
    }
}

/// One parked batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParkedBatch {
    pub batch_id: Uuid,
    pub channel_id: Uuid,
    pub scope: ScopeRef,
    pub reason: ParkReason,
    /// Whether the harness saw agent output or a tool call for this batch's
    /// turn. A started batch never replays on its own.
    pub started: bool,
    /// Whether the batch waits for an operator rather than for a probe.
    #[serde(default)]
    pub needs_review: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs_review_reason: Option<String>,
    /// Set immediately before a replay prompt is sent. A batch that still has
    /// this set at start-up crashed mid-replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replayed_at: Option<DateTime<Utc>>,
    /// Set by the operator's `replay_batch` control frame. It is the only way a
    /// batch that had started becomes replay-eligible.
    #[serde(default)]
    pub forced: bool,
    pub parked_at: DateTime<Utc>,
    pub events: Vec<ParkedEvent>,
}

impl ParkedBatch {
    /// Park a live batch. Events past [`MAX_PARKED_EVENTS`] are refused rather
    /// than silently trimmed — the queue never builds a larger batch, so a
    /// larger one is a bug, not a message to drop.
    pub fn from_batch(
        batch: &FlushBatch,
        reason: ParkReason,
        started: bool,
        now: DateTime<Utc>,
    ) -> Result<Self, ParkError> {
        if batch.events.len() > MAX_PARKED_EVENTS {
            return Err(ParkError::TooManyEvents(batch.events.len()));
        }
        let events = batch
            .events
            .iter()
            .map(|be| ParkedEvent {
                event: be.event.clone(),
                prompt_tag: truncate_chars(&be.prompt_tag, MAX_TAG_CHARS),
                received_at: DateTime::from_timestamp(be.event.created_at.as_secs() as i64, 0)
                    .unwrap_or(now),
            })
            .collect();
        Ok(Self {
            batch_id: batch.batch_id,
            channel_id: batch.channel_id,
            scope: ScopeRef::from_scope(&batch.scope),
            reason,
            started,
            needs_review: started,
            needs_review_reason: started.then(|| "interrupted after it had started".to_string()),
            replayed_at: None,
            forced: false,
            parked_at: now,
            events,
        })
    }

    /// Whether this batch may replay on its own after a successful probe.
    pub fn replay_eligible(&self) -> bool {
        (!self.started || self.forced) && !self.needs_review && self.replayed_at.is_none()
    }

    /// Rebuild the batch events for a replay prompt.
    pub fn to_batch_events(&self) -> Vec<BatchEvent> {
        self.events
            .iter()
            .map(|pe| BatchEvent {
                event: pe.event.clone(),
                prompt_tag: pe.prompt_tag.clone(),
                received_at: std::time::Instant::now(),
            })
            .collect()
    }

    /// The live scope this batch belongs to.
    pub fn scope(&self) -> SessionScope {
        self.scope.to_scope()
    }
}

/// Why a park could not be recorded.
#[derive(Debug, thiserror::Error)]
pub enum ParkError {
    /// The park file is at its cap. Nothing was written; the caller keeps the
    /// batch.
    #[error("park file is full ({0} batches, {1} bytes) — the batch was NOT parked")]
    Full(usize, u64),
    /// A batch larger than the queue can build.
    #[error("batch carries {0} events, over the park file's per-batch cap")]
    TooManyEvents(usize),
    /// The write itself failed.
    #[error("park file write failed: {0}")]
    Io(#[from] io::Error),
}

/// What a start-up reconciliation pass changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Batches that had been sent for replay and never finished.
    pub crashed_mid_replay: usize,
    /// Replay-eligible batches older than [`REPLAY_MAX_AGE_DAYS`].
    pub aged_out: usize,
    /// Replay-eligible batches over a scope's cap.
    pub over_scope_cap: usize,
}

impl ReconcileReport {
    /// Whether the pass changed anything.
    pub fn is_empty(&self) -> bool {
        self.crashed_mid_replay == 0 && self.aged_out == 0 && self.over_scope_cap == 0
    }
}

/// The park file and its in-memory image.
pub struct ParkFile {
    path: PathBuf,
    batches: Vec<ParkedBatch>,
    write_failures: u64,
}

impl ParkFile {
    /// Open (or create) the park file in `dir`.
    pub fn open(dir: &Path) -> io::Result<Self> {
        state_dir::ensure_dir(dir)?;
        let path = dir.join(PARK_FILE);
        drop(state_dir::open_append(&path)?);
        let batches = read_batches(&path)?;
        Ok(Self {
            path,
            batches,
            write_failures: 0,
        })
    }

    /// Path of the park file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes that failed. Never reset.
    pub fn write_failures(&self) -> u64 {
        self.write_failures
    }

    /// Every parked batch, oldest first.
    pub fn batches(&self) -> &[ParkedBatch] {
        &self.batches
    }

    /// Whether `batch_id` is already parked.
    pub fn contains(&self, batch_id: Uuid) -> bool {
        self.batches.iter().any(|b| b.batch_id == batch_id)
    }

    /// Park one batch, writing the file before the caller drops the batch.
    ///
    /// Returns `Err` when nothing was written, so a caller that keeps its own
    /// copy knows the batch is still its responsibility.
    pub fn park(&mut self, batch: ParkedBatch) -> Result<(), ParkError> {
        if self.batches.iter().any(|b| b.batch_id == batch.batch_id) {
            // Idempotent: a retried park of the same batch is not a second copy.
            return Ok(());
        }
        if self.batches.len() >= MAX_PARKED_TOTAL {
            return Err(ParkError::Full(self.batches.len(), self.byte_size()));
        }
        let mut next = self.batches.clone();
        next.push(batch);
        let bytes = serialize(&next)?;
        if bytes.len() as u64 > MAX_PARK_BYTES {
            return Err(ParkError::Full(next.len(), bytes.len() as u64));
        }
        self.commit(next, bytes)?;
        self.enforce_scope_cap()?;
        Ok(())
    }

    /// Remove a batch entirely. Used by `discard_batch` and once a replayed
    /// batch has finished.
    pub fn remove(&mut self, batch_id: Uuid) -> Result<Option<ParkedBatch>, ParkError> {
        let Some(index) = self.batches.iter().position(|b| b.batch_id == batch_id) else {
            return Ok(None);
        };
        let mut next = self.batches.clone();
        let removed = next.remove(index);
        let bytes = serialize(&next)?;
        self.commit(next, bytes)?;
        Ok(Some(removed))
    }

    /// Stamp `replayed_at` on a batch. Written before the replay prompt is sent
    /// so a crash between the two is visible at the next start.
    pub fn mark_replayed(&mut self, batch_id: Uuid, at: DateTime<Utc>) -> Result<(), ParkError> {
        self.mutate(batch_id, |batch| batch.replayed_at = Some(at))
    }

    /// Clear a replay stamp after the replay turn failed, so the batch is
    /// eligible again on the next successful probe.
    pub fn unmark_replayed(&mut self, batch_id: Uuid) -> Result<(), ParkError> {
        self.mutate(batch_id, |batch| batch.replayed_at = None)
    }

    /// Move a batch to the review list.
    pub fn mark_needs_review(&mut self, batch_id: Uuid, reason: &str) -> Result<(), ParkError> {
        let reason = truncate_chars(reason, MAX_REASON_CHARS);
        self.mutate(batch_id, move |batch| {
            batch.needs_review = true;
            batch.needs_review_reason = Some(reason.clone());
            batch.replayed_at = None;
        })
    }

    /// Operator override: make a batch replay-eligible whatever its `started`
    /// flag, and take it off the review list.
    pub fn clear_review(&mut self, batch_id: Uuid) -> Result<(), ParkError> {
        self.mutate(batch_id, |batch| {
            batch.needs_review = false;
            batch.needs_review_reason = None;
            batch.replayed_at = None;
            batch.forced = true;
        })
    }

    /// Batches for `scope` that may replay on their own, oldest first.
    pub fn replay_candidates(&self, scope: &SessionScope) -> Vec<&ParkedBatch> {
        let mut candidates: Vec<&ParkedBatch> = self
            .batches
            .iter()
            .filter(|b| b.replay_eligible() && &b.scope() == scope)
            .collect();
        candidates.sort_by_key(|b| b.parked_at);
        candidates
    }

    /// One parked batch by id.
    pub fn get(&self, batch_id: Uuid) -> Option<&ParkedBatch> {
        self.batches.iter().find(|b| b.batch_id == batch_id)
    }

    /// Start-up reconciliation.
    ///
    /// `crashed` is the batch-id list the ledger reports as replayed with no
    /// `turn_finished`. Those, and replay-eligible batches older than
    /// [`REPLAY_MAX_AGE_DAYS`], move to the review list; nothing is removed.
    pub fn reconcile_on_start(
        &mut self,
        crashed: &[Uuid],
        now: DateTime<Utc>,
    ) -> Result<ReconcileReport, ParkError> {
        let cutoff = now - Duration::days(REPLAY_MAX_AGE_DAYS);
        let mut report = ReconcileReport::default();
        let mut next = self.batches.clone();
        for batch in next.iter_mut() {
            if !batch.needs_review
                && (crashed.contains(&batch.batch_id) || batch.replayed_at.is_some())
            {
                batch.needs_review = true;
                batch.needs_review_reason =
                    Some("replay was sent but the turn never finished".to_string());
                batch.replayed_at = None;
                report.crashed_mid_replay += 1;
                continue;
            }
            if batch.replay_eligible() && batch.parked_at < cutoff {
                batch.needs_review = true;
                batch.needs_review_reason =
                    Some(format!("waited more than {REPLAY_MAX_AGE_DAYS} days"));
                report.aged_out += 1;
            }
        }
        if !report.is_empty() {
            let bytes = serialize(&next)?;
            self.commit(next, bytes)?;
        }
        report.over_scope_cap = self.enforce_scope_cap()?;
        Ok(report)
    }

    /// Demote the oldest replay-eligible batches of any scope over
    /// [`MAX_PARKED_PER_SCOPE`] to the review list. Returns how many moved.
    fn enforce_scope_cap(&mut self) -> Result<usize, ParkError> {
        use std::collections::HashMap;

        let mut per_scope: HashMap<SessionScope, Vec<usize>> = HashMap::new();
        for (index, batch) in self.batches.iter().enumerate() {
            if batch.replay_eligible() {
                per_scope.entry(batch.scope()).or_default().push(index);
            }
        }
        let mut demote: Vec<usize> = Vec::new();
        for indices in per_scope.values() {
            if indices.len() > MAX_PARKED_PER_SCOPE {
                demote.extend(&indices[..indices.len() - MAX_PARKED_PER_SCOPE]);
            }
        }
        if demote.is_empty() {
            return Ok(0);
        }
        let mut next = self.batches.clone();
        for index in &demote {
            let batch = &mut next[*index];
            batch.needs_review = true;
            batch.needs_review_reason = Some(format!(
                "more than {MAX_PARKED_PER_SCOPE} batches were waiting for this conversation"
            ));
        }
        let bytes = serialize(&next)?;
        self.commit(next, bytes)?;
        Ok(demote.len())
    }

    fn mutate(
        &mut self,
        batch_id: Uuid,
        apply: impl Fn(&mut ParkedBatch),
    ) -> Result<(), ParkError> {
        let Some(index) = self.batches.iter().position(|b| b.batch_id == batch_id) else {
            return Ok(());
        };
        let mut next = self.batches.clone();
        apply(&mut next[index]);
        let bytes = serialize(&next)?;
        self.commit(next, bytes)
    }

    /// Write the new image atomically, then adopt it. The in-memory image only
    /// changes once the bytes are on disk, so a failed write leaves the caller
    /// looking at exactly what the file holds.
    fn commit(&mut self, next: Vec<ParkedBatch>, bytes: Vec<u8>) -> Result<(), ParkError> {
        if let Err(error) = state_dir::write_atomic(&self.path, &bytes) {
            self.write_failures = self.write_failures.saturating_add(1);
            return Err(ParkError::Io(error));
        }
        self.batches = next;
        Ok(())
    }

    fn byte_size(&self) -> u64 {
        serialize(&self.batches)
            .map(|b| b.len() as u64)
            .unwrap_or(0)
    }
}

fn serialize(batches: &[ParkedBatch]) -> Result<Vec<u8>, ParkError> {
    let mut buffer = Vec::new();
    for batch in batches {
        serde_json::to_writer(&mut buffer, batch)
            .map_err(|e| ParkError::Io(io::Error::new(io::ErrorKind::InvalidData, e)))?;
        buffer.push(b'\n');
    }
    Ok(buffer)
}

/// Read the park file with a hard byte cap on the input and a hard cap per
/// line. Unreadable lines are counted and skipped.
fn read_batches(path: &Path) -> io::Result<Vec<ParkedBatch>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut reader = io::BufReader::new(file.take(MAX_PARK_BYTES));
    let mut batches = Vec::new();
    let mut skipped = 0usize;
    let mut line = Vec::new();
    loop {
        if batches.len() >= MAX_PARKED_TOTAL {
            tracing::warn!(
                cap = MAX_PARKED_TOTAL,
                path = %path.display(),
                "park file holds more batches than the cap — ignoring the rest of the file"
            );
            break;
        }
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        if line.len() > MAX_LINE_BYTES {
            skipped += 1;
            continue;
        }
        let text = match std::str::from_utf8(&line) {
            Ok(text) => text.trim(),
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        if text.is_empty() {
            continue;
        }
        match serde_json::from_str::<ParkedBatch>(text) {
            Ok(mut batch) => {
                batch.events.truncate(MAX_PARKED_EVENTS);
                batches.push(batch);
            }
            Err(_) => skipped += 1,
        }
    }
    if skipped > 0 {
        tracing::warn!(
            skipped,
            path = %path.display(),
            "skipped unreadable park file lines"
        );
    }
    batches.sort_by_key(|b| b.parked_at);
    Ok(batches)
}
