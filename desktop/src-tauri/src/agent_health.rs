//! Desktop health store: schema, insert-or-ignore, and retention.

use std::path::{Path, PathBuf};

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
