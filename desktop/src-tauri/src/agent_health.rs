//! Desktop health store: schema, insert-or-ignore, and retention.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use buzz_acp_pkg::reliability::ledger::{
    read_ledger_file_for_agent, LedgerBody, LedgerRecord, TurnOutcome, LEDGER_FILE,
};
use rusqlite::{params, params_from_iter, Connection, Row};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::managed_agents::storage::{load_managed_agents, managed_agent_state_dir};
use crate::managed_agents::ManagedAgentRecord;

#[allow(dead_code)]
pub(crate) const SCHEMA_VERSION: i64 = 1;
#[allow(dead_code)]
pub(crate) const RETENTION_SECS: i64 = 30 * 24 * 60 * 60;
pub(crate) const DEFAULT_MAX_HEALTH_EVENTS_ROWS: i64 = 1_000;
pub(crate) const DEFAULT_MAX_HEALTH_DB_BYTES: i64 = 10 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HealthEvent {
    pub agent: String,
    pub at: i64,
    pub kind: String,
    pub event_key: String,
    pub batch_id: Option<String>,
    pub channel_id: Option<String>,
    pub class: Option<String>,
    pub payload: Option<String>,
}

impl HealthEvent {
    #[allow(dead_code)]
    pub(crate) fn compute_event_key(at_rfc3339: &str, kind: &str, target: Option<&str>) -> String {
        compute_event_key(at_rfc3339, kind, target)
    }
}

/// Canonicalize an RFC3339 timestamp to a form two equivalent spellings of
/// the same instant always agree on (e.g. `+00:00` vs `Z`), while keeping
/// millisecond precision. Two *distinct* instants within the same second
/// must still produce different keys — collapsing to whole-second precision
/// (as `SecondsFormat::Secs` does) makes two different sub-second events
/// collide on `compute_event_key` and one silently disappears via the
/// `(agent, event_key)` insert-or-ignore primary key.
pub(crate) fn canonicalize_timestamp(at_rfc3339: &str) -> String {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(at_rfc3339) {
        dt.with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    } else {
        at_rfc3339.to_string()
    }
}

pub(crate) fn compute_event_key(at_rfc3339: &str, kind: &str, target: Option<&str>) -> String {
    let canon_at = canonicalize_timestamp(at_rfc3339);
    format!("{canon_at}|{kind}|{}", target.unwrap_or(""))
}

#[allow(dead_code)]
/// Resolve the agent-health database path, scoped to the active community
/// (relay + owner identity) exactly like `managed_agents::retention`'s
/// persona-event store: one database file per `(relay_url, owner_pubkey)`,
/// not one shared file. Without this, health rows and the alert-suppression
/// table carry no community identity at all, so an agent pubkey reused
/// across two communities would blend their health data, and old-community
/// work queued right at a workspace switch could still be written after the
/// switch completes — see `AGENTS.md` "Community Switching".
pub(crate) fn db_path(
    app: &AppHandle,
    relay_url: &str,
    owner_pubkey: &str,
) -> Result<PathBuf, String> {
    let base_dir = crate::managed_agents::storage::managed_agents_base_dir(app)?;
    let path = crate::managed_agents::retention::scoped_db_path(
        &base_dir,
        "agent-health",
        relay_url,
        owner_pubkey,
    );
    let parent = path
        .parent()
        .ok_or_else(|| "agent-health db path has no parent".to_string())?;
    std::fs::create_dir_all(parent).map_err(|e| format!("create agent-health data dir: {e}"))?;
    Ok(path)
}

/// Resolve the `(relay_url, owner_pubkey)` pair `db_path` scopes the
/// agent-health database to, from the app's current `AppState`. Kept
/// separate from `db_path` itself so callers can resolve this BEFORE
/// entering a `blocking::run` closure — `State<'_, AppState>` is not
/// `'static` and cannot be moved into a spawned blocking task, but the two
/// owned `String`s this returns can.
pub(crate) fn resolve_health_db_scope(
    state: &crate::app_state::AppState,
) -> Result<(String, String), String> {
    let relay_url = crate::relay::relay_ws_url_with_override(state);
    let owner_pubkey = state.signing_keys()?.public_key().to_hex();
    Ok((relay_url, owner_pubkey))
}

#[allow(dead_code)]
pub(crate) fn open_db(path: &Path) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|e| format!("open agent-health db: {e}"))?;
    conn.pragma_update(None, "busy_timeout", 5_000)
        .map_err(|e| format!("configure agent-health db: {e}"))?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| format!("configure agent-health WAL: {e}"))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_meta(version INTEGER NOT NULL);
        INSERT INTO schema_meta(version) SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM schema_meta);
        CREATE TABLE IF NOT EXISTS health_events(
            agent TEXT NOT NULL,
            at INTEGER NOT NULL,
            kind TEXT NOT NULL,
            event_key TEXT NOT NULL,
            batch_id TEXT,
            channel_id TEXT,
            class TEXT,
            payload TEXT,
            PRIMARY KEY(agent, event_key)
        );
        CREATE INDEX IF NOT EXISTS health_events_agent_at ON health_events(agent, at);
        CREATE INDEX IF NOT EXISTS health_events_kind_at ON health_events(kind, at);
        CREATE TABLE IF NOT EXISTS alert_state(
            agent TEXT NOT NULL,
            rule TEXT NOT NULL,
            last_fired_at INTEGER NOT NULL,
            PRIMARY KEY(agent, rule)
        );",
    )
    .map_err(|e| format!("initialize agent-health db: {e}"))?;
    let version: i64 = conn
        .query_row("SELECT version FROM schema_meta LIMIT 1", [], |row| {
            row.get(0)
        })
        .map_err(|e| format!("read agent-health schema: {e}"))?;
    if version != SCHEMA_VERSION {
        return Err(format!("unsupported agent-health schema version {version}"));
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    prune(&conn, now)?;
    Ok(conn)
}

#[allow(dead_code)]
pub(crate) fn insert_event(conn: &Connection, event: &HealthEvent) -> Result<bool, String> {
    let changed = conn
        .execute(
            "INSERT OR IGNORE INTO health_events
                (agent, at, kind, event_key, batch_id, channel_id, class, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                event.agent,
                event.at,
                event.kind,
                event.event_key,
                event.batch_id,
                event.channel_id,
                event.class,
                event.payload,
            ],
        )
        .map_err(|e| format!("insert agent-health event: {e}"))?;
    Ok(changed > 0)
}

