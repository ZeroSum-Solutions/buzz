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
    /// The agent was paused due to provider capacity limit.
    Pause,
    /// The scope breaker opened due to repeated failures.
    BreakerOpen,
    /// The agent task panicked after the turn had already produced output or
    /// a tool call; parked directly rather than risking an auto-replay of
    /// already-started work through the ordinary retry loop.
    Panic,
}

impl ParkReason {
    /// The `reason` string written to the ledger.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RetriesExhausted => "retries_exhausted",
            Self::HardTimeout => "hard_timeout",
            Self::Auth => "auth",
            Self::BreakerExpired => "breaker_expired",
            Self::Pause => "pause",
            Self::BreakerOpen => "breaker_open",
            Self::Panic => "panic",
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
    ///
    /// `cancelled_events` — the carryover from an interrupted or replayed
    /// prior turn that `FlushBatch` keeps separate for prompt framing — are
    /// persisted too, ordered before `events` the same way
    /// `requeue_preserve_timestamps` restores them: "original before newer".
    /// A version that parked only `events` silently erased every interrupted
    /// or in-flight-replay message the moment its batch got parked instead of
    /// requeued (T16 delta 1, finding 3).
    pub fn from_batch(
        batch: &FlushBatch,
        reason: ParkReason,
        started: bool,
        now: DateTime<Utc>,
    ) -> Result<Self, ParkError> {
        let total_events = batch.cancelled_events.len() + batch.events.len();
        if total_events > MAX_PARKED_EVENTS {
            return Err(ParkError::TooManyEvents(total_events));
        }
        let to_parked_event = |be: &BatchEvent| ParkedEvent {
            event: be.event.clone(),
            prompt_tag: truncate_chars(&be.prompt_tag, MAX_TAG_CHARS),
            received_at: DateTime::from_timestamp(be.event.created_at.as_secs() as i64, 0)
                .unwrap_or(now),
        };
        let events = batch
            .cancelled_events
            .iter()
            .chain(batch.events.iter())
            .map(to_parked_event)
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
    /// A single batch line exceeds the per-line cap.
    #[error("serialized batch is {0} bytes, over the {MAX_LINE_BYTES}-byte line cap — the batch was NOT parked")]
    LineTooLong(usize),
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
        apply_scope_cap(&mut next);
        let bytes = serialize(&next)?;
        if bytes.len() as u64 > MAX_PARK_BYTES {
            return Err(ParkError::Full(next.len(), bytes.len() as u64));
        }
        self.commit(next, bytes)?;
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
        report.over_scope_cap = apply_scope_cap(&mut next);
        if !report.is_empty() {
            let bytes = serialize(&next)?;
            self.commit(next, bytes)?;
        }
        Ok(report)
    }

