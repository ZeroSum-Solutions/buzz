//! The harness-owned append-only ledger, `state/ledger.jsonl`.
//!
//! One JSON object per line, every line carrying `at`, `agent`, `kind` and —
//! where it applies — `batch_id`. The harness owns the file; observer frames
//! (T17) mirror it but are never the source of truth.
//!
//! Every string that reaches a record is provider- or relay-sourced, so each
//! one is capped at its DTO: see [`TurnStarted::new`] and friends, which are
//! the only way to build a record body.
//!
//! Design: `docs/plans/2026-09-06-harness-reliability-design.md`, "Ledger records".

use std::collections::HashSet;
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::error_class::truncate_chars;
use super::state_dir;

/// File name inside the state directory.
pub const LEDGER_FILE: &str = "ledger.jsonl";

/// Records older than this are dropped on start and every
/// [`TRUNCATE_INTERVAL_HOURS`].
pub const RETENTION_DAYS: i64 = 30;

/// How often the ledger is truncated while the harness runs.
pub const TRUNCATE_INTERVAL_HOURS: i64 = 6;

/// Hard cap on the ledger file. Appending past it drops the oldest records
/// first; the count of dropped records is returned so the operator is told.
pub const MAX_LEDGER_BYTES: u64 = 10 * 1024 * 1024;

/// Longest single line read back. A longer line is skipped, not buffered.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// Most event ids recorded on one `turn_started`. Matches the queue's own
/// per-batch event cap, so a full batch is recorded whole and nothing larger
/// can be.
pub const MAX_EVENT_IDS: usize = 50;

/// Longest id string stored (a Nostr event id is 64 hex characters).
pub const MAX_ID_CHARS: usize = 64;

/// Longest short text field (`scope`, `reason`, `class`).
pub const MAX_LABEL_CHARS: usize = 128;

/// Longest raw provider error stored beside its class, so a misclassification
/// can be diagnosed without keeping an unbounded string.
pub const MAX_RAW_CHARS: usize = 512;

/// One line of the ledger.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LedgerRecord {
    /// RFC 3339 UTC.
    pub at: DateTime<Utc>,
    /// Agent public key.
    pub agent: String,
    /// The record itself, flattened so `kind` sits beside `at` and `agent`.
    #[serde(flatten)]
    pub body: LedgerBody,
}

impl LedgerRecord {
    /// The batch this record is about, when it is about one.
    pub fn batch_id(&self) -> Option<Uuid> {
        self.body.batch_id()
    }

    /// The record kind, as written to the `kind` field.
    pub fn kind(&self) -> &'static str {
        self.body.kind()
    }
}

/// The record kinds. One struct per kind, tagged by `kind` on the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LedgerBody {
    TurnStarted(TurnStarted),
    TurnActivity(TurnActivity),
    TurnFinished(TurnFinished),
    BatchParked(BatchParked),
    BatchReplayed(BatchReplayed),
    BatchNeedsReview(BatchNeedsReview),
    BatchDiscarded(BatchDiscarded),
    AgentPaused(AgentPaused),
    AgentResumed(AgentResumed),
    BreakerOpened(BreakerOpened),
    BreakerClosed(BreakerClosed),
    RelayReconnected(RelayReconnected),
}

impl LedgerBody {
    /// The batch this record is about, when it is about one.
    pub fn batch_id(&self) -> Option<Uuid> {
        match self {
            Self::TurnStarted(r) => Some(r.batch_id),
            Self::TurnActivity(r) => Some(r.batch_id),
            Self::TurnFinished(r) => Some(r.batch_id),
            Self::BatchParked(r) => Some(r.batch_id),
            Self::BatchReplayed(r) => Some(r.batch_id),
            Self::BatchNeedsReview(r) => Some(r.batch_id),
            Self::BatchDiscarded(r) => Some(r.batch_id),
            Self::AgentPaused(_)
            | Self::AgentResumed(_)
            | Self::BreakerOpened(_)
            | Self::BreakerClosed(_)
            | Self::RelayReconnected(_) => None,
        }
    }