#[allow(dead_code)]
pub(crate) fn prune(conn: &Connection, now: i64) -> Result<usize, String> {
    let cutoff = now - RETENTION_SECS;
    // Both retention deletes run in one transaction: a mid-way failure must
    // not leave `health_events` pruned while `alert_state` keeps its stale
    // rows (or vice versa) — the two tables' retention windows are meant to
    // stay in lockstep.
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("begin prune transaction: {e}"))?;
    tx.execute(
        "DELETE FROM alert_state WHERE last_fired_at < ?1",
        params![cutoff],
    )
    .map_err(|e| format!("prune alert_state: {e}"))?;
    let mut total_pruned = tx
        .execute("DELETE FROM health_events WHERE at < ?1", params![cutoff])
        .map_err(|e| format!("prune agent-health events: {e}"))?;
    tx.commit()
        .map_err(|e| format!("commit prune transaction: {e}"))?;

    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM health_events", [], |r| r.get(0))
        .map_err(|e| format!("count health events: {e}"))?;
    if count > DEFAULT_MAX_HEALTH_EVENTS_ROWS {
        let excess = count - DEFAULT_MAX_HEALTH_EVENTS_ROWS;
        let deleted = conn
            .execute(
                "DELETE FROM health_events WHERE (agent, event_key) IN (
                    SELECT agent, event_key FROM health_events ORDER BY at ASC LIMIT ?1
                )",
                params![excess],
            )
            .map_err(|e| format!("prune excess rows: {e}"))?;
        total_pruned += deleted;
    }

    let db_bytes = current_db_bytes(conn)?;
    if db_bytes > DEFAULT_MAX_HEALTH_DB_BYTES {
        let mut db_bytes = db_bytes;
        while db_bytes > DEFAULT_MAX_HEALTH_DB_BYTES {
            let deleted = conn
                .execute(
                    "DELETE FROM health_events WHERE (agent, event_key) IN (
                        SELECT agent, event_key FROM health_events ORDER BY at ASC LIMIT 100
                    )",
                    [],
                )
                .map_err(|e| format!("prune excess bytes: {e}"))?;
            if deleted == 0 {
                break;
            }
            total_pruned += deleted;
            // Both propagated: a failed reclaim must be visible as a prune
            // failure, not silently leave the database over budget while
            // reporting success.
            conn.execute("VACUUM", [])
                .map_err(|e| format!("vacuum agent-health db: {e}"))?;
            let new_bytes = current_db_bytes(conn)?;
            if new_bytes >= db_bytes {
                break;
            }
            db_bytes = new_bytes;
        }
    }

    Ok(total_pruned)
}

/// Current on-disk size of the agent-health database, WAL included.
///
/// `PRAGMA page_count * page_size` alone only reports the main database
/// file; under WAL journaling (this connection's mode — see `open_db`),
/// recently committed writes can sit in the `-wal` file, uncounted, well
/// past the configured byte budget. Checkpointing first folds the WAL back
/// into the main file (and truncates it) so the byte count this function
/// returns reflects what is actually on disk.
fn current_db_bytes(conn: &Connection) -> Result<i64, String> {
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .map_err(|e| format!("checkpoint agent-health WAL: {e}"))?;
    let page_count: i64 = conn
        .query_row("PRAGMA page_count", [], |r| r.get(0))
        .map_err(|e| format!("read agent-health page_count: {e}"))?;
    let page_size: i64 = conn
        .query_row("PRAGMA page_size", [], |r| r.get(0))
        .map_err(|e| format!("read agent-health page_size: {e}"))?;
    Ok(page_count * page_size)
}

/// Map one ledger record to a health event, or `None` when the record
/// carries no health signal (`turn_activity`).
///
/// `turn_finished` with an error outcome becomes kind `turn_failed` with
/// `raw` dropped; every other outcome (and `turn_started`) keeps its ledger
/// kind so `sync_ledger` can feed the turns-24h/7d counters.
fn ledger_record_to_health_event(agent: &str, record: &LedgerRecord) -> Option<HealthEvent> {
    let at_rfc3339 = record
        .at
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let batch_id = record.batch_id().map(|id| id.to_string());
    let channel_id = record.channel_id().map(|id| id.to_string());

    let (kind, class, payload): (&str, Option<String>, serde_json::Value) = match &record.body {
        LedgerBody::TurnStarted(r) => (
            "turn_started",
            None,
            serde_json::json!({"scope": r.scope, "attempt": r.attempt}),
        ),
        LedgerBody::TurnActivity(_) => return None,
        LedgerBody::TurnFinished(r) => match &r.outcome {
            TurnOutcome::Ok => ("turn_finished", None, serde_json::json!({"result": "ok"})),
            TurnOutcome::Error { class, .. } => (
                "turn_failed",
                Some(class.clone()),
                serde_json::json!({"result": "error", "class": class}),
            ),
            TurnOutcome::Timeout { kind, started } => (
                "turn_finished",
                Some(kind.clone()),
                serde_json::json!({"result": "timeout", "kind": kind, "started": started}),
            ),
            TurnOutcome::Cancelled => (
                "turn_finished",
                None,
                serde_json::json!({"result": "cancelled"}),
            ),
            TurnOutcome::Exited => (
                "turn_finished",
                None,
                serde_json::json!({"result": "exited"}),
            ),
        },
        LedgerBody::BatchParked(r) => (
            "batch_parked",
            None,
            serde_json::json!({"reason": r.reason, "started": r.started, "events": r.events}),
        ),
        LedgerBody::BatchReplayed(r) => (
            "batch_replayed",
            None,
            serde_json::json!({"replayOf": r.replay_of}),
        ),
        LedgerBody::BatchNeedsReview(r) => (
            "batch_needs_review",
            None,
            serde_json::json!({"reason": r.reason}),
        ),
        LedgerBody::BatchDiscarded(r) => ("batch_discarded", None, serde_json::json!({"by": r.by})),
        LedgerBody::AgentPaused(r) => (
            "agent_paused",
            Some(r.class.clone()),
            serde_json::json!({"class": r.class, "until": r.until, "waiting": r.waiting}),
        ),
        LedgerBody::AgentResumed(_) => ("agent_resumed", None, serde_json::json!({})),
        LedgerBody::BreakerOpened(r) => (
            "breaker_opened",
            None,
            serde_json::json!({"scope": r.scope, "consecutive": r.consecutive}),
        ),
        LedgerBody::BreakerClosed(r) => (
            "breaker_closed",
            None,
            serde_json::json!({"scope": r.scope}),
        ),
        LedgerBody::RelayReconnected(r) => (
            "relay_reconnected",
            None,
            serde_json::json!({"afterSecs": r.after_secs}),
        ),
    };

    let target = batch_id.clone().or_else(|| match &record.body {
        LedgerBody::BreakerOpened(r) => Some(r.scope.clone()),
        LedgerBody::BreakerClosed(r) => Some(r.scope.clone()),
        _ => None,
    });

    Some(HealthEvent {
        agent: agent.to_string(),
        at: record.at.timestamp(),
        kind: kind.to_string(),
        event_key: compute_event_key(&at_rfc3339, kind, target.as_deref()),
        batch_id,
        channel_id,
        class,
        payload: Some(payload.to_string()),
    })
}