    /// Demote the oldest replay-eligible batches of any scope over
    /// [`MAX_PARKED_PER_SCOPE`] to the review list. Returns how many moved.
    #[allow(dead_code)]
    fn enforce_scope_cap(&mut self) -> Result<usize, ParkError> {
        let mut next = self.batches.clone();
        let demoted = apply_scope_cap(&mut next);
        if demoted == 0 {
            return Ok(0);
        }
        let bytes = serialize(&next)?;
        self.commit(next, bytes)?;
        Ok(demoted)
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

/// Demote the oldest replay-eligible batches of any scope over
/// [`MAX_PARKED_PER_SCOPE`] to the review list. Returns how many moved.
fn apply_scope_cap(batches: &mut [ParkedBatch]) -> usize {
    use std::collections::HashMap;

    let mut per_scope: HashMap<SessionScope, Vec<usize>> = HashMap::new();
    for (index, batch) in batches.iter().enumerate() {
        if batch.replay_eligible() {
            per_scope.entry(batch.scope()).or_default().push(index);
        }
    }
    let mut demote_count = 0;
    for indices in per_scope.values() {
        if indices.len() > MAX_PARKED_PER_SCOPE {
            for &index in &indices[..indices.len() - MAX_PARKED_PER_SCOPE] {
                let batch = &mut batches[index];
                batch.needs_review = true;
                batch.needs_review_reason = Some(format!(
                    "more than {MAX_PARKED_PER_SCOPE} batches were waiting for this conversation"
                ));
                demote_count += 1;
            }
        }
    }
    demote_count
}

fn serialize(batches: &[ParkedBatch]) -> Result<Vec<u8>, ParkError> {
    let mut buffer = Vec::new();
    for batch in batches {
        let start = buffer.len();
        serde_json::to_writer(&mut buffer, batch)
            .map_err(|e| ParkError::Io(io::Error::new(io::ErrorKind::InvalidData, e)))?;
        buffer.push(b'\n');
        let line_len = buffer.len() - start;
        if line_len > MAX_LINE_BYTES {
            return Err(ParkError::LineTooLong(line_len));
        }
    }
    Ok(buffer)
}

/// Suffix of the sibling file an unreadable park line is copied to, verbatim,
/// before it is dropped from the live in-memory image.
const QUARANTINE_SUFFIX: &str = ".corrupt";

/// Read the park file with a hard byte cap on the input and a hard cap per
/// line.
///
/// A line this cannot use — too long, not UTF-8, not valid JSON, or (once
/// parsed) carrying more events than [`MAX_PARKED_EVENTS`] — is never
/// silently modified or dropped without a trace. Every such line is copied
/// verbatim to a `.corrupt` sibling file (best-effort) before being excluded
/// from the live batches, so an operator can recover the original bytes
/// instead of the client messages in it simply vanishing on the next read (T16
/// delta 1, finding 6).
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
            quarantine_line(path, &line);
            continue;
        }
        let text = match std::str::from_utf8(&line) {
            Ok(text) => text.trim(),
            Err(_) => {
                skipped += 1;
                quarantine_line(path, &line);
                continue;
            }
        };
        if text.is_empty() {
            continue;
        }
        match serde_json::from_str::<ParkedBatch>(text) {
            Ok(batch) if batch.events.len() > MAX_PARKED_EVENTS => {
                // A syntactically valid record with more events than the
                // cap allows is corruption (or a future/incompatible
                // format), not a batch to admit with its tail silently cut
                // off — every event past the cap would otherwise vanish
                // with the read reporting success.
                tracing::error!(
                    batch_id = %batch.batch_id,
                    events = batch.events.len(),
                    cap = MAX_PARKED_EVENTS,
                    path = %path.display(),
                    "parked batch carries more events than the cap allows — \
                     quarantining the whole record rather than truncating it"
                );
                skipped += 1;
                quarantine_line(path, &line);
            }
            Ok(batch) => batches.push(batch),
            Err(_) => {
                skipped += 1;
                quarantine_line(path, &line);
            }
        }
    }
    if skipped > 0 {
        tracing::error!(
            skipped,
            path = %path.display(),
            "unreadable park file lines were quarantined to a .corrupt sibling \
             file rather than dropped — operator recovery required"
        );
    }
    batches.sort_by_key(|b| b.parked_at);
    Ok(batches)
}

