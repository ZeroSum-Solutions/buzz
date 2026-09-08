//! Bounded personal-calendar render cache, separate from the archive database.
use super::{dto::CalendarEvent, interval::ProvenInterval};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;

const MAX_PARTITION_BYTES: usize = 16 * 1024 * 1024;
const MAX_TOTAL_BYTES: i64 = 128 * 1024 * 1024;
const MAX_PARTITIONS: i64 = 64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub events: Vec<CalendarEvent>,
    pub interval: Option<ProvenInterval>,
    pub stale: bool,
    pub refreshed_at_ms: Option<i64>,
    pub generation: u64,
}

pub struct Cache(Connection);
impl Cache {
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|_| "cannot create calendar cache directory")?;
        }
        let resolved = path
            .parent()
            .ok_or("calendar cache has no parent")?
            .canonicalize()
            .map_err(|_| "calendar cache directory unavailable")?
            .join(path.file_name().ok_or("calendar cache has no filename")?);
        let path = resolved.as_path();
        if std::fs::symlink_metadata(path).is_ok_and(|metadata| !metadata.file_type().is_file()) {
            return Err("calendar cache is not a regular file".into());
        }
        let connection = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|_| "cannot open calendar cache")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(|_| "cannot protect calendar cache")?;
        }
        connection.execute_batch("PRAGMA foreign_keys=ON; PRAGMA temp_store=MEMORY; PRAGMA journal_mode=DELETE; PRAGMA secure_delete=ON; PRAGMA max_page_count=32768; CREATE TABLE IF NOT EXISTS snapshots (identity TEXT PRIMARY KEY, generation TEXT NOT NULL, expires INTEGER NOT NULL, touched INTEGER NOT NULL, body BLOB NOT NULL); CREATE TABLE IF NOT EXISTS events (identity TEXT NOT NULL REFERENCES snapshots(identity) ON DELETE CASCADE, ordinal INTEGER NOT NULL, body BLOB NOT NULL, PRIMARY KEY(identity,ordinal));").map_err(|_| "cannot initialize calendar cache")?;
        Ok(Self(connection))
    }
    pub fn read(
        &self,
        identity: &str,
        generation: u64,
        now: i64,
    ) -> Result<Option<Snapshot>, String> {
        let raw: Option<Vec<u8>> = self.0.query_row("SELECT body FROM snapshots WHERE identity=?1 AND generation=?2 AND expires>?3 AND length(body)<=?4", params![identity, generation.to_string(), now, MAX_PARTITION_BYTES], |row|row.get(0)).optional().map_err(|_| "cannot read calendar cache")?;
        let Some(bytes) = raw else {
            return Ok(None);
        };
        let (count,total_bytes,max_bytes):(usize,usize,usize)=self.0.query_row("SELECT count(*),coalesce(sum(length(body)),0),coalesce(max(length(body)),0) FROM events WHERE identity=?1",[identity],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).map_err(|_|"cannot inspect cached events")?;
        if count > super::client::MAX_EVENTS
            || total_bytes + bytes.len() > MAX_PARTITION_BYTES
            || max_bytes > 256 * 1024
        {
            return Err("calendar cache exceeded bounds".into());
        }
        let mut snapshot: Snapshot =
            serde_json::from_slice(&bytes).map_err(|_| "calendar cache is corrupt")?;
        if snapshot.generation != generation {
            return Err("calendar cache generation mismatch".into());
        }
        if !snapshot.events.is_empty() {
            return Err("unsupported calendar cache metadata".into());
        }
        let mut statement = self
            .0
            .prepare("SELECT body FROM events WHERE identity=?1 ORDER BY ordinal LIMIT 5001")
            .map_err(|_| "cannot read cached events")?;
        let rows = statement
            .query_map([identity], |row| row.get::<_, Vec<u8>>(0))
            .map_err(|_| "cannot read cached events")?;
        let mut total = bytes.len();
        for row in rows {
            let bytes = row.map_err(|_| "cannot read cached event")?;
            total += bytes.len();
            if bytes.len() > 256 * 1024
                || total > MAX_PARTITION_BYTES
                || snapshot.events.len() >= super::client::MAX_EVENTS
            {
                return Err("calendar cache exceeded bounds".into());
            }
            snapshot.events.push(
                serde_json::from_slice(&bytes).map_err(|_| "cached calendar event is corrupt")?,
            );
        }
        self.0
            .execute(
                "UPDATE snapshots SET touched=?2 WHERE identity=?1",
                params![identity, now],
            )
            .map_err(|_| "cannot touch calendar cache")?;
        Ok(Some(snapshot))
    }

    pub fn read_stale(
        &self,
        identity: &str,
        generation: u64,
        now: i64,
        authority_expires: i64,
    ) -> Result<Option<Snapshot>, String> {
        if now >= authority_expires {
            return Ok(None);
        }
        let mut snapshot = self.read(identity, generation, now)?;
        if let Some(snapshot) = &mut snapshot {
            snapshot.stale = true;
            for event in &mut snapshot.events {
                event.can_edit = false;
                event.can_delete = false;
            }
        }
        Ok(snapshot)
    }
    pub fn write(
        &mut self,
        identity: &str,
        snapshot: &Snapshot,
        expires: i64,
        now: i64,
    ) -> Result<(), String> {
        if snapshot.events.len() > super::client::MAX_EVENTS {
            return Err("calendar cache event cap exceeded".into());
        }
        let rows: Vec<Vec<u8>> = snapshot
            .events
            .iter()
            .map(serde_json::to_vec)
            .collect::<Result<_, _>>()
            .map_err(|_| "cannot encode calendar event")?;
        if rows.iter().any(|bytes| bytes.len() > 256 * 1024) {
            return Err("calendar cache row cap exceeded".into());
        }
        let mut metadata = snapshot.clone();
        metadata.events.clear();
        let bytes = serde_json::to_vec(&metadata).map_err(|_| "cannot encode calendar cache")?;
        let total_bytes = bytes.len() + rows.iter().map(Vec::len).sum::<usize>();
        if total_bytes > MAX_PARTITION_BYTES {
            return Err("calendar cache partition cap exceeded".into());
        }
        let tx = self
            .0
            .transaction()
            .map_err(|_| "cannot lock calendar cache")?;
        tx.execute(
            "DELETE FROM snapshots WHERE identity=?1 OR expires<=?2",
            params![identity, now],
        )
        .map_err(|_| "cannot evict calendar cache")?;
        loop {
            let (count, size): (i64, i64) = tx
                .query_row(
                    "SELECT count(*),coalesce(sum(length(body)),0)+(SELECT coalesce(sum(length(body)),0) FROM events) FROM snapshots",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(|_| "cannot inspect calendar cache")?;
            // Reserve space for SQLite pages and the rollback journal as well as data.
            if count < MAX_PARTITIONS && size + (total_bytes as i64) < MAX_TOTAL_BYTES / 4 {
                break;
            }
            tx.execute("DELETE FROM snapshots WHERE identity=(SELECT identity FROM snapshots ORDER BY touched LIMIT 1)", []).map_err(|_| "cannot evict calendar cache")?;
        }
        tx.execute(
            "INSERT INTO snapshots VALUES (?1,?2,?3,?4,?5)",
            params![
                identity,
                snapshot.generation.to_string(),
                expires,
                now,
                bytes
            ],
        )
        .map_err(|_| "cannot save calendar cache")?;
        for (ordinal, bytes) in rows.iter().enumerate() {
            tx.execute(
                "INSERT INTO events VALUES (?1,?2,?3)",
                params![identity, ordinal, bytes],
            )
            .map_err(|_| "cannot save cached calendar event")?;
        }
        tx.commit().map_err(|_| "cannot commit calendar cache")?;
        self.0
            .execute_batch("VACUUM;")
            .map_err(|_| "cannot compact calendar cache")?;
        Ok(())
    }
    pub fn purge(&self, identity: &str) -> Result<(), String> {
        self.0
            .execute("DELETE FROM snapshots WHERE identity=?1", [identity])
            .map_err(|_| "cannot purge calendar cache")?;
        self.0
            .execute_batch("VACUUM;")
            .map_err(|_| "cannot compact calendar cache")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn calendar_cache_expiry_and_generation_survive_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("calendar.sqlite");
        let snapshot = Snapshot {
            events: vec![],
            interval: None,
            stale: false,
            refreshed_at_ms: Some(1),
            generation: 7,
        };
        Cache::open(&path)
            .unwrap()
            .write("alice", &snapshot, 10, 1)
            .unwrap();
        let cache = Cache::open(&path).unwrap();
        assert!(cache.read("alice", 7, 9).unwrap().is_some());
        assert!(cache.read("alice", 7, 10).unwrap().is_none());
        assert!(cache.read("alice", 8, 2).unwrap().is_none());
        assert!(cache.read("bob", 7, 2).unwrap().is_none());
        cache.purge("alice").unwrap();
        assert!(cache.read("alice", 7, 2).unwrap().is_none());
    }
}