/// Read `ledger_path` (through the read-only reader, never `Ledger::open`,
/// which would rewrite the harness's own file) and insert every record not
/// already stored. Safe to call repeatedly: duplicates are ignored by the
/// `(agent, event_key)` primary key.
#[allow(dead_code)]
pub(crate) fn sync_ledger(
    conn: &Connection,
    agent: &str,
    ledger_path: &Path,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<usize, String> {
    // Validated, not the raw reader: a ledger record embedding an `agent`
    // different from this state directory's owner, or a future-dated
    // record, must never be blended into this agent's counters.
    let records = read_ledger_file_for_agent(ledger_path, agent, now)
        .map_err(|e| format!("read agent ledger: {e}"))?;
    let mut inserted = 0usize;
    for record in &records {
        if let Some(event) = ledger_record_to_health_event(agent, record) {
            if insert_event(conn, &event)? {
                inserted += 1;
            }
        }
    }
    Ok(inserted)
}

#[derive(Default, Clone)]
pub struct AgentHealthStore {
    pub(crate) write_lock: Arc<Mutex<()>>,
    pub(crate) in_flight_syncs: Arc<Mutex<HashSet<String>>>,
    /// Keys that requested a sync while one was already in flight for them.
    /// Consumed (and, if present, causes one more run) by
    /// `finish_sync_or_rerun` — see `sync_for_agent`.
    pub(crate) dirty_syncs: Arc<Mutex<HashSet<String>>>,
}

pub(crate) struct InFlightGuard {
    pub(crate) in_flight: Arc<Mutex<HashSet<String>>>,
    pub(crate) dirty: Arc<Mutex<HashSet<String>>>,
    pub(crate) key: String,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = self.in_flight.lock() {
            set.remove(&self.key);
        }
        if let Ok(mut dirty) = self.dirty.lock() {
            dirty.remove(&self.key);
        }
    }
}

/// Whether a caller requesting a sync for `key` should start one now, or the
/// key is already in flight.
pub(crate) enum SyncClaim {
    Start,
    AlreadyRunning,
}

/// Claim the in-flight slot for `key`, or — if another run already holds it
/// — mark `key` dirty so that run repeats once more after it finishes,
/// instead of silently dropping this request.
pub(crate) fn claim_sync_slot(
    in_flight: &Mutex<HashSet<String>>,
    dirty: &Mutex<HashSet<String>>,
    key: &str,
) -> SyncClaim {
    let mut set = match in_flight.lock() {
        Ok(s) => s,
        Err(poisoned) => poisoned.into_inner(),
    };
    if set.insert(key.to_string()) {
        SyncClaim::Start
    } else {
        if let Ok(mut d) = dirty.lock() {
            d.insert(key.to_string());
        }
        SyncClaim::AlreadyRunning
    }
}

/// After one sync run for `key` finishes, whether to run again immediately
/// (`true`) — because a request arrived while this run was already in
/// flight, per `claim_sync_slot` — or release the in-flight slot (`false`).
///
/// Checking (and clearing) `dirty` BEFORE releasing `in_flight` closes the
/// gap a "release first, then check dirty" order would leave open: a
/// request landing in between those two steps would see the key still
/// in-flight and mark it dirty, and that mark must still be observed by
/// this same run rather than left stranded after the slot is released.
pub(crate) fn finish_sync_or_rerun(
    in_flight: &Mutex<HashSet<String>>,
    dirty: &Mutex<HashSet<String>>,
    key: &str,
) -> bool {
    let rerun = {
        let mut d = match dirty.lock() {
            Ok(d) => d,
            Err(poisoned) => poisoned.into_inner(),
        };
        d.remove(key)
    };
    if !rerun {
        let mut set = match in_flight.lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };
        set.remove(key);
    }
    rerun
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentHealthCounters {
    pub agent: String,
    pub turns: i64,
    pub failed: i64,
    pub parked: i64,
    pub needs_review: i64,
    pub reconnects: i64,
    pub last_failure_class: Option<String>,
    pub last_failure_at: Option<i64>,
    #[serde(alias = "pausedUntil")]
    pub latest_paused_until: Option<String>,
    #[serde(alias = "latestBreakerOpen")]
    pub breaker_open: bool,
}

mod blocking {
    pub(super) struct OnBlockingThread(());

    pub(super) async fn run<T, F>(task: F) -> Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(OnBlockingThread) -> Result<T, String> + Send + 'static,
    {
        tauri::async_runtime::spawn_blocking(move || task(OnBlockingThread(())))
            .await
            .map_err(|error| format!("agent-health db task failed: {error}"))?
    }
}

/// Per-agent running totals built by a single ascending-by-`at` pass over
/// `health_events` (see `query_agent_health_summary`). Ascending order means
/// "the value seen last for this agent" is always the latest one within the
/// window, so `last_failure_*`, `latest_paused_until` and `breaker_open` are
/// simple overwrite-on-each-match assignments rather than a second query.
/// Sentinel scope used when a `breaker_opened` payload cannot be parsed or
/// carries no `scope` field. No real payload can ever produce this string
/// (a legitimate `scope` comes from `SessionScope`'s own formatting), so it
/// never collides with a real breaker scope, and its presence in
/// `open_scopes` still makes `breaker_open` read `true`.
const CORRUPTED_BREAKER_SCOPE: &str = "__corrupted_breaker_payload__";

/// Parse a breaker event's `scope` out of its JSON payload. `Err` covers
/// both unparseable JSON and JSON missing (or non-string) `scope` — both are
/// "this payload is corrupt", not "the scope is empty".
fn parse_breaker_scope(payload: Option<&str>) -> Result<String, String> {
    let raw = payload.ok_or_else(|| "missing breaker payload".to_string())?;
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("invalid breaker payload JSON: {e}"))?;
    value
        .get("scope")
        .and_then(|s| s.as_str())
        .map(str::to_string)
        .ok_or_else(|| "breaker payload missing string \"scope\" field".to_string())
}

#[derive(Default)]
struct SummaryAccumulator {
    turns: i64,
    failed: i64,
    reconnects: i64,
    last_failure_class: Option<String>,
    last_failure_at: Option<i64>,
    latest_paused_until: Option<String>,
    open_scopes: std::collections::HashSet<String>,
    active_needs_review: std::collections::HashSet<String>,
    active_parked: std::collections::HashSet<String>,
}