/// Best-effort: append `line` verbatim to `<path>.corrupt`. A failure here is
/// logged, never propagated — quarantining is a courtesy on top of the
/// primary guarantee (the line is excluded from the live batches either way),
/// not itself load-bearing for correctness.
fn quarantine_line(path: &Path, line: &[u8]) {
    use std::io::Write as _;

    let quarantine_path = {
        let mut name = path.as_os_str().to_owned();
        name.push(QUARANTINE_SUFFIX);
        PathBuf::from(name)
    };
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Same 0600 owner-only mode every other state file gets — this file
        // can hold client message content.
        options.mode(0o600);
    }
    let result = options.open(&quarantine_path).and_then(|mut file| {
        file.write_all(line)?;
        if line.last() != Some(&b'\n') {
            file.write_all(b"\n")?;
        }
        Ok(())
    });
    if let Err(error) = result {
        tracing::error!(
            path = %quarantine_path.display(),
            error = %error,
            "could not quarantine an unreadable park file line — it is still \
             excluded from the live batches, but its original bytes are lost"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{EventBuilder, Keys, Kind};

    fn dummy_event(content: &str) -> nostr::Event {
        let keys = Keys::generate();
        EventBuilder::new(Kind::Custom(9), content)
            .sign_with_keys(&keys)
            .unwrap()
    }

    fn dummy_batch(
        channel_id: Uuid,
        batch_id: Uuid,
        scope: SessionScope,
        content: &str,
    ) -> FlushBatch {
        FlushBatch {
            batch_id,
            channel_id,
            scope,
            events: vec![BatchEvent {
                event: dummy_event(content),
                prompt_tag: "test".into(),
                received_at: std::time::Instant::now(),
            }],
            cancelled_events: vec![],
            cancel_reason: None,
            started: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    #[test]
    fn test_park_file_basic_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut park = ParkFile::open(dir.path()).unwrap();
        let ch = Uuid::new_v4();
        let b_id = Uuid::new_v4();
        let scope = SessionScope::Conversation { channel_id: ch };
        let batch = dummy_batch(ch, b_id, scope, "payload");
        let parked =
            ParkedBatch::from_batch(&batch, ParkReason::RetriesExhausted, false, Utc::now())
                .unwrap();
        park.park(parked).unwrap();
        assert!(park.contains(b_id));
        assert_eq!(park.batches().len(), 1);

        // Reopen from disk
        let reopened = ParkFile::open(dir.path()).unwrap();
        assert!(reopened.contains(b_id));
        assert_eq!(reopened.batches().len(), 1);
    }

    // T16 delta 1, finding 3: a batch carrying `cancelled_events` (the
    // carryover from an interrupted or in-flight-replay turn) must not lose
    // them just because the batch itself ends up parked instead of
    // requeued.
    #[test]
    fn test_from_batch_preserves_cancelled_events_ordered_before_new_ones() {
        let channel_id = Uuid::new_v4();
        let scope = SessionScope::Conversation { channel_id };
        let mut batch = dummy_batch(channel_id, Uuid::new_v4(), scope, "new message");
        batch.cancelled_events = vec![BatchEvent {
            event: dummy_event("interrupted message"),
            prompt_tag: "test".into(),
            received_at: std::time::Instant::now(),
        }];
        batch.cancel_reason = Some(crate::queue::CancelReason::Interrupt);

        let parked =
            ParkedBatch::from_batch(&batch, ParkReason::RetriesExhausted, false, Utc::now())
                .unwrap();

        assert_eq!(
            parked.events.len(),
            2,
            "both the cancelled carryover and the new event must be persisted"
        );
        assert_eq!(
            parked.events[0].event.content, "interrupted message",
            "the cancelled carryover must come first, matching the \
             original-before-newer ordering used everywhere else"
        );
        assert_eq!(parked.events[1].event.content, "new message");
    }

    #[test]
    fn test_from_batch_rejects_when_cancelled_plus_new_events_exceed_the_cap() {
        let channel_id = Uuid::new_v4();
        let scope = SessionScope::Conversation { channel_id };
        let mut batch = dummy_batch(channel_id, Uuid::new_v4(), scope.clone(), "new");
        // events.len() == 1 already; add MAX_PARKED_EVENTS more via cancelled
        // carryover so the combined total is one over the cap.
        batch.cancelled_events = (0..MAX_PARKED_EVENTS)
            .map(|i| BatchEvent {
                event: dummy_event(&format!("cancelled-{i}")),
                prompt_tag: "test".into(),
                received_at: std::time::Instant::now(),
            })
            .collect();

        let result =
            ParkedBatch::from_batch(&batch, ParkReason::RetriesExhausted, false, Utc::now());
        assert!(
            matches!(result, Err(ParkError::TooManyEvents(n)) if n == MAX_PARKED_EVENTS + 1),
            "the cap must count cancelled_events + events together, not just \
             events alone: {result:?}"
        );
    }

    #[test]
    fn test_park_rejects_oversized_individual_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut park = ParkFile::open(dir.path()).unwrap();
        let ch = Uuid::new_v4();
        let b_id = Uuid::new_v4();
        let scope = SessionScope::Conversation { channel_id: ch };
        // Create an event whose serialized line exceeds MAX_LINE_BYTES (512 KB),
        // but whose size is well under MAX_PARK_BYTES (10 MB).
        let big_content = "x".repeat(MAX_LINE_BYTES + 1024);
        let batch = dummy_batch(ch, b_id, scope, &big_content);
        let parked =
            ParkedBatch::from_batch(&batch, ParkReason::RetriesExhausted, false, Utc::now())
                .unwrap();

        let result = park.park(parked);
        assert!(
            result.is_err(),
            "park() must reject line exceeding MAX_LINE_BYTES"
        );
        assert!(!park.contains(b_id));
        assert!(park.batches().is_empty());
    }

    // T16 delta 1, finding 6: a syntactically valid on-disk record with more
    // events than MAX_PARKED_EVENTS must be quarantined whole, never
    // silently truncated and admitted as if nothing were wrong.
    #[test]
    fn test_read_batches_quarantines_rather_than_truncates_an_oversized_record() {
        let dir = tempfile::tempdir().unwrap();
        let channel_id = Uuid::new_v4();
        let batch_id = Uuid::new_v4();
        let over_cap = ParkedBatch {
            batch_id,
            channel_id,
            scope: ScopeRef::from_scope(&SessionScope::Conversation { channel_id }),
            reason: ParkReason::RetriesExhausted,
            started: false,
            needs_review: false,
            needs_review_reason: None,
            replayed_at: None,
            forced: false,
            parked_at: Utc::now(),
            events: (0..MAX_PARKED_EVENTS + 1)
                .map(|i| ParkedEvent {
                    event: dummy_event(&format!("event-{i}")),
                    prompt_tag: "test".into(),
                    received_at: Utc::now(),
                })
                .collect(),
        };
        let line = serde_json::to_string(&over_cap).unwrap();

        let park_path = dir.path().join(PARK_FILE);
        std::fs::write(&park_path, format!("{line}\n")).unwrap();

        let park = ParkFile::open(dir.path()).unwrap();
        assert!(
            !park.contains(batch_id),
            "an over-cap record must never be admitted, truncated or otherwise"
        );
        assert!(
            park.batches().is_empty(),
            "no events from the over-cap record may survive into the live image"
        );

        let quarantine_path = dir.path().join(format!("{PARK_FILE}{QUARANTINE_SUFFIX}"));
        let quarantined = std::fs::read_to_string(&quarantine_path)
            .expect("the original record must be preserved in the quarantine file");
        assert!(
            quarantined.contains(&batch_id.to_string()),
            "the quarantined line must be the original record, recoverable by an operator"
        );
    }

    #[test]
    fn test_reconcile_on_start_crashed_mid_replay_moves_to_needs_review() {
        let dir = tempfile::tempdir().unwrap();
        let mut park = ParkFile::open(dir.path()).unwrap();
        let ch = Uuid::new_v4();
        let b_id = Uuid::new_v4();
        let scope = SessionScope::Conversation { channel_id: ch };
        let batch = dummy_batch(ch, b_id, scope, "payload");
        let parked =
            ParkedBatch::from_batch(&batch, ParkReason::RetriesExhausted, false, Utc::now())
                .unwrap();
        park.park(parked).unwrap();
        assert!(!park.get(b_id).unwrap().needs_review);

        let report = park.reconcile_on_start(&[b_id], Utc::now()).unwrap();
        assert_eq!(report.crashed_mid_replay, 1);
        let updated = park.get(b_id).unwrap();
        assert!(updated.needs_review);
        assert!(!updated.replay_eligible());
        assert_eq!(
            updated.needs_review_reason.as_deref(),
            Some("replay was sent but the turn never finished")
        );
    }

    #[test]
    fn test_replay_candidates_filters_started_batches() {
        let dir = tempfile::tempdir().unwrap();
        let mut park = ParkFile::open(dir.path()).unwrap();
        let ch = Uuid::new_v4();
        let scope = SessionScope::Conversation { channel_id: ch };
        let b_started = dummy_batch(ch, Uuid::new_v4(), scope.clone(), "started");
        let b_not_started = dummy_batch(ch, Uuid::new_v4(), scope.clone(), "not started");

        park.park(
            ParkedBatch::from_batch(&b_started, ParkReason::HardTimeout, true, Utc::now()).unwrap(),
        )
        .unwrap();
        park.park(
            ParkedBatch::from_batch(
                &b_not_started,
                ParkReason::RetriesExhausted,
                false,
                Utc::now(),
            )
            .unwrap(),
        )
        .unwrap();

        let candidates = park.replay_candidates(&scope);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].batch_id, b_not_started.batch_id);
    }

    #[test]
    fn test_park_101st_batch_scope_cap_single_atomic_write() {
        let dir = tempfile::tempdir().unwrap();
        let mut park = ParkFile::open(dir.path()).unwrap();
        let ch = Uuid::new_v4();
        let scope = SessionScope::Conversation { channel_id: ch };

        for i in 0..100 {
            let b_id = Uuid::new_v4();
            let batch = dummy_batch(ch, b_id, scope.clone(), &format!("msg {i}"));
            let parked =
                ParkedBatch::from_batch(&batch, ParkReason::RetriesExhausted, false, Utc::now())
                    .unwrap();
            park.park(parked).unwrap();
        }
        assert_eq!(park.batches().len(), 100);
        assert_eq!(park.replay_candidates(&scope).len(), 100);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let original_mode = std::fs::metadata(dir.path()).unwrap().permissions().mode();
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();

            let b_id_101 = Uuid::new_v4();
            let batch_101 = dummy_batch(ch, b_id_101, scope.clone(), "msg 101");
            let parked_101 = ParkedBatch::from_batch(
                &batch_101,
                ParkReason::RetriesExhausted,
                false,
                Utc::now(),
            )
            .unwrap();

            let res = park.park(parked_101);
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(original_mode))
                .unwrap();

            assert!(
                res.is_err(),
                "park of 101st batch must fail when write fails"
            );
            let on_disk = read_batches(park.path()).unwrap();
            assert_eq!(
                on_disk.len(),
                100,
                "disk must still hold exactly 100 batches"
            );
            assert!(!on_disk.iter().any(|b| b.batch_id == b_id_101));
            assert_eq!(park.batches().len(), 100);
            assert!(!park.contains(b_id_101));
        }

        // When write succeeds, 101st batch lands and oldest is demoted in ONE atomic update:
        let b_id_101 = Uuid::new_v4();
        let batch_101 = dummy_batch(ch, b_id_101, scope.clone(), "msg 101");
        let parked_101 =
            ParkedBatch::from_batch(&batch_101, ParkReason::RetriesExhausted, false, Utc::now())
                .unwrap();
        park.park(parked_101).unwrap();

        assert_eq!(park.batches().len(), 101);
        assert!(park.contains(b_id_101));
        assert_eq!(park.replay_candidates(&scope).len(), 100);
        assert!(park.batches()[0].needs_review);
    }
}