#[cfg(test)]
mod bounded_rows_tests {
    use super::*;
    #[test]
    fn calendar_cache_stores_bounded_event_rows_and_rejects_oversized_partition() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("calendar.sqlite");
        let mut cache = Cache::open(&path).unwrap();
        let event=super::super::dto::parse_single_event(br#"{"id":"fixture","etag":"version","status":"confirmed","start":{"date":"2026-09-08"},"end":{"date":"2026-09-09"}}"#,1024,super::super::dto::AccessRole::Owner).unwrap();
        let mut snapshot = Snapshot {
            events: vec![event.clone()],
            interval: None,
            stale: false,
            refreshed_at_ms: Some(1),
            generation: 4,
        };
        cache.write("alice", &snapshot, 100, 1).unwrap();
        let saved = cache.read("alice", 4, 2).unwrap().unwrap();
        assert_eq!(saved.events, vec![event.clone()]);
        let count: i64 = cache
            .0
            .query_row("SELECT count(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        snapshot.events = vec![event; super::super::client::MAX_EVENTS + 1];
        assert!(cache.write("alice", &snapshot, 100, 3).is_err());
        assert_eq!(cache.read("alice", 4, 4).unwrap().unwrap().events.len(), 1);
        cache.purge("alice").unwrap();
        let count: i64 = cache
            .0
            .query_row("SELECT count(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}

#[cfg(test)]
mod stale_tests {
    use super::*;
    #[test]
    fn calendar_stale_fallback_cannot_extend_secret_authority_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let mut cache = Cache::open(&directory.path().join("calendar.sqlite")).unwrap();
        let event=super::super::dto::parse_single_event(br#"{"id":"fixture","etag":"version","status":"confirmed","start":{"date":"2026-09-08"},"end":{"date":"2026-09-09"}}"#,1024,super::super::dto::AccessRole::Owner).unwrap();
        let snapshot = Snapshot {
            events: vec![event],
            interval: None,
            stale: false,
            refreshed_at_ms: Some(1),
            generation: 4,
        };
        cache.write("alice", &snapshot, 100, 1).unwrap();
        let stale = cache.read_stale("alice", 4, 9, 10).unwrap().unwrap();
        assert!(stale.stale);
        assert!(!stale.events[0].can_edit);
        assert!(!stale.events[0].can_delete);
        assert_eq!(stale.refreshed_at_ms, Some(1));
        assert!(cache.read_stale("alice", 4, 10, 10).unwrap().is_none());
    }
}