fn row_to_health_event(row: &Row<'_>) -> rusqlite::Result<HealthEvent> {
    Ok(HealthEvent {
        agent: row.get(0)?,
        at: row.get(1)?,
        kind: row.get(2)?,
        event_key: row.get(3)?,
        batch_id: row.get(4)?,
        channel_id: row.get(5)?,
        class: row.get(6)?,
        payload: row.get(7)?,
    })
}

/// Longest a single `kind` filter string may be, and the most filter values
/// one query may carry. Real kind names (`turn_failed`, `batch_parked`, …)
/// are short, closed identifiers; the Tauri command boundary hands `kinds`
/// straight to `query_agent_health_events`, which otherwise emits one SQL
/// placeholder and bound parameter per caller-supplied entry with no count
/// or length limit — an easy excessive-allocation / SQLite variable-limit
/// denial of service from an untrusted renderer call.
pub(crate) const MAX_KIND_FILTER_COUNT: usize = 32;
pub(crate) const MAX_KIND_FILTER_CHARS: usize = 64;

fn validate_kinds(kinds: Option<&[String]>) -> Result<(), String> {
    let Some(ks) = kinds else {
        return Ok(());
    };
    if ks.len() > MAX_KIND_FILTER_COUNT {
        return Err(format!(
            "kinds filter must have at most {MAX_KIND_FILTER_COUNT} entries, got {}",
            ks.len()
        ));
    }
    for k in ks {
        if k.chars().count() > MAX_KIND_FILTER_CHARS {
            return Err(format!(
                "kind filter entry exceeds {MAX_KIND_FILTER_CHARS} characters"
            ));
        }
    }
    Ok(())
}

fn validate_since_hours(since_hours: Option<i64>, now: i64) -> Result<Option<i64>, String> {
    const MAX_HOURS: i64 = 30 * 24;
    match since_hours {
        Some(hours) => {
            if !(0..=MAX_HOURS).contains(&hours) {
                return Err(format!(
                    "since_hours must be between 0 and {MAX_HOURS}, got {hours}"
                ));
            }
            let cutoff = hours
                .checked_mul(3600)
                .and_then(|secs| now.checked_sub(secs))
                .ok_or_else(|| format!("cutoff timestamp overflow for since_hours: {hours}"))?;
            Ok(Some(cutoff))
        }
        None => Ok(None),
    }
}

/// Group `health_events` by agent (optionally windowed to the last
/// `since_hours`) into the counters the Health tab and `buzz agents health`
/// both need: `turns`/`failed`/`reconnects` are windowed activity counts;
/// `parked`/`needs_review` are current outstanding-batch counts (see the
/// keyed reconciliation in step 3 below, not a windowed event count — a
/// batch parked eight days ago with no resolution is still outstanding, and
/// one resolved a minute after entering the window must stop counting). Also
/// the most recent failure's class and timestamp, the most recent
/// `agent_paused.until` (read out of the event's JSON payload — there is no
/// dedicated column for it), and whether the latest breaker event for the
/// agent was an open with no later close.
pub(crate) fn query_agent_health_summary(
    conn: &Connection,
    since_hours: Option<i64>,
    now: i64,
) -> Result<Vec<AgentHealthCounters>, String> {
    let cutoff = validate_since_hours(since_hours, now)?;
    let mut by_agent: BTreeMap<String, SummaryAccumulator> = BTreeMap::new();

    // 1. Aggregated activity counters within window
    let counters_sql = if cutoff.is_some() {
        "SELECT agent,
                SUM(CASE WHEN kind IN ('turn_finished', 'turn_failed') THEN 1 ELSE 0 END) AS turns,
                SUM(CASE WHEN kind = 'turn_failed' THEN 1 ELSE 0 END) AS failed,
                SUM(CASE WHEN kind = 'relay_reconnected' THEN 1 ELSE 0 END) AS reconnects
         FROM health_events
         WHERE at >= ?1
         GROUP BY agent
         ORDER BY agent ASC"
    } else {
        "SELECT agent,
                SUM(CASE WHEN kind IN ('turn_finished', 'turn_failed') THEN 1 ELSE 0 END) AS turns,
                SUM(CASE WHEN kind = 'turn_failed' THEN 1 ELSE 0 END) AS failed,
                SUM(CASE WHEN kind = 'relay_reconnected' THEN 1 ELSE 0 END) AS reconnects
         FROM health_events
         GROUP BY agent
         ORDER BY agent ASC"
    };

    let mut stmt = conn
        .prepare(counters_sql)
        .map_err(|e| format!("prepare agent-health counters query: {e}"))?;

    let map_row = |row: &Row<'_>| -> rusqlite::Result<(String, i64, i64, i64)> {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    };

    let counter_rows = if let Some(cutoff_val) = cutoff {
        stmt.query_map(params![cutoff_val], map_row)
    } else {
        stmt.query_map([], map_row)
    }
    .map_err(|e| format!("query agent-health counters: {e}"))?
    .collect::<Result<Vec<_>, _>>()
    .map_err(|e| format!("read agent-health counters: {e}"))?;

    for (agent, turns, failed, reconnects) in counter_rows {
        let entry = by_agent.entry(agent).or_default();
        entry.turns = turns;
        entry.failed = failed;
        entry.reconnects = reconnects;
    }

    // 2. Last failure within window
    let mut fail_stmt = conn
        .prepare(LATEST_FAILURES_SQL)
        .map_err(|e| format!("prepare agent-health last failure query: {e}"))?;

    let map_fail = |row: &Row<'_>| -> rusqlite::Result<(String, i64, Option<String>)> {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    };

    let fail_rows = fail_stmt
        .query_map(params![cutoff], map_fail)
        .map_err(|e| format!("query agent-health last failure: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read agent-health last failure: {e}"))?;

    for (agent, at, class) in fail_rows {
        let entry = by_agent.entry(agent).or_default();
        if entry.last_failure_at.is_none() {
            entry.last_failure_at = Some(at);
            entry.last_failure_class = class;
        }
    }

    // 3. Unwindowed current-state scan across full 30-day retention
    let state_cutoff = now - RETENTION_SECS;
    let mut state_stmt = conn
        .prepare(
            "SELECT agent, at, kind, batch_id, payload FROM health_events
             WHERE kind IN ('agent_paused', 'agent_resumed', 'breaker_opened', 'breaker_closed', 'batch_parked', 'batch_needs_review', 'batch_replayed', 'batch_discarded')
               AND at >= ?1
             ORDER BY agent ASC, at ASC",
        )
        .map_err(|e| format!("prepare agent-health state query: {e}"))?;

    let state_rows = state_stmt
        .query_map(params![state_cutoff], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })
        .map_err(|e| format!("query agent-health state: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read agent-health state: {e}"))?;

    for (agent, at, kind, batch_id, payload) in state_rows {
        let entry = by_agent.entry(agent).or_default();
        match kind.as_str() {
            // `parked`/`needs_review` are current outstanding state, tracked
            // the same way `agent_paused`/`breaker_opened` are just below:
            // unconditionally, bounded only by the 30-day `state_cutoff`
            // this whole scan already applies — never by the caller's
            // requested `since_hours` window. A batch parked or flagged
            // for review outside that window but never resolved is still
            // outstanding and must not disappear from the count.
            "batch_parked" => {
                let bid = batch_id.unwrap_or_else(|| format!("anon_{at}"));
                entry.active_parked.insert(bid);
            }
            "batch_needs_review" => {
                let bid = batch_id.unwrap_or_else(|| format!("anon_{at}"));
                entry.active_needs_review.insert(bid);
            }
            "batch_replayed" | "batch_discarded" => {
                if let Some(bid) = batch_id {
                    entry.active_needs_review.remove(&bid);
                    entry.active_parked.remove(&bid);
                }
            }
            "agent_paused" => match payload.as_deref() {
                None => {}
                Some(raw) => match serde_json::from_str::<serde_json::Value>(raw) {
                    Ok(v) => {
                        if let Some(until) =
                            v.get("until").and_then(|u| u.as_str()).map(str::to_string)
                        {
                            entry.latest_paused_until = Some(until);
                        }
                    }
                    Err(err) => {
                        eprintln!("buzz-desktop: corrupted payload in agent_paused event: {err}");
                        entry.latest_paused_until = Some("degraded: malformed payload".to_string());
                    }
                },
            },
            "agent_resumed" => {
                entry.latest_paused_until = None;
            }
            "breaker_opened" => match parse_breaker_scope(payload.as_deref()) {
                Ok(scope) => {
                    entry.open_scopes.insert(scope);
                }
                Err(err) => {
                    eprintln!("buzz-desktop: corrupted payload in breaker_opened event: {err}");
                    // Fail open, not silently healthy: a corrupt open we
                    // cannot attribute to a real scope still must not read
                    // as "no breaker is open" — track it under a sentinel
                    // scope no real payload can ever produce.
                    entry
                        .open_scopes
                        .insert(CORRUPTED_BREAKER_SCOPE.to_string());
                }
            },
            "breaker_closed" => match parse_breaker_scope(payload.as_deref()) {
                Ok(scope) => {
                    entry.open_scopes.remove(&scope);
                }
                Err(err) => {
                    eprintln!("buzz-desktop: corrupted payload in breaker_closed event: {err}");
                    // Do NOT guess a scope to remove: closing scope "" (the
                    // old default) could silently clear an unrelated,
                    // legitimately-open breaker. Leave every tracked scope
                    // as-is — degraded/open is the safe failure here, not a
                    // wrong close.
                }
            },
            _ => {}
        }
    }

    Ok(by_agent
        .into_iter()
        .map(|(agent, a)| AgentHealthCounters {
            agent,
            turns: a.turns,
            failed: a.failed,
            parked: a.active_parked.len() as i64,
            needs_review: a.active_needs_review.len() as i64,
            reconnects: a.reconnects,
            last_failure_class: a.last_failure_class,
            last_failure_at: a.last_failure_at,
            latest_paused_until: a.latest_paused_until,
            breaker_open: !a.open_scopes.is_empty(),
        })
        .collect())
}

