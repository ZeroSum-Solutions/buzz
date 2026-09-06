//! Desktop health store: schema, insert-or-ignore, and retention.

use std::path::{Path, PathBuf};

use buzz_acp_pkg::reliability::ledger::{read_ledger_file, LedgerBody, LedgerRecord, TurnOutcome};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

#[allow(dead_code)]
pub(crate) const SCHEMA_VERSION: i64 = 1;
#[allow(dead_code)]
pub(crate) const RETENTION_SECS: i64 = 30 * 24 * 60 * 60;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
}
