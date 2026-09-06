//! Desktop health store: schema, insert-or-ignore, and retention.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use buzz_acp_pkg::reliability::ledger::{
    read_ledger_file, LedgerBody, LedgerRecord, TurnOutcome, LEDGER_FILE,
};
use rusqlite::{params, Connection, Row};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::managed_agents::storage::{load_managed_agents, managed_agent_state_dir};

#[allow(dead_code)]
pub(crate) const SCHEMA_VERSION: i64 = 1;
#[allow(dead_code)]
pub(crate) const RETENTION_SECS: i64 = 30 * 24 * 60 * 60;

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

pub(crate) fn compute_event_key(at_rfc3339: &str, kind: &str, target: Option<&str>) -> String {
    format!("{at_rfc3339}|{kind}|{}", target.unwrap_or(""))
}

#[allow(dead_code)]
pub(crate) fn db_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("resolve agent-health data dir: {e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("create agent-health data dir: {e}"))?;
    Ok(dir.join("agent-health.db"))
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
        CREATE INDEX IF NOT EXISTS health_events_kind_at ON health_events(kind, at);",
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
    conn.execute("DELETE FROM health_events WHERE at < ?1", params![cutoff])
        .map_err(|e| format!("prune agent-health events: {e}"))
}

/// Map one ledger record to a health event, or `None` when the record
/// carries no health signal (`turn_activity`).
///
/// `turn_finished` with an error outcome becomes kind `turn_failed` with
/// `raw` dropped; every other outcome (and `turn_started`) keeps its ledger
/// kind so `sync_ledger` can feed the turns-24h/7d counters.
fn ledger_record_to_health_event(agent: &str, record: &LedgerRecord) -> Option<HealthEvent> {
    let at_rfc3339 = record.at.to_rfc3339();
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
) -> Result<usize, String> {
    let records = read_ledger_file(ledger_path).map_err(|e| format!("read agent ledger: {e}"))?;
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

#[derive(Default)]
pub struct AgentHealthStore {
    write_lock: Arc<Mutex<()>>,
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
#[derive(Default)]
struct SummaryAccumulator {
    turns: i64,
    failed: i64,
    parked: i64,
    needs_review: i64,
    reconnects: i64,
    last_failure_class: Option<String>,
    last_failure_at: Option<i64>,
    latest_paused_until: Option<String>,
    breaker_open: bool,
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

/// One `health_events` row projected down to the columns the summary
/// grouping pass needs: agent, at, kind, class, payload.
type SummaryRow = (String, i64, String, Option<String>, Option<String>);

/// Group `health_events` by agent (optionally windowed to the last
/// `since_hours`) into the counters the Health tab and `buzz agents health`
/// both need: turns/failed/parked/needs_review/reconnects counts, the most
/// recent failure's class and timestamp, the most recent `agent_paused.until`
/// (read out of the event's JSON payload — there is no dedicated column for
/// it), and whether the latest breaker event for the agent was an open with
/// no later close.
pub(crate) fn query_agent_health_summary(
    conn: &Connection,
    since_hours: Option<i64>,
    now: i64,
) -> Result<Vec<AgentHealthCounters>, String> {
    let rows: Vec<SummaryRow> = if let Some(hours) = since_hours {
        let cutoff = now - hours * 3600;
        let mut stmt = conn
            .prepare(
                "SELECT agent, at, kind, class, payload FROM health_events
                     WHERE at >= ?1 ORDER BY agent ASC, at ASC",
            )
            .map_err(|e| format!("prepare agent-health summary query: {e}"))?;
        let mapped = stmt
            .query_map(params![cutoff], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .map_err(|e| format!("query agent-health summary: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("read agent-health summary: {e}"))?;
        mapped
    } else {
        let mut stmt = conn
            .prepare(
                "SELECT agent, at, kind, class, payload FROM health_events
                     ORDER BY agent ASC, at ASC",
            )
            .map_err(|e| format!("prepare agent-health summary query: {e}"))?;
        let mapped = stmt
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .map_err(|e| format!("query agent-health summary: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("read agent-health summary: {e}"))?;
        mapped
    };

    let mut by_agent: BTreeMap<String, SummaryAccumulator> = BTreeMap::new();
    for (agent, at, kind, class, payload) in rows {
        let entry = by_agent.entry(agent).or_default();
        match kind.as_str() {
            "turn_finished" => entry.turns += 1,
            "turn_failed" => {
                entry.turns += 1;
                entry.failed += 1;
                entry.last_failure_class = class;
                entry.last_failure_at = Some(at);
            }
            "batch_parked" => entry.parked += 1,
            "batch_needs_review" => entry.needs_review += 1,
            "relay_reconnected" => entry.reconnects += 1,
            "agent_paused" => {
                let until = payload
                    .as_deref()
                    .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                    .and_then(|v| v.get("until").and_then(|u| u.as_str()).map(str::to_string));
                if until.is_some() {
                    entry.latest_paused_until = until;
                }
            }
            "breaker_opened" => entry.breaker_open = true,
            "breaker_closed" => entry.breaker_open = false,
            _ => {}
        }
    }

    Ok(by_agent
        .into_iter()
        .map(|(agent, a)| AgentHealthCounters {
            agent,
            turns: a.turns,
            failed: a.failed,
            parked: a.parked,
            needs_review: a.needs_review,
            reconnects: a.reconnects,
            last_failure_class: a.last_failure_class,
            last_failure_at: a.last_failure_at,
            latest_paused_until: a.latest_paused_until,
            breaker_open: a.breaker_open,
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

    let events: Vec<HealthEvent> = if let Some(hours) = since_hours {
        let cutoff = now - hours * 3600;
        let mut stmt = conn
            .prepare(
                "SELECT agent, at, kind, event_key, batch_id, channel_id, class, payload
                 FROM health_events WHERE agent = ?1 AND at >= ?2 ORDER BY at DESC",
            )
            .map_err(|e| format!("prepare agent-health events query: {e}"))?;
        let mapped = stmt
            .query_map(params![agent, cutoff], row_to_health_event)
            .map_err(|e| format!("query agent-health events: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("read agent-health events: {e}"))?;
        mapped
    } else {
        let mut stmt = conn
            .prepare(
                "SELECT agent, at, kind, event_key, batch_id, channel_id, class, payload
                 FROM health_events WHERE agent = ?1 ORDER BY at DESC",
            )
            .map_err(|e| format!("prepare agent-health events query: {e}"))?;
        let mapped = stmt
            .query_map(params![agent], row_to_health_event)
            .map_err(|e| format!("query agent-health events: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("read agent-health events: {e}"))?;
        mapped
    };

    Ok(events
        .into_iter()
        .filter(|event| {
            kinds
                .map(|ks| ks.iter().any(|k| k == &event.kind))
                .unwrap_or(true)
        })
        .take(effective_limit)
        .collect())
}

/// Shape of a frame delivered by `ingest_agent_health_frame`: what
/// `parseHealthFrame` (desktop TS, Step 7) hands over after mirroring one of
/// the nine observer health frames. `at` is RFC3339, matching the ledger.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HealthFrame {
    at: String,
    kind: String,
    batch_id: Option<String>,
    channel_id: Option<String>,
    class: Option<String>,
    payload: Option<serde_json::Value>,
}

fn frame_to_health_event(agent: &str, frame: &HealthFrame) -> Result<HealthEvent, String> {
    let at = chrono::DateTime::parse_from_rfc3339(&frame.at)
        .map_err(|e| format!("invalid agent-health frame timestamp {:?}: {e}", frame.at))?
        .timestamp();
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
        event_key: compute_event_key(&frame.at, &frame.kind, target.as_deref()),
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
) -> Result<Vec<AgentHealthCounters>, String> {
    let write_lock = Arc::clone(&store.write_lock);
    blocking::run(move |_proof| {
        let _guard = write_lock.lock().map_err(|e| e.to_string())?;
        let conn = open_db(&db_path(&app)?)?;
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
) -> Result<Vec<HealthEvent>, String> {
    let write_lock = Arc::clone(&store.write_lock);
    blocking::run(move |_proof| {
        let _guard = write_lock.lock().map_err(|e| e.to_string())?;
        let conn = open_db(&db_path(&app)?)?;
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
#[tauri::command]
pub(crate) async fn sync_agent_health(
    agent: Option<String>,
    app: AppHandle,
    store: State<'_, AgentHealthStore>,
) -> Result<usize, String> {
    let write_lock = Arc::clone(&store.write_lock);
    blocking::run(move |_proof| {
        let _guard = write_lock.lock().map_err(|e| e.to_string())?;
        let conn = open_db(&db_path(&app)?)?;

        let pubkeys: Vec<String> = match agent {
            Some(pubkey) => vec![pubkey],
            None => load_managed_agents(&app)?
                .into_iter()
                .map(|record| record.pubkey)
                .collect(),
        };

        let mut inserted = 0usize;
        for pubkey in pubkeys {
            let ledger_path = managed_agent_state_dir(&app, &pubkey)?.join(LEDGER_FILE);
            if !ledger_path.exists() {
                continue;
            }
            inserted += sync_ledger(&conn, &pubkey, &ledger_path)?;
        }
        Ok(inserted)
    })
    .await
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
) -> Result<bool, String> {
    let health_frame: HealthFrame =
        serde_json::from_value(frame).map_err(|e| format!("parse agent-health frame: {e}"))?;
    let event = frame_to_health_event(&agent, &health_frame)?;

    let write_lock = Arc::clone(&store.write_lock);
    blocking::run(move |_proof| {
        let _guard = write_lock.lock().map_err(|e| e.to_string())?;
        let conn = open_db(&db_path(&app)?)?;
        insert_event(&conn, &event)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_acp_pkg::reliability::ledger::{AgentPaused, BatchParked, Ledger, TurnStarted};
    use chrono::Utc;
    use uuid::Uuid;

    fn db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_db(&dir.path().join("agent-health.db")).unwrap();
        (dir, conn)
    }

    #[test]
    fn insert_ignores_duplicate_event_key() {
        let (_d, conn) = db();
        let event = HealthEvent {
            agent: "agent_alpha".to_string(),
            at: 1700000000,
            kind: "turn_finished".to_string(),
            event_key: compute_event_key("2026-09-06T15:00:00Z", "turn_finished", Some("batch-1")),
            batch_id: Some("batch-1".to_string()),
            channel_id: Some("channel-1".to_string()),
            class: Some("provider_error".to_string()),
            payload: Some("{\"result\":\"error\"}".to_string()),
        };

        let first = insert_event(&conn, &event).unwrap();
        assert!(first, "first insert must succeed");

        let second = insert_event(&conn, &event).unwrap();
        assert!(!second, "duplicate (agent, event_key) must be ignored");

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM health_events WHERE agent = ?1",
                [&event.agent],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "only one row should exist after duplicate insert");
    }

    #[test]
    fn retention_prunes_older_than_30_days() {
        let (_d, conn) = db();
        let now = 1_725_600_000i64;
        let cutoff = now - RETENTION_SECS;

        let old_event = HealthEvent {
            agent: "agent_alpha".to_string(),
            at: cutoff - 1,
            kind: "turn_finished".to_string(),
            event_key: "old_event_key".to_string(),
            batch_id: Some("b-old".to_string()),
            channel_id: None,
            class: None,
            payload: None,
        };

        let boundary_event = HealthEvent {
            agent: "agent_alpha".to_string(),
            at: cutoff,
            kind: "turn_finished".to_string(),
            event_key: "boundary_event_key".to_string(),
            batch_id: Some("b-boundary".to_string()),
            channel_id: None,
            class: None,
            payload: None,
        };

        let recent_event = HealthEvent {
            agent: "agent_alpha".to_string(),
            at: now - 3600,
            kind: "turn_finished".to_string(),
            event_key: "recent_event_key".to_string(),
            batch_id: Some("b-recent".to_string()),
            channel_id: None,
            class: None,
            payload: None,
        };

        assert!(insert_event(&conn, &old_event).unwrap());
        assert!(insert_event(&conn, &boundary_event).unwrap());
        assert!(insert_event(&conn, &recent_event).unwrap());

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM health_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 3);

        let pruned = prune(&conn, now).unwrap();
        assert_eq!(pruned, 1, "exactly 1 old event should be pruned");

        let count_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM health_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count_after, 2, "2 events should remain");
    }

    #[test]
    fn sync_is_idempotent_and_inserts_only_missing_records() {
        let (_d, conn) = db();
        let ledger_dir = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let mut ledger = Ledger::open(ledger_dir.path(), "agent_alpha", now).unwrap();

        let batch_id = Uuid::new_v4();
        let channel_id = Uuid::new_v4();
        ledger
            .append(
                now,
                LedgerBody::TurnStarted(TurnStarted::new(
                    batch_id,
                    channel_id,
                    "scope-1",
                    vec!["event-1".to_string()],
                    1,
                )),
            )
            .unwrap();
        ledger
            .append(
                now,
                LedgerBody::BatchParked(BatchParked {
                    batch_id,
                    channel_id,
                    reason: "retries_exhausted".to_string(),
                    started: true,
                    events: 3,
                }),
            )
            .unwrap();
        ledger
            .append(
                now,
                LedgerBody::AgentPaused(AgentPaused {
                    class: "capacity_exhausted".to_string(),
                    until: now,
                    waiting: 2,
                }),
            )
            .unwrap();

        let ledger_path = ledger.path().to_path_buf();

        let first = sync_ledger(&conn, "agent_alpha", &ledger_path).unwrap();
        assert_eq!(first, 3, "all three ledger records must be inserted once");

        let second = sync_ledger(&conn, "agent_alpha", &ledger_path).unwrap();
        assert_eq!(second, 0, "a repeat sync must insert nothing new");

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM health_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 3, "row count must not double after a second sync");
    }

    #[test]
    fn summary_counts_per_agent_within_window() {
        let (_d, conn) = db();
        let now = 1_725_600_000i64;
        let window_hours = 24;
        let cutoff = now - window_hours * 3600;

        // agent_1 events within window
        insert_event(
            &conn,
            &HealthEvent {
                agent: "agent_1".to_string(),
                at: now - 1000,
                kind: "turn_finished".to_string(),
                event_key: "k1".to_string(),
                batch_id: Some("b1".to_string()),
                channel_id: Some("c1".to_string()),
                class: None,
                payload: Some(r#"{"result":"ok"}"#.to_string()),
            },
        )
        .unwrap();

        insert_event(
            &conn,
            &HealthEvent {
                agent: "agent_1".to_string(),
                at: now - 2000,
                kind: "turn_finished".to_string(),
                event_key: "k2".to_string(),
                batch_id: Some("b2".to_string()),
                channel_id: Some("c1".to_string()),
                class: None,
                payload: Some(r#"{"result":"ok"}"#.to_string()),
            },
        )
        .unwrap();

        insert_event(
            &conn,
            &HealthEvent {
                agent: "agent_1".to_string(),
                at: now - 3000,
                kind: "turn_failed".to_string(),
                event_key: "k3".to_string(),
                batch_id: Some("b3".to_string()),
                channel_id: Some("c1".to_string()),
                class: Some("capacity_exhausted".to_string()),
                payload: Some(r#"{"result":"error","class":"capacity_exhausted"}"#.to_string()),
            },
        )
        .unwrap();

        insert_event(
            &conn,
            &HealthEvent {
                agent: "agent_1".to_string(),
                at: now - 4000,
                kind: "batch_parked".to_string(),
                event_key: "k4".to_string(),
                batch_id: Some("b4".to_string()),
                channel_id: Some("c1".to_string()),
                class: None,
                payload: Some(r#"{"reason":"retries_exhausted"}"#.to_string()),
            },
        )
        .unwrap();

        insert_event(
            &conn,
            &HealthEvent {
                agent: "agent_1".to_string(),
                at: now - 5000,
                kind: "batch_needs_review".to_string(),
                event_key: "k5".to_string(),
                batch_id: Some("b5".to_string()),
                channel_id: Some("c1".to_string()),
                class: None,
                payload: Some(r#"{"reason":"manual_intervention"}"#.to_string()),
            },
        )
        .unwrap();

        insert_event(
            &conn,
            &HealthEvent {
                agent: "agent_1".to_string(),
                at: now - 6000,
                kind: "relay_reconnected".to_string(),
                event_key: "k6".to_string(),
                batch_id: None,
                channel_id: None,
                class: None,
                payload: Some(r#"{"afterSecs":5}"#.to_string()),
            },
        )
        .unwrap();

        insert_event(
            &conn,
            &HealthEvent {
                agent: "agent_1".to_string(),
                at: now - 7000,
                kind: "agent_paused".to_string(),
                event_key: "k7".to_string(),
                batch_id: None,
                channel_id: None,
                class: Some("capacity_exhausted".to_string()),
                payload: Some(
                    r#"{"class":"capacity_exhausted","until":"2026-09-07T00:00:00Z","waiting":2}"#
                        .to_string(),
                ),
            },
        )
        .unwrap();

        insert_event(
            &conn,
            &HealthEvent {
                agent: "agent_1".to_string(),
                at: now - 8000,
                kind: "breaker_opened".to_string(),
                event_key: "k8".to_string(),
                batch_id: None,
                channel_id: None,
                class: None,
                payload: Some(r#"{"scope":"scope1","consecutive":3}"#.to_string()),
            },
        )
        .unwrap();

        // agent_1 events OUTSIDE the window (older than 24 hours)
        insert_event(
            &conn,
            &HealthEvent {
                agent: "agent_1".to_string(),
                at: cutoff - 100,
                kind: "turn_finished".to_string(),
                event_key: "old_k1".to_string(),
                batch_id: Some("old_b1".to_string()),
                channel_id: Some("c1".to_string()),
                class: None,
                payload: Some(r#"{"result":"ok"}"#.to_string()),
            },
        )
        .unwrap();

        insert_event(
            &conn,
            &HealthEvent {
                agent: "agent_1".to_string(),
                at: cutoff - 200,
                kind: "turn_failed".to_string(),
                event_key: "old_k2".to_string(),
                batch_id: Some("old_b2".to_string()),
                channel_id: Some("c1".to_string()),
                class: Some("old_class".to_string()),
                payload: Some(r#"{"result":"error"}"#.to_string()),
            },
        )
        .unwrap();

        insert_event(
            &conn,
            &HealthEvent {
                agent: "agent_1".to_string(),
                at: cutoff - 300,
                kind: "batch_parked".to_string(),
                event_key: "old_k3".to_string(),
                batch_id: Some("old_b3".to_string()),
                channel_id: None,
                class: None,
                payload: None,
            },
        )
        .unwrap();

        insert_event(
            &conn,
            &HealthEvent {
                agent: "agent_1".to_string(),
                at: cutoff - 400,
                kind: "batch_needs_review".to_string(),
                event_key: "old_k4".to_string(),
                batch_id: Some("old_b4".to_string()),
                channel_id: None,
                class: None,
                payload: None,
            },
        )
        .unwrap();

        insert_event(
            &conn,
            &HealthEvent {
                agent: "agent_1".to_string(),
                at: cutoff - 500,
                kind: "relay_reconnected".to_string(),
                event_key: "old_k5".to_string(),
                batch_id: None,
                channel_id: None,
                class: None,
                payload: None,
            },
        )
        .unwrap();

        // agent_2 within window: breaker opened and closed (breaker not open)
        insert_event(
            &conn,
            &HealthEvent {
                agent: "agent_2".to_string(),
                at: now - 3000,
                kind: "turn_finished".to_string(),
                event_key: "a2_k1".to_string(),
                batch_id: Some("a2_b1".to_string()),
                channel_id: None,
                class: None,
                payload: None,
            },
        )
        .unwrap();

        insert_event(
            &conn,
            &HealthEvent {
                agent: "agent_2".to_string(),
                at: now - 2000,
                kind: "breaker_opened".to_string(),
                event_key: "a2_k2".to_string(),
                batch_id: None,
                channel_id: None,
                class: None,
                payload: Some(r#"{"scope":"scope2"}"#.to_string()),
            },
        )
        .unwrap();

        insert_event(
            &conn,
            &HealthEvent {
                agent: "agent_2".to_string(),
                at: now - 1000,
                kind: "breaker_closed".to_string(),
                event_key: "a2_k3".to_string(),
                batch_id: None,
                channel_id: None,
                class: None,
                payload: Some(r#"{"scope":"scope2"}"#.to_string()),
            },
        )
        .unwrap();

        let summaries = query_agent_health_summary(&conn, Some(window_hours), now).unwrap();
        assert_eq!(summaries.len(), 2);

        let a1 = summaries.iter().find(|s| s.agent == "agent_1").unwrap();
        assert_eq!(a1.turns, 3, "turns within window must be 3 (b1, b2, b3)");
        assert_eq!(a1.failed, 1, "failed turns within window must be 1 (b3)");
        assert_eq!(a1.parked, 1, "parked within window must be 1 (b4)");
        assert_eq!(
            a1.needs_review, 1,
            "needs_review within window must be 1 (b5)"
        );
        assert_eq!(a1.reconnects, 1, "reconnects within window must be 1");
        assert_eq!(a1.last_failure_class.as_deref(), Some("capacity_exhausted"));
        assert_eq!(a1.last_failure_at, Some(now - 3000));
        assert_eq!(
            a1.latest_paused_until.as_deref(),
            Some("2026-09-07T00:00:00Z")
        );
        assert!(a1.breaker_open, "breaker must be open for agent_1");

        let a2 = summaries.iter().find(|s| s.agent == "agent_2").unwrap();
        assert_eq!(a2.turns, 1);
        assert_eq!(a2.failed, 0);
        assert_eq!(a2.parked, 0);
        assert_eq!(a2.needs_review, 0);
        assert_eq!(a2.reconnects, 0);
        assert_eq!(a2.last_failure_class, None);
        assert_eq!(a2.last_failure_at, None);
        assert_eq!(a2.latest_paused_until, None);
        assert!(!a2.breaker_open, "breaker must be closed for agent_2");
    }

    #[test]
    fn events_are_filtered_by_kind_and_capped() {
        let (_d, conn) = db();
        let now = 1_725_600_000i64;

        // Insert 10 turn_finished, 10 turn_failed, 10 batch_parked
        for i in 0..10 {
            insert_event(
                &conn,
                &HealthEvent {
                    agent: "agent_alpha".to_string(),
                    at: now - 3000 + i,
                    kind: "turn_finished".to_string(),
                    event_key: format!("tf_{i}"),
                    batch_id: Some(format!("b_tf_{i}")),
                    channel_id: None,
                    class: None,
                    payload: None,
                },
            )
            .unwrap();
        }

        for i in 0..10 {
            insert_event(
                &conn,
                &HealthEvent {
                    agent: "agent_alpha".to_string(),
                    at: now - 2000 + i,
                    kind: "turn_failed".to_string(),
                    event_key: format!("err_{i}"),
                    batch_id: Some(format!("b_err_{i}")),
                    channel_id: None,
                    class: Some("err".to_string()),
                    payload: None,
                },
            )
            .unwrap();
        }

        for i in 0..10 {
            insert_event(
                &conn,
                &HealthEvent {
                    agent: "agent_alpha".to_string(),
                    at: now - 1000 + i,
                    kind: "batch_parked".to_string(),
                    event_key: format!("park_{i}"),
                    batch_id: Some(format!("b_park_{i}")),
                    channel_id: None,
                    class: None,
                    payload: None,
                },
            )
            .unwrap();
        }

        // Insert 250 relay_reconnected events (to test 200 cap)
        for i in 0..250 {
            insert_event(
                &conn,
                &HealthEvent {
                    agent: "agent_alpha".to_string(),
                    at: now - 500 + i,
                    kind: "relay_reconnected".to_string(),
                    event_key: format!("recon_{i}"),
                    batch_id: None,
                    channel_id: None,
                    class: None,
                    payload: None,
                },
            )
            .unwrap();
        }

        // 1. Filter by kinds: only turn_failed and batch_parked
        let kinds = vec!["turn_failed".to_string(), "batch_parked".to_string()];
        let filtered =
            query_agent_health_events(&conn, "agent_alpha", Some(&kinds), None, Some(50), now)
                .unwrap();
        assert_eq!(filtered.len(), 20);
        assert!(filtered
            .iter()
            .all(|e| e.kind == "turn_failed" || e.kind == "batch_parked"));
        // Check order is descending by `at`
        for window in filtered.windows(2) {
            assert!(window[0].at >= window[1].at);
        }

        // 2. Capped by requested limit
        let limited =
            query_agent_health_events(&conn, "agent_alpha", None, None, Some(5), now).unwrap();
        assert_eq!(limited.len(), 5);

        // 3. Hard cap at 200 even when limit requested > 200
        let capped = query_agent_health_events(
            &conn,
            "agent_alpha",
            Some(&["relay_reconnected".to_string()]),
            None,
            Some(300),
            now,
        )
        .unwrap();
        assert_eq!(capped.len(), 200, "query must cap results to at most 200");
    }
}