/// Read one agent's events newest-first, optionally restricted to `kinds`
/// and to the last `since_hours`, capped at `min(limit.unwrap_or(200), 200)`
/// regardless of what the caller asks for (design §3: the drawer shows at
/// most the last 200 events).
pub(crate) fn query_agent_health_events(
    conn: &Connection,
    agent: &str,
    kinds: Option<&[String]>,
    since_hours: Option<i64>,
    limit: Option<usize>,
    now: i64,
) -> Result<Vec<HealthEvent>, String> {
    const HARD_CAP: usize = 200;
    let effective_limit = limit.unwrap_or(HARD_CAP).min(HARD_CAP);
    let cutoff = validate_since_hours(since_hours, now)?;
    validate_kinds(kinds)?;

    if let Some(ks) = kinds {
        if ks.is_empty() {
            return Ok(Vec::new());
        }
    }

    let mut sql = String::from(
        "SELECT agent, at, kind, event_key, batch_id, channel_id, class, payload
         FROM health_events WHERE agent = ?1",
    );
    let mut params_vec: Vec<rusqlite::types::Value> = vec![agent.to_string().into()];

    if let Some(ks) = kinds {
        let placeholders = (0..ks.len())
            .map(|i| format!("?{}", params_vec.len() + 1 + i))
            .collect::<Vec<_>>()
            .join(", ");
        sql.push_str(&format!(" AND kind IN ({placeholders})"));
        for k in ks {
            params_vec.push(k.to_string().into());
        }
    }

    if let Some(cutoff_val) = cutoff {
        let param_idx = params_vec.len() + 1;
        sql.push_str(&format!(" AND at >= ?{param_idx}"));
        params_vec.push(cutoff_val.into());
    }

    let limit_param_idx = params_vec.len() + 1;
    sql.push_str(&format!(" ORDER BY at DESC LIMIT ?{limit_param_idx}"));
    params_vec.push((effective_limit as i64).into());

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("prepare agent-health events query: {e}"))?;

    let mapped = stmt
        .query_map(params_from_iter(params_vec), row_to_health_event)
        .map_err(|e| format!("query agent-health events: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read agent-health events: {e}"))?;

    Ok(mapped)
}

pub(crate) const KNOWN_HEALTH_FRAME_KINDS: &[&str] = &[
    "turn_failed",
    "batch_parked",
    "batch_replayed",
    "batch_needs_review",
    "agent_paused",
    "agent_resumed",
    "breaker_opened",
    "breaker_closed",
    "relay_reconnected",
];

pub(crate) const MAX_CLASS_CHARS: usize = 256;
pub(crate) const MAX_BATCH_ID_CHARS: usize = 128;
pub(crate) const MAX_CHANNEL_ID_CHARS: usize = 128;
/// Longest serialized `payload` a health frame may carry. `payload` is an
/// arbitrary `serde_json::Value` with no per-field caps of its own (unlike
/// `class`/`batch_id`/`channel_id`); without an overall cap a syntactically
/// valid frame can still carry an arbitrarily large or deeply nested value
/// that gets stored verbatim and re-serialized on every summary read.
pub(crate) const MAX_PAYLOAD_CHARS: usize = 8_192;