    /// The record kind, as written to the `kind` field.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::TurnStarted(_) => "turn_started",
            Self::TurnActivity(_) => "turn_activity",
            Self::TurnFinished(_) => "turn_finished",
            Self::BatchParked(_) => "batch_parked",
            Self::BatchReplayed(_) => "batch_replayed",
            Self::BatchNeedsReview(_) => "batch_needs_review",
            Self::BatchDiscarded(_) => "batch_discarded",
            Self::AgentPaused(_) => "agent_paused",
            Self::AgentResumed(_) => "agent_resumed",
            Self::BreakerOpened(_) => "breaker_opened",
            Self::BreakerClosed(_) => "breaker_closed",
            Self::RelayReconnected(_) => "relay_reconnected",
        }
    }
}

/// A turn was dispatched for a batch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TurnStarted {
    pub batch_id: Uuid,
    pub channel_id: Uuid,
    pub scope: String,
    pub event_ids: Vec<String>,
    pub attempt: u32,
}

impl TurnStarted {
    /// Build a capped record: at most [`MAX_EVENT_IDS`] ids, each at most
    /// [`MAX_ID_CHARS`] characters, and a scope label at most
    /// [`MAX_LABEL_CHARS`].
    pub fn new(
        batch_id: Uuid,
        channel_id: Uuid,
        scope: &str,
        event_ids: impl IntoIterator<Item = String>,
        attempt: u32,
    ) -> Self {
        Self {
            batch_id,
            channel_id,
            scope: truncate_chars(scope, MAX_LABEL_CHARS),
            event_ids: event_ids
                .into_iter()
                .take(MAX_EVENT_IDS)
                .map(|id| truncate_chars(&id, MAX_ID_CHARS))
                .collect(),
            attempt,
        }
    }
}

/// The first output or tool call seen for a batch. Written once per batch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TurnActivity {
    pub batch_id: Uuid,
    pub channel_id: Uuid,
}

/// How a turn ended.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TurnFinished {
    pub batch_id: Uuid,
    pub channel_id: Uuid,
    pub outcome: TurnOutcome,
}

/// The `outcome` field of a `turn_finished` record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum TurnOutcome {
    Ok,
    Error { class: String, raw: String },
    Timeout { kind: String, started: bool },
    Cancelled,
    Exited,
}

impl TurnOutcome {
    /// An `error` outcome with both fields capped.
    pub fn error(class: &str, raw: &str) -> Self {
        Self::Error {
            class: truncate_chars(class, MAX_LABEL_CHARS),
            raw: truncate_chars(raw, MAX_RAW_CHARS),
        }
    }

    /// A `timeout` outcome with the kind capped.
    pub fn timeout(kind: &str, started: bool) -> Self {
        Self::Timeout {
            kind: truncate_chars(kind, MAX_LABEL_CHARS),
            started,
        }
    }
}

/// A batch was moved to the park file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BatchParked {
    pub batch_id: Uuid,
    pub channel_id: Uuid,
    pub reason: String,
    pub started: bool,
    pub events: usize,
}

/// A parked batch was re-sent. Written **before** the prompt is sent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BatchReplayed {
    pub batch_id: Uuid,
    pub channel_id: Uuid,
    /// The batch id of the new turn carrying the replayed events.
    pub replay_of: Uuid,
}

/// A batch will not run again on its own.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BatchNeedsReview {
    pub batch_id: Uuid,
    pub channel_id: Uuid,
    pub reason: String,
}

/// An operator discarded a parked batch through a control frame.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BatchDiscarded {
    pub batch_id: Uuid,
    pub channel_id: Uuid,
    pub by: String,
}

/// The agent stopped running turns until `until`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentPaused {
    pub class: String,
    pub until: DateTime<Utc>,
    pub waiting: usize,
}

/// The agent started running turns again.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentResumed {}

/// A scope's breaker opened after consecutive provider failures.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BreakerOpened {
    pub scope: String,
    pub consecutive: u32,
}

/// A scope's breaker closed after a successful probe.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BreakerClosed {
    pub scope: String,
}

/// The relay connection came back.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RelayReconnected {
    pub after_secs: u64,
}

/// What a truncation pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TruncateReport {
    /// Records dropped because they were older than the retention window.
    pub aged_out: usize,
    /// Records dropped because the file was at its byte cap.
    pub over_cap: usize,
}