pub(crate) fn validate_hex64(agent: &str) -> Result<(), String> {
    if agent.len() != 64 || !agent.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "agent must be a 64-character hex string, got {agent:?}"
        ));
    }
    Ok(())
}

/// Whether `agent` may have local state created for it: a correctly shaped
/// hex64 id that is also a member of this machine's managed-agent roster.
/// `managed_agent_state_dir` unconditionally `create_dir_all`s its target —
/// every call site that takes an `agent` string from a Tauri command
/// argument (a real trust boundary) must gate on this first, never on hex64
/// format alone, so an unrecognized or remote-only id never mints a fresh
/// empty state directory on disk.
pub(crate) fn agent_may_have_local_state(known: &[ManagedAgentRecord], agent: &str) -> bool {
    validate_hex64(agent).is_ok() && known.iter().any(|record| record.pubkey == agent)
}

/// Shape of a frame delivered by `ingest_agent_health_frame`: what
/// `parseHealthFrame` (desktop TS, Step 7) hands over after mirroring one of
/// the nine observer health frames. `at` is RFC3339, matching the ledger.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HealthFrame {
    pub at: String,
    pub kind: String,
    pub batch_id: Option<String>,
    pub channel_id: Option<String>,
    pub class: Option<String>,
    pub payload: Option<serde_json::Value>,
}

pub(crate) fn frame_to_health_event(
    agent: &str,
    frame: &HealthFrame,
) -> Result<HealthEvent, String> {
    if !KNOWN_HEALTH_FRAME_KINDS.contains(&frame.kind.as_str()) {
        return Err(format!("unknown health frame kind: {:?}", frame.kind));
    }
    if let Some(c) = &frame.class {
        if c.chars().count() > MAX_CLASS_CHARS {
            return Err(format!("class string exceeds {MAX_CLASS_CHARS} characters"));
        }
    }
    if let Some(b) = &frame.batch_id {
        if b.chars().count() > MAX_BATCH_ID_CHARS {
            return Err(format!(
                "batch_id string exceeds {MAX_BATCH_ID_CHARS} characters"
            ));
        }
    }
    if let Some(ch) = &frame.channel_id {
        if ch.chars().count() > MAX_CHANNEL_ID_CHARS {
            return Err(format!(
                "channel_id string exceeds {MAX_CHANNEL_ID_CHARS} characters"
            ));
        }
    }
    if let Some(p) = &frame.payload {
        if p.to_string().chars().count() > MAX_PAYLOAD_CHARS {
            return Err(format!("payload exceeds {MAX_PAYLOAD_CHARS} characters"));
        }
    }

    let parsed_dt = chrono::DateTime::parse_from_rfc3339(&frame.at)
        .map_err(|e| format!("invalid agent-health frame timestamp {:?}: {e}", frame.at))?;
    let parsed_utc = parsed_dt.with_timezone(&chrono::Utc);

    let now = chrono::Utc::now();
    let max_future = now + chrono::Duration::minutes(5);
    if parsed_utc > max_future {
        return Err(format!(
            "frame timestamp {:?} is more than 5 minutes in the future",
            frame.at
        ));
    }

    let at = parsed_utc.timestamp();
    let at_rfc3339 = parsed_utc.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

    let target = frame.batch_id.clone().or_else(|| {
        frame
            .payload
            .as_ref()
            .and_then(|p| p.get("scope"))
            .and_then(|s| s.as_str())
            .map(str::to_string)
    });
    Ok(HealthEvent {
        agent: agent.to_string(),
        at,
        kind: frame.kind.clone(),
        event_key: compute_event_key(&at_rfc3339, &frame.kind, target.as_deref()),
        batch_id: frame.batch_id.clone(),
        channel_id: frame.channel_id.clone(),
        class: frame.class.clone(),
        payload: frame.payload.as_ref().map(|p| p.to_string()),
    })
}

#[tauri::command]
pub(crate) async fn get_agent_health_summary(
    since_hours: Option<i64>,
    app: AppHandle,
    store: State<'_, AgentHealthStore>,
    app_state: State<'_, crate::app_state::AppState>,
) -> Result<Vec<AgentHealthCounters>, String> {
    let (relay_url, owner_pubkey) = resolve_health_db_scope(&app_state)?;
    let write_lock = Arc::clone(&store.write_lock);
    blocking::run(move |_proof| {
        let _guard = write_lock.lock().map_err(|e| e.to_string())?;
        let conn = open_db(&db_path(&app, &relay_url, &owner_pubkey)?)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        query_agent_health_summary(&conn, since_hours, now)
    })
    .await
}

#[tauri::command]
pub(crate) async fn get_agent_health_events(
    agent: String,
    kinds: Option<Vec<String>>,
    since_hours: Option<i64>,
    limit: Option<usize>,
    app: AppHandle,
    store: State<'_, AgentHealthStore>,
    app_state: State<'_, crate::app_state::AppState>,
) -> Result<Vec<HealthEvent>, String> {
    let (relay_url, owner_pubkey) = resolve_health_db_scope(&app_state)?;
    let write_lock = Arc::clone(&store.write_lock);
    blocking::run(move |_proof| {
        let _guard = write_lock.lock().map_err(|e| e.to_string())?;
        let conn = open_db(&db_path(&app, &relay_url, &owner_pubkey)?)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        query_agent_health_events(&conn, &agent, kinds.as_deref(), since_hours, limit, now)
    })
    .await
}

/// Sync one agent's ledger (or, when `agent` is `None`, every local agent
/// from `load_managed_agents`) into the health store. A remote-owned agent
/// with no ledger on this machine is skipped, not an error: `Health tab
/// only, no local ledger` is the desktop's row-level messaging for that case,
/// not a sync failure.
pub(crate) fn load_alert_state(
    conn: &Connection,
) -> Result<HashMap<(String, String), chrono::DateTime<chrono::Utc>>, String> {
    let mut stmt = conn
        .prepare("SELECT agent, rule, last_fired_at FROM alert_state")
        .map_err(|e| format!("prepare alert_state query: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            let agent: String = row.get(0)?;
            let rule: String = row.get(1)?;
            let last_fired_at: i64 = row.get(2)?;
            Ok(((agent, rule), last_fired_at))
        })
        .map_err(|e| format!("query alert_state: {e}"))?;

    let mut result = HashMap::new();
    for item in rows {
        let ((agent, rule), last_fired_at) =
            item.map_err(|e| format!("read alert_state row: {e}"))?;
        if let Some(dt) = chrono::DateTime::from_timestamp(last_fired_at, 0) {
            result.insert((agent, rule), dt);
        }
    }
    Ok(result)
}

pub(crate) fn record_alerts(
    conn: &Connection,
    alerts: &[crate::agent_health_alerts::Alert],
    now: i64,
) -> Result<(), String> {
    if alerts.is_empty() {
        return Ok(());
    }
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("begin transaction for record_alerts: {e}"))?;
    {
        let mut stmt = tx
            .prepare(
                "INSERT INTO alert_state (agent, rule, last_fired_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(agent, rule) DO UPDATE SET last_fired_at = excluded.last_fired_at",
            )
            .map_err(|e| format!("prepare insert alert_state: {e}"))?;
        for alert in alerts {
            stmt.execute(params![alert.agent, alert.rule, now])
                .map_err(|e| format!("record alert_state: {e}"))?;
        }
    }
    tx.commit()
        .map_err(|e| format!("commit alert_state: {e}"))?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HealthIngestResult {
    pub inserted: usize,
    pub alerts: Vec<crate::agent_health_alerts::Alert>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<(String, String)>,
}

#[allow(dead_code)]
pub type IngestResult = HealthIngestResult;

/// `known_secrets` are literal values scrubbed unconditionally, on top of
/// `redact_secrets_with`'s built-in shape-based patterns (bearer tokens,
/// known API-key prefixes, GitHub token shapes). Those patterns alone miss
/// any secret that doesn't look like one of them — a custom provider key, or
/// this agent's own nsec, echoed verbatim into a failure line. Callers
/// should pass every secret value this agent's own configuration holds.
pub(crate) fn sanitize_last_error(raw: &str, known_secrets: &[&str]) -> String {
    let redacted = crate::managed_agents::redact_secrets_with(raw, known_secrets);
    buzz_acp_pkg::reliability::error_class::truncate_chars(
        &redacted,
        buzz_acp_pkg::reliability::ledger::MAX_RAW_CHARS,
    )
}

/// Local agent records to sync, plus `(agent, error)` pairs for agents whose
/// own state could not be read.
pub(crate) type ManagedAgentsForSync = (Vec<ManagedAgentRecord>, Vec<(String, String)>);

/// What to do with a `load_managed_agents` result inside `sync_agent_health`:
/// a full-sync call (`targeted_agent: None`) has no other source of truth for
/// which agents exist, so a broken store is a hard error; a single-agent sync
/// already has its target and can proceed with an empty roster, recording the
/// failure instead of pretending the store came back clean. Factored out of
/// `sync_agent_health` (which needs a real `AppHandle` and cannot be unit
/// tested directly) so this decision has its own binding test.
pub(crate) fn resolve_managed_agents_for_sync(
    load_result: Result<Vec<ManagedAgentRecord>, String>,
    targeted_agent: Option<&str>,
) -> Result<ManagedAgentsForSync, String> {
    match load_result {
        Ok(agents) => Ok((agents, Vec::new())),
        Err(e) => {
            if targeted_agent.is_none() {
                Err(format!("load managed agents: {e}"))
            } else {
                let agent_id = targeted_agent.unwrap_or("all");
                Ok((
                    Vec::new(),
                    vec![(agent_id.to_string(), format!("load managed agents: {e}"))],
                ))
            }
        }
    }
}

/// Sync one agent's ledger (or, when `agent` is `None`, every local agent
/// from `load_managed_agents`) into the health store. A remote-owned agent
/// with no ledger on this machine is skipped, not an error: `Health tab
/// only, no local ledger` is the desktop's row-level messaging for that case,
/// not a sync failure.
#[tauri::command]
pub(crate) async fn sync_agent_health(
    agent: Option<String>,
    app: AppHandle,
    store: State<'_, AgentHealthStore>,
    app_state: State<'_, crate::app_state::AppState>,
) -> Result<HealthIngestResult, String> {
    let (relay_url, owner_pubkey) = resolve_health_db_scope(&app_state)?;
    let write_lock = Arc::clone(&store.write_lock);
    blocking::run(move |_proof| {
        let _guard = write_lock.lock().map_err(|e| e.to_string())?;
        let conn = open_db(&db_path(&app, &relay_url, &owner_pubkey)?)?;

        let (managed_agents, mut errors) =
            resolve_managed_agents_for_sync(load_managed_agents(&app), agent.as_deref())?;
        let pubkeys: Vec<String> = match agent {
            Some(pubkey) => vec![pubkey],
            None => managed_agents
                .iter()
                .map(|record| record.pubkey.clone())
                .collect(),
        };

        let mut inserted = 0usize;
        let mut all_events = Vec::new();
        let mut all_parked = Vec::new();
        let now_dt = chrono::Utc::now();
        let now_ts = now_dt.timestamp();

        for pubkey in &pubkeys {
            // `managed_agent_state_dir` unconditionally `create_dir_all`s its
            // target, so it must never run for an id this machine doesn't
            // actually manage locally — a single-agent sync request can name
            // any string, and a remote-owned agent legitimately has no local
            // state to read. Skip it exactly like "no local ledger yet",
            // without minting a state directory for it.
            if !agent_may_have_local_state(&managed_agents, pubkey) {
                continue;
            }
            let state_dir = managed_agent_state_dir(&app, pubkey);
            if let Ok(dir) = &state_dir {
                match read_parked_batches(dir) {
                    Ok(mut batches) => {
                        for b in &mut batches {
                            b.agent = Some(pubkey.clone());
                        }
                        all_parked.extend(batches);
                    }
                    Err(e) => {
                        errors.push((pubkey.clone(), e));
                    }
                }

                let ledger_path = dir.join(LEDGER_FILE);
                if ledger_path.exists() {
                    match sync_ledger(&conn, pubkey, &ledger_path, now_dt) {
                        Ok(c) => inserted += c,
                        Err(e) => errors.push((pubkey.clone(), e)),
                    }
                }
            }

            if let Ok(recent) =
                query_agent_health_events(&conn, pubkey, None, Some(1), Some(50), now_ts)
            {
                all_events.extend(recent);
            }

            if let Some(record) = managed_agents.iter().find(|r| &r.pubkey == pubkey) {
                let code = record
                    .last_error_code
                    .or_else(|| record.last_exit_code.map(|c| c as i64));
                if let Some(c) = code {
                    if c != 0 || record.last_error.is_some() {
                        let mut known_secrets: Vec<&str> =
                            record.env_vars.values().map(String::as_str).collect();
                        if !record.private_key_nsec.is_empty() {
                            known_secrets.push(record.private_key_nsec.as_str());
                        }
                        let sanitized_error = record
                            .last_error
                            .as_deref()
                            .map(|e| sanitize_last_error(e, &known_secrets));
                        let class_name = sanitized_error.as_deref().map(|e| {
                            buzz_acp_pkg::reliability::error_class::truncate_chars(
                                e,
                                buzz_acp_pkg::reliability::ledger::MAX_LABEL_CHARS,
                            )
                        });
                        all_events.push(HealthEvent {
                            agent: pubkey.clone(),
                            at: now_ts,
                            kind: "process_exit".to_string(),
                            event_key: compute_event_key(
                                &now_dt.to_rfc3339(),
                                "process_exit",
                                None,
                            ),
                            batch_id: None,
                            channel_id: None,
                            class: class_name,
                            payload: Some(
                                serde_json::json!({
                                    "code": c,
                                    "lastError": sanitized_error,
                                })
                                .to_string(),
                            ),
                        });
                    }
                }
            }
        }

        // `open_db` only prunes once, before this call's inserts run. A sync
        // can insert an entire ledger's worth of rows in one call, so the
        // byte/row budget must be re-enforced here too — otherwise the
        // database can sit over budget for the rest of this session, until
        // the app is restarted and `open_db` runs again.
        if inserted > 0 {
            prune(&conn, now_ts)?;
        }

        let last_fired = load_alert_state(&conn)?;
        let alerts =
            crate::agent_health_alerts::evaluate(&all_events, &all_parked, now_dt, &last_fired);

        Ok(HealthIngestResult {
            inserted,
            alerts,
            errors,
        })
    })
    .await
}