impl TruncateReport {
    /// Whether anything was dropped.
    pub fn is_empty(&self) -> bool {
        self.aged_out == 0 && self.over_cap == 0
    }
}

/// The append-only ledger file.
pub struct Ledger {
    path: PathBuf,
    agent: String,
    len_bytes: u64,
    next_truncate: DateTime<Utc>,
    write_failures: u64,
}

impl Ledger {
    /// Open (or create) the ledger in `dir` for `agent`, dropping records older
    /// than the retention window.
    pub fn open(dir: &Path, agent: &str, now: DateTime<Utc>) -> io::Result<Self> {
        state_dir::ensure_dir(dir)?;
        let path = dir.join(LEDGER_FILE);
        // Create it if absent so the mode is ours from the first byte.
        drop(state_dir::open_append(&path)?);
        sanitize_dangling_final_line(&path)?;
        let mut ledger = Self {
            len_bytes: std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
            path,
            agent: truncate_chars(agent, MAX_ID_CHARS),
            next_truncate: now + Duration::hours(TRUNCATE_INTERVAL_HOURS),
            write_failures: 0,
        };
        ledger.truncate(now)?;
        Ok(ledger)
    }

    /// Path of the ledger file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends that failed. Never reset: a non-zero value means the ledger is
    /// missing records and the operator has to be told.
    pub fn write_failures(&self) -> u64 {
        self.write_failures
    }

    /// Append one record and flush it to disk.
    ///
    /// The write is fsynced before returning, so a record that this call
    /// reports as written survives a crash. A failure is returned, never
    /// swallowed; the caller keeps whatever the record described.
    pub fn append(&mut self, at: DateTime<Utc>, body: LedgerBody) -> io::Result<()> {
        let record = LedgerRecord {
            at,
            agent: self.agent.clone(),
            body,
        };
        let mut line = serde_json::to_string(&record)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        if line.len() > MAX_LINE_BYTES {
            self.write_failures = self.write_failures.saturating_add(1);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "ledger record of kind {} serialised to {} bytes, over the {MAX_LINE_BYTES}-byte line cap",
                    record.kind(),
                    line.len()
                ),
            ));
        }
        if self.len_bytes + line.len() as u64 > MAX_LEDGER_BYTES {
            self.compact_for(line.len() as u64)?;
        }
        match self.append_line(line.as_bytes()) {
            Ok(()) => {
                self.len_bytes += line.len() as u64;
                Ok(())
            }
            Err(error) => {
                self.write_failures = self.write_failures.saturating_add(1);
                Err(error)
            }
        }
    }

    fn append_line(&self, line: &[u8]) -> io::Result<()> {
        let mut file = state_dir::open_append(&self.path)?;
        file.write_all(line)?;
        file.flush()?;
        file.sync_all()
    }

    /// Read every record back, bounded by [`MAX_LEDGER_BYTES`] of input and
    /// [`MAX_LINE_BYTES`] per line. Malformed lines are counted and skipped,
    /// never propagated as a parse failure for the whole file.
    pub fn read_all(&self) -> io::Result<Vec<LedgerRecord>> {
        read_records(&self.path)
    }

    /// Batch ids with a `batch_replayed` record and no later `turn_finished`.
    ///
    /// On start these are crashes mid-replay: the prompt may or may not have
    /// reached the agent, so the batch moves to `needs_review` rather than
    /// replaying a second time.
    pub fn replays_without_finish(&self) -> io::Result<Vec<Uuid>> {
        let records = self.read_all()?;
        let mut replayed: Vec<Uuid> = Vec::new();
        let mut finished: HashSet<Uuid> = HashSet::new();
        for record in &records {
            match &record.body {
                LedgerBody::BatchReplayed(r) => replayed.push(r.batch_id),
                LedgerBody::TurnFinished(r) => {
                    finished.insert(r.batch_id);
                }
                _ => {}
            }
        }
        let mut seen: HashSet<Uuid> = HashSet::new();
        Ok(replayed
            .into_iter()
            .filter(|id| !finished.contains(id) && seen.insert(*id))
            .collect())
    }

    /// Truncate if the interval has elapsed. Returns what was dropped.
    pub fn maybe_truncate(&mut self, now: DateTime<Utc>) -> io::Result<TruncateReport> {
        if now < self.next_truncate {
            return Ok(TruncateReport::default());
        }
        self.truncate(now)
    }

    /// Drop records older than the retention window, then the oldest records
    /// still over the byte cap.
    pub fn truncate(&mut self, now: DateTime<Utc>) -> io::Result<TruncateReport> {
        self.next_truncate = now + Duration::hours(TRUNCATE_INTERVAL_HOURS);
        let cutoff = now - Duration::days(RETENTION_DAYS);
        let records = self.read_all()?;
        let total = records.len();
        let kept: Vec<LedgerRecord> = records.into_iter().filter(|r| r.at >= cutoff).collect();
        let aged_out = total - kept.len();
        let (kept, over_cap) = fit_to_cap(kept)?;
        if aged_out == 0 && over_cap == 0 {
            return Ok(TruncateReport::default());
        }
        self.rewrite(&kept)?;
        Ok(TruncateReport { aged_out, over_cap })
    }

    /// Make room for `incoming` bytes by dropping the oldest records.
    fn compact_for(&mut self, incoming: u64) -> io::Result<()> {
        let records = self.read_all()?;
        let budget = MAX_LEDGER_BYTES.saturating_sub(incoming);
        let (kept, dropped) = fit_to_budget(records, budget)?;
        if dropped > 0 {
            tracing::warn!(
                dropped,
                cap = MAX_LEDGER_BYTES,
                "ledger reached its byte cap — dropped the oldest records"
            );
        }
        self.rewrite(&kept)
    }

    fn rewrite(&mut self, records: &[LedgerRecord]) -> io::Result<()> {
        let mut buffer = Vec::new();
        for record in records {
            serde_json::to_writer(&mut buffer, record)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            buffer.push(b'\n');
        }
        state_dir::write_atomic(&self.path, &buffer)?;
        self.len_bytes = buffer.len() as u64;
        Ok(())
    }
}