/// Trigger an asynchronous sync of an agent's ledger into the health store.
///
/// Spawns onto Tauri's async runtime so callers in synchronous command or setup
/// contexts do not block. If `pubkey` is empty, syncs all local agents.
pub(crate) fn sync_for_agent(app: &AppHandle, pubkey: &str) {
    let store = app.state::<AgentHealthStore>();
    let in_flight = Arc::clone(&store.in_flight_syncs);
    let dirty = Arc::clone(&store.dirty_syncs);
    let key = pubkey.to_string();
    if matches!(
        claim_sync_slot(&in_flight, &dirty, &key),
        SyncClaim::AlreadyRunning
    ) {
        // Already running: `claim_sync_slot` marked `key` dirty, so the
        // active run reruns once more after it finishes — this request is
        // not dropped, just coalesced into that rerun.
        return;
    }
    let guard = InFlightGuard {
        in_flight: Arc::clone(&in_flight),
        dirty: Arc::clone(&dirty),
        key: key.clone(),
    };
    let app_handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let _guard = guard;
        loop {
            let store = app_handle.state::<AgentHealthStore>();
            let app_state = app_handle.state::<crate::app_state::AppState>();
            let agent_arg = if key.is_empty() {
                None
            } else {
                Some(key.clone())
            };
            if let Err(e) = sync_agent_health(agent_arg, app_handle.clone(), store, app_state).await
            {
                eprintln!("buzz-desktop: agent_health sync failed for {key}: {e}");
            }
            if !finish_sync_or_rerun(&in_flight, &dirty, &key) {
                break;
            }
        }
    });
}

/// Insert one health frame mirrored live from the harness observer (Step 7's
/// `parseHealthFrame`). Duplicate-safe like ledger sync: the same
/// `(agent, event_key)` primary key ignores a frame that a later
/// `sync_agent_health` also picks up from the ledger.
#[tauri::command]
pub(crate) async fn ingest_agent_health_frame(
    agent: String,
    frame: serde_json::Value,
    app: AppHandle,
    store: State<'_, AgentHealthStore>,
    app_state: State<'_, crate::app_state::AppState>,
) -> Result<HealthIngestResult, String> {
    validate_hex64(&agent)?;
    let health_frame: HealthFrame =
        serde_json::from_value(frame).map_err(|e| format!("parse agent-health frame: {e}"))?;
    let event = frame_to_health_event(&agent, &health_frame)?;
    let (relay_url, owner_pubkey) = resolve_health_db_scope(&app_state)?;

    let write_lock = Arc::clone(&store.write_lock);
    blocking::run(move |_proof| {
        let _guard = write_lock.lock().map_err(|e| e.to_string())?;

        // A syntactically valid hex64 string is not proof this frame really
        // came from that agent's own observer session — nothing upstream of
        // this command signs or otherwise authenticates the mirrored frame.
        // Require current membership in the local managed-agent roster
        // before persisting anything under that identity, exactly like the
        // parked-batch lookup below already does: an unrecognized id is a
        // no-op (remote-owned agents legitimately have no local health
        // state), never an insert.
        let (known, errors) =
            resolve_managed_agents_for_sync(load_managed_agents(&app), Some(&agent))?;
        if !agent_may_have_local_state(&known, &agent) {
            return Ok(HealthIngestResult {
                inserted: 0,
                alerts: Vec::new(),
                errors,
            });
        }

        let conn = open_db(&db_path(&app, &relay_url, &owner_pubkey)?)?;
        let inserted = insert_event(&conn, &event)?;
        if inserted {
            prune(&conn, chrono::Utc::now().timestamp())?;
        }

        let now_dt = chrono::Utc::now();
        let last_fired = load_alert_state(&conn)?;

        let mut parked = match managed_agent_state_dir(&app, &agent) {
            Ok(d) => read_parked_batches(&d)?,
            Err(_) => Vec::new(),
        };
        for b in &mut parked {
            b.agent = Some(agent.clone());
        }

        let alerts = crate::agent_health_alerts::evaluate(
            std::slice::from_ref(&event),
            &parked,
            now_dt,
            &last_fired,
        );

        Ok(HealthIngestResult {
            inserted: if inserted { 1 } else { 0 },
            alerts,
            errors,
        })
    })
    .await
}

#[path = "agent_health/acks.rs"]
mod acks;
#[path = "agent_health/parked.rs"]
mod parked;

pub(crate) use acks::*;
pub(crate) use parked::*;

#[cfg(test)]
#[path = "agent_health/tests.rs"]
mod tests;
#[cfg(test)]
#[path = "agent_health/tests_ingest.rs"]
mod tests_ingest;

// SQL emits one row per agent, not every retained failed turn.
const LATEST_FAILURES_SQL: &str = "SELECT agent, at, class FROM (
    SELECT agent, at, class, ROW_NUMBER() OVER (
        PARTITION BY agent ORDER BY at DESC, event_key DESC
    ) AS ordinal FROM health_events
    WHERE kind = 'turn_failed' AND (?1 IS NULL OR at >= ?1)
) WHERE ordinal = 1";

#[cfg(all(test, unix))]
#[path = "agent_health/tests_pipeline.rs"]
mod tests_pipeline;