fn serialized_len(record: &LedgerRecord) -> io::Result<u64> {
    serde_json::to_string(record)
        .map(|s| s.len() as u64 + 1)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn fit_to_cap(records: Vec<LedgerRecord>) -> io::Result<(Vec<LedgerRecord>, usize)> {
    fit_to_budget(records, MAX_LEDGER_BYTES)
}

/// Keep the newest records that fit in `budget` bytes, dropping oldest first.
fn fit_to_budget(
    records: Vec<LedgerRecord>,
    budget: u64,
) -> io::Result<(Vec<LedgerRecord>, usize)> {
    let mut total: u64 = 0;
    for record in &records {
        total += serialized_len(record)?;
    }
    if total <= budget {
        return Ok((records, 0));
    }
    let mut dropped = 0usize;
    let mut remaining = records;
    while total > budget && !remaining.is_empty() {
        let head = remaining.remove(0);
        total = total.saturating_sub(serialized_len(&head)?);
        dropped += 1;
    }
    Ok((remaining, dropped))
}

/// Detect and truncate a dangling final line without a trailing newline, so
/// future appends are not fused with corrupted partial lines.
fn sanitize_dangling_final_line(path: &Path) -> io::Result<()> {
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last_byte = [0u8; 1];
    file.read_exact(&mut last_byte)?;
    if last_byte[0] == b'\n' {
        return Ok(());
    }

    let mut pos = len - 1;
    let mut found_nl = false;
    let mut buf = [0u8; 4096];
    while pos > 0 {
        let chunk_size = (pos as usize).min(buf.len());
        let chunk_start = pos - chunk_size as u64;
        file.seek(SeekFrom::Start(chunk_start))?;
        file.read_exact(&mut buf[..chunk_size])?;
        if let Some(offset) = buf[..chunk_size].iter().rposition(|&b| b == b'\n') {
            file.set_len(chunk_start + offset as u64 + 1)?;
            found_nl = true;
            break;
        }
        pos = chunk_start;
    }
    if !found_nl {
        file.set_len(0)?;
    }
    file.sync_all()?;
    tracing::warn!(
        path = %path.display(),
        "truncated dangling partial line with no trailing newline in ledger file"
    );
    Ok(())
}

/// Read a JSONL ledger file with a hard byte cap on the input and a hard cap
/// per line.
fn read_records(path: &Path) -> io::Result<Vec<LedgerRecord>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut reader = io::BufReader::new(file.take(MAX_LEDGER_BYTES));
    let mut records = Vec::new();
    let mut skipped = 0usize;
    let mut line = Vec::new();
    loop {
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
        match serde_json::from_str::<LedgerRecord>(text) {
            Ok(record) => records.push(record),
            Err(_) => skipped += 1,
        }
    }
    if skipped > 0 {
        tracing::warn!(
            skipped,
            path = %path.display(),
            "skipped unreadable ledger lines"
        );
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ledger_append_and_read_all() {
        let dir = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let mut ledger = Ledger::open(dir.path(), "agent-pubkey", now).unwrap();
        let batch_id = Uuid::new_v4();
        let ch = Uuid::new_v4();

        ledger
            .append(
                now,
                LedgerBody::BatchParked(BatchParked {
                    batch_id,
                    channel_id: ch,
                    reason: "retries_exhausted".to_string(),
                    started: false,
                    events: 1,
                }),
            )
            .unwrap();

        let records = ledger.read_all().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].batch_id(), Some(batch_id));
        assert_eq!(records[0].kind(), "batch_parked");
    }

    #[test]
    fn test_replays_without_finish_detects_unmatched_replay() {
        let dir = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let mut ledger = Ledger::open(dir.path(), "agent-pubkey", now).unwrap();
        let batch_id1 = Uuid::new_v4();
        let batch_id2 = Uuid::new_v4();
        let ch = Uuid::new_v4();

        // batch 1: replayed with matching turn_finished
        ledger
            .append(
                now,
                LedgerBody::BatchReplayed(BatchReplayed {
                    batch_id: batch_id1,
                    channel_id: ch,
                    replay_of: Uuid::new_v4(),
                }),
            )
            .unwrap();
        ledger
            .append(
                now,
                LedgerBody::TurnFinished(TurnFinished {
                    batch_id: batch_id1,
                    channel_id: ch,
                    outcome: TurnOutcome::Ok,
                }),
            )
            .unwrap();

        // batch 2: replayed without turn_finished
        ledger
            .append(
                now,
                LedgerBody::BatchReplayed(BatchReplayed {
                    batch_id: batch_id2,
                    channel_id: ch,
                    replay_of: Uuid::new_v4(),
                }),
            )
            .unwrap();

        let crashed = ledger.replays_without_finish().unwrap();
        assert_eq!(crashed, vec![batch_id2]);
    }

    #[test]
    fn test_ledger_open_quarantines_dangling_final_line_without_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LEDGER_FILE);
        let now = Utc::now();

        // Pre-seed a ledger file with valid lines, then a truncated line with no trailing newline:
        let valid_record = LedgerRecord {
            at: now,
            agent: "test-agent".to_string(),
            body: LedgerBody::AgentResumed(AgentResumed {}),
        };
        let valid_line = format!("{}\n", serde_json::to_string(&valid_record).unwrap());
        let truncated_line =
            "{\"at\":\"2026-09-06T00:01:00Z\",\"agent\":\"test-agent\",\"kind\":\"batch_parked\",\"batch_id\":\"";
        std::fs::write(&path, format!("{valid_line}{truncated_line}")).unwrap();

        let mut ledger = Ledger::open(dir.path(), "test-agent", now).unwrap();

        // Append a new record
        let new_batch_id = Uuid::new_v4();
        ledger
            .append(
                now,
                LedgerBody::BatchParked(BatchParked {
                    batch_id: new_batch_id,
                    channel_id: Uuid::new_v4(),
                    reason: "retry".to_string(),
                    started: false,
                    events: 1,
                }),
            )
            .unwrap();

        let records = ledger.read_all().unwrap();
        assert!(
            records.iter().any(
                |r| matches!(&r.body, LedgerBody::BatchParked(bp) if bp.batch_id == new_batch_id)
            ),
            "new record must be recovered cleanly and not fused with dangling line"
        );
        assert_eq!(
            records.len(),
            2,
            "must recover 1 valid pre-seeded record + 1 new record"
        );
    }
}
