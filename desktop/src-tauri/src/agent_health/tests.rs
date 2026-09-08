use super::*;
use buzz_acp_pkg::reliability::ledger::{AgentPaused, BatchParked, Ledger, TurnStarted};
use chrono::Utc;
use uuid::Uuid;

pub(super) fn db() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let conn = open_db(&dir.path().join("agent-health.db")).unwrap();
    (dir, conn)
}

#[test]
fn sync_rejects_incomplete_ledger_without_reporting_partial_success() {
    let (_d, conn) = db();
    let ledger_dir = tempfile::tempdir().unwrap();
    let now = Utc::now();
    let mut ledger = Ledger::open(ledger_dir.path(), "agent_alpha", now).unwrap();
    ledger
        .append(
            now,
            LedgerBody::AgentPaused(AgentPaused {
                class: "capacity_exhausted".to_string(),
                until: now,
                waiting: 1,
            }),
        )
        .unwrap();
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(ledger.path())
        .unwrap();
    file.write_all(b"{broken record}\n").unwrap();
    let error = sync_ledger(&conn, "agent_alpha", ledger.path(), now).unwrap_err();
    assert!(error.contains("incomplete ledger"), "{error}");
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM health_events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        count, 0,
        "an incomplete scan must not look like a complete partial import"
    );
}

#[test]
fn last_failure_query_materializes_one_row_per_agent_at_scale() {
    let (_d, conn) = db();
    conn.execute_batch("BEGIN").unwrap();
    for at in 0..10_000 {
        conn.execute("INSERT INTO health_events(agent, event_key, at, kind, class) VALUES ('scale', ?1, ?2, 'turn_failed', 'provider_error')", params![at.to_string(), at]).unwrap();
    }
    conn.execute_batch("COMMIT").unwrap();
    let mut statement = conn.prepare(LATEST_FAILURES_SQL).unwrap();
    let rows = statement
        .query_map(params![Option::<i64>::None], |row| row.get::<_, i64>(1))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(rows, vec![9_999]);
    let rows = statement
        .query_map(params![10_000], |row| row.get::<_, i64>(1))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(rows.is_empty());
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

/// `retention_prunes_older_than_30_days` above proves `prune()` itself works,
/// but calls it directly — it does not prove `open_db` actually wires
/// `prune(&conn, now)?` into the open path. Close the connection and reopen
/// through `open_db` with no direct `prune` call: if the `prune(&conn, now)?`
/// line inside `open_db` (currently present) were ever removed, this test
/// would fail while `retention_prunes_older_than_30_days` would keep passing.
#[test]
fn retention_prunes_on_reopen_via_open_db() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("agent-health.db");
    let real_now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let far_past = real_now - RETENTION_SECS - 3600;

    {
        let conn = open_db(&db_path).unwrap();
        assert!(insert_event(
            &conn,
            &HealthEvent {
                agent: "agent_reopen".to_string(),
                at: far_past,
                kind: "turn_finished".to_string(),
                event_key: "reopen_old".to_string(),
                batch_id: None,
                channel_id: None,
                class: None,
                payload: None,
            },
        )
        .unwrap());
        assert!(insert_event(
            &conn,
            &HealthEvent {
                agent: "agent_reopen".to_string(),
                at: real_now - 3600,
                kind: "turn_finished".to_string(),
                event_key: "reopen_recent".to_string(),
                batch_id: None,
                channel_id: None,
                class: None,
                payload: None,
            },
        )
        .unwrap());
        // conn dropped here: no explicit prune() call on this connection.
    }

    // Reopening must run the retention prune as a side effect of open_db,
    // using its own real-clock `now` — not a test-supplied one.
    let reopened = open_db(&db_path).unwrap();
    let count: i64 = reopened
        .query_row(
            "SELECT COUNT(*) FROM health_events WHERE agent = 'agent_reopen'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 1,
        "the far-past event must be pruned on reopen, without a direct prune() call"
    );
    let remaining_key: String = reopened
        .query_row(
            "SELECT event_key FROM health_events WHERE agent = 'agent_reopen'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(remaining_key, "reopen_recent");
}

/// The two retention deletes inside `prune()` must commit or roll back
/// together. Drop `alert_state` out from under the connection so its delete
/// fails, and assert `prune()` propagates the error (and does not silently
/// prune `health_events` anyway) — binds the transaction wrapping, not just
/// its happy path.
#[test]
fn prune_rolls_back_health_events_when_alert_state_delete_fails() {
    let (_d, conn) = db();
    let now = 1_725_600_000i64;
    let cutoff = now - RETENTION_SECS;

    assert!(insert_event(
        &conn,
        &HealthEvent {
            agent: "agent_txfail".to_string(),
            at: cutoff - 1,
            kind: "turn_finished".to_string(),
            event_key: "txfail_old".to_string(),
            batch_id: None,
            channel_id: None,
            class: None,
            payload: None,
        },
    )
    .unwrap());

    conn.execute("DROP TABLE alert_state", []).unwrap();

    let result = prune(&conn, now);
    assert!(
        result.is_err(),
        "prune must fail when the alert_state delete fails, not silently succeed"
    );

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM health_events WHERE agent = 'agent_txfail'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 1,
        "the health_events delete must roll back when alert_state's delete fails"
    );
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

    let first = sync_ledger(&conn, "agent_alpha", &ledger_path, now).unwrap();
    assert_eq!(first, 3, "all three ledger records must be inserted once");

    let second = sync_ledger(&conn, "agent_alpha", &ledger_path, now).unwrap();
    assert_eq!(second, 0, "a repeat sync must insert nothing new");

    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM health_events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 3, "row count must not double after a second sync");
}

/// A ledger record whose embedded `agent` differs from the state directory's
/// owner, or whose `at` is far in the future, must never be ingested — binds
/// `sync_ledger` to `read_ledger_file_for_agent` rather than the raw reader.
#[test]
fn sync_ledger_ignores_mismatched_agent_and_future_records() {
    let (_d, conn) = db();
    let ledger_dir = tempfile::tempdir().unwrap();
    let ledger_path = ledger_dir.path().join("ledger.jsonl");
    let now = Utc::now();

    let make =
        |agent: &str, at: chrono::DateTime<Utc>| buzz_acp_pkg::reliability::ledger::LedgerRecord {
            at,
            agent: agent.to_string(),
            body: LedgerBody::TurnFinished(buzz_acp_pkg::reliability::ledger::TurnFinished {
                batch_id: Uuid::new_v4(),
                channel_id: Uuid::new_v4(),
                outcome: buzz_acp_pkg::reliability::ledger::TurnOutcome::Ok,
            }),
        };

    let legit = make("agent_alpha", now);
    let intruder = make("intruder_agent", now);
    let future = make("agent_alpha", now + chrono::Duration::days(1));

    let contents = [&legit, &intruder, &future]
        .iter()
        .map(|r| serde_json::to_string(r).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&ledger_path, format!("{contents}\n")).unwrap();

    let inserted = sync_ledger(&conn, "agent_alpha", &ledger_path, now).unwrap();
    assert_eq!(
        inserted, 1,
        "only the matching, non-future record must be ingested"
    );

    let agents: Vec<String> = conn
        .prepare("SELECT DISTINCT agent FROM health_events")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(agents, vec!["agent_alpha".to_string()]);
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

    // agent_2 within window: scope A opens, scope B opens, scope A closes (breaker still open due to scope B)
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
            at: now - 2500,
            kind: "breaker_opened".to_string(),
            event_key: "a2_k2_a".to_string(),
            batch_id: None,
            channel_id: None,
            class: None,
            payload: Some(r#"{"scope":"scopeA"}"#.to_string()),
        },
    )
    .unwrap();

    insert_event(
        &conn,
        &HealthEvent {
            agent: "agent_2".to_string(),
            at: now - 2000,
            kind: "breaker_opened".to_string(),
            event_key: "a2_k2_b".to_string(),
            batch_id: None,
            channel_id: None,
            class: None,
            payload: Some(r#"{"scope":"scopeB"}"#.to_string()),
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
            payload: Some(r#"{"scope":"scopeA"}"#.to_string()),
        },
    )
    .unwrap();

    let summaries = query_agent_health_summary(&conn, Some(window_hours), now).unwrap();
    assert_eq!(summaries.len(), 2);

    let a1 = summaries.iter().find(|s| s.agent == "agent_1").unwrap();
    assert_eq!(a1.turns, 3, "turns within window must be 3 (b1, b2, b3)");
    assert_eq!(a1.failed, 1, "failed turns within window must be 1 (b3)");
    // `parked`/`needs_review` are current outstanding counts across the full
    // 30-day retention, not gated to the requested 24h window: b4/old_b3 and
    // b5/old_b4 are all still-unresolved batches (no replay/discard event),
    // so both the in-window and the older-but-unresolved batch must count.
    assert_eq!(
        a1.parked, 2,
        "parked must count b4 (in window) and old_b3 (older, still unresolved)"
    );
    assert_eq!(
        a1.needs_review, 2,
        "needs_review must count b5 (in window) and old_b4 (older, still unresolved)"
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
    assert!(
        a2.breaker_open,
        "breaker must still be open for agent_2 because scopeB is open"
    );
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
        query_agent_health_events(&conn, "agent_alpha", Some(&kinds), None, Some(50), now).unwrap();
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

    // 4. Bound wall time on >10k rows to assert query does not visit all rows
    {
        let tx = conn.unchecked_transaction().unwrap();
        let mut stmt = tx
            .prepare(
                "INSERT INTO health_events (agent, at, kind, event_key)
                     VALUES (?1, ?2, ?3, ?4)",
            )
            .unwrap();
        for i in 0..10_000 {
            stmt.execute(params![
                "agent_perf",
                now - 10_000 + i,
                "turn_finished",
                format!("perf_{i}")
            ])
            .unwrap();
        }
        drop(stmt);
        tx.commit().unwrap();
    }
    let start = std::time::Instant::now();
    let perf_res =
        query_agent_health_events(&conn, "agent_perf", None, None, Some(5), now).unwrap();
    let elapsed = start.elapsed();
    assert_eq!(perf_res.len(), 5);
    assert!(
        elapsed < std::time::Duration::from_millis(50),
        "query took too long ({elapsed:?}), likely scanned all rows"
    );
}

#[test]
fn invalid_since_hours_is_rejected() {
    let (_d, conn) = db();
    let now = 1_725_600_000i64;
    assert!(query_agent_health_events(&conn, "agent_alpha", None, Some(-1), None, now).is_err());
    assert!(
        query_agent_health_events(&conn, "agent_alpha", None, Some(i64::MAX), None, now).is_err()
    );
    assert!(query_agent_health_summary(&conn, Some(-1), now).is_err());
    assert!(query_agent_health_summary(&conn, Some(i64::MAX), now).is_err());
}

#[test]
fn oversized_or_over_count_kinds_filter_is_rejected() {
    let (_d, conn) = db();
    let now = 1_725_600_000i64;

    let too_many: Vec<String> = (0..(MAX_KIND_FILTER_COUNT + 1))
        .map(|i| format!("kind_{i}"))
        .collect();
    assert!(
        query_agent_health_events(&conn, "agent_alpha", Some(&too_many), None, None, now).is_err(),
        "a kinds vector over the count cap must be rejected"
    );

    let oversized_entry = vec!["k".repeat(MAX_KIND_FILTER_CHARS + 1)];
    assert!(
        query_agent_health_events(
            &conn,
            "agent_alpha",
            Some(&oversized_entry),
            None,
            None,
            now
        )
        .is_err(),
        "a single kind entry over the length cap must be rejected"
    );

    let within_caps = vec!["turn_failed".to_string()];
    assert!(
        query_agent_health_events(&conn, "agent_alpha", Some(&within_caps), None, None, now)
            .is_ok(),
        "a small, valid kinds filter must still succeed"
    );
}

#[test]
fn parked_batches_excerpt_is_cut_to_120_chars_and_carries_no_full_text() {
    let dir = tempfile::tempdir().unwrap();
    let mut park_file = buzz_acp_pkg::reliability::park::ParkFile::open(dir.path()).unwrap();

    let author = nostr::Keys::generate();
    let full_text = "This is a very long message that definitely exceeds one hundred and twenty characters in total length. \
            We want to verify that the excerpt in the parked batch view is strictly capped at 120 characters and does not leak the full message content anywhere in the returned structure.";
    assert!(full_text.chars().count() > 120);

    let event = nostr::EventBuilder::text_note(full_text)
        .sign_with_keys(&author)
        .unwrap();

    let batch_id = Uuid::new_v4();
    let channel_id = Uuid::new_v4();
    let now = Utc::now();
    let parked_batch = buzz_acp_pkg::reliability::park::ParkedBatch {
        batch_id,
        channel_id,
        scope: buzz_acp_pkg::reliability::park::ScopeRef {
            channel_id,
            root_event_id: None,
        },
        reason: buzz_acp_pkg::reliability::park::ParkReason::RetriesExhausted,
        started: true,
        needs_review: true,
        needs_review_reason: Some("retries exhausted".to_string()),
        replayed_at: None,
        forced: false,
        parked_at: now,
        notice_pending: false,
        events: vec![buzz_acp_pkg::reliability::park::ParkedEvent {
            event: event.clone(),
            prompt_tag: "prompt".to_string(),
            received_at: now,
        }],
    };

    park_file.park(parked_batch).unwrap();

    let views = read_parked_batches(dir.path()).unwrap();
    assert_eq!(views.len(), 1);
    let view = &views[0];

    assert_eq!(view.batch_id, batch_id.to_string());
    assert_eq!(view.channel_id, channel_id.to_string());
    assert_eq!(view.reason, "retries_exhausted");
    assert!(view.started);
    assert!(view.needs_review);
    assert_eq!(view.events, 1);
    assert_eq!(view.excerpt.chars().count(), 120);
    assert_eq!(view.excerpt, event.content[..120]);
    assert_ne!(view.excerpt, full_text);

    // Verify that the serialized view carries no full text
    let serialized = serde_json::to_string(view).unwrap();
    assert!(!serialized.contains(full_text));
    assert!(!serialized.contains("content"));

    // Check field names in serialized JSON
    let val: serde_json::Value = serde_json::from_str(&serialized).unwrap();
    assert!(val.get("batchId").is_some());
    assert!(val.get("channelId").is_some());
    assert!(val.get("reason").is_some());
    assert!(val.get("started").is_some());
    assert!(val.get("needsReview").is_some());
    assert!(val.get("parkedAt").is_some());
    assert!(val.get("events").is_some());
    assert!(val.get("excerpt").is_some());
}

#[test]
fn alert_state_records_and_loads_and_prunes() {
    let (_d, conn) = db();
    let now = 1_725_600_000i64;

    let alerts = vec![
        crate::agent_health_alerts::Alert {
            agent: "agent_alpha".to_string(),
            rule: "needs_review".to_string(),
            title: "agent_alpha".to_string(),
            body: "A request needs review".to_string(),
        },
        crate::agent_health_alerts::Alert {
            agent: "agent_alpha".to_string(),
            rule: "breaker_opened".to_string(),
            title: "agent_alpha".to_string(),
            body: "Breaker opened".to_string(),
        },
    ];

    record_alerts(&conn, &alerts, now).unwrap();

    let loaded = load_alert_state(&conn).unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(
        loaded.get(&("agent_alpha".to_string(), "needs_review".to_string())),
        chrono::DateTime::from_timestamp(now, 0).as_ref()
    );
    assert_eq!(
        loaded.get(&("agent_alpha".to_string(), "breaker_opened".to_string())),
        chrono::DateTime::from_timestamp(now, 0).as_ref()
    );

    // Updating timestamp on conflict
    let later = now + 100;
    record_alerts(&conn, &[alerts[0].clone()], later).unwrap();
    let updated = load_alert_state(&conn).unwrap();
    assert_eq!(updated.len(), 2);
    assert_eq!(
        updated.get(&("agent_alpha".to_string(), "needs_review".to_string())),
        chrono::DateTime::from_timestamp(later, 0).as_ref()
    );

    // Test pruning: entries older than RETENTION_SECS are pruned
    prune(&conn, later + RETENTION_SECS + 1).unwrap();
    let after_prune = load_alert_state(&conn).unwrap();
    assert_eq!(after_prune.len(), 0);
}

#[test]
fn needs_review_cleared_by_replayed_or_discarded() {
    let (_d, conn) = db();
    let now = 1_725_600_000i64;
    let batch_id = "batch-xyz";

    insert_event(
        &conn,
        &HealthEvent {
            agent: "agent_alpha".to_string(),
            at: now - 500,
            kind: "batch_needs_review".to_string(),
            event_key: "k_nr".to_string(),
            batch_id: Some(batch_id.to_string()),
            channel_id: None,
            class: None,
            payload: None,
        },
    )
    .unwrap();

    let summaries = query_agent_health_summary(&conn, Some(24), now).unwrap();
    let s = summaries.iter().find(|s| s.agent == "agent_alpha").unwrap();
    assert_eq!(s.needs_review, 1);

    insert_event(
        &conn,
        &HealthEvent {
            agent: "agent_alpha".to_string(),
            at: now - 100,
            kind: "batch_replayed".to_string(),
            event_key: "k_rep".to_string(),
            batch_id: Some(batch_id.to_string()),
            channel_id: None,
            class: None,
            payload: None,
        },
    )
    .unwrap();

    let summaries_after = query_agent_health_summary(&conn, Some(24), now).unwrap();
    let s_after = summaries_after
        .iter()
        .find(|s| s.agent == "agent_alpha")
        .unwrap();
    assert_eq!(
        s_after.needs_review, 0,
        "needs_review must be 0 after batch_replayed"
    );
}

/// `parked` must be the count of batches CURRENTLY sitting parked (keyed by
/// batch id, resolved by `batch_replayed`/`batch_discarded`), not a raw
/// count of `batch_parked` events inside the caller's requested window. A
/// batch parked 8 days ago with no resolution is still outstanding even when
/// the caller asks for the last 24 hours; a batch resolved inside the window
/// must stop counting immediately.
#[test]
fn parked_counts_current_outstanding_batches_not_windowed_events() {
    let (_d, conn) = db();
    let now = 1_725_600_000i64;
    let eight_days_ago = now - 8 * 24 * 3600;
    let old_unresolved = "old-unresolved-batch";
    let old_resolved = "old-resolved-batch";

    insert_event(
        &conn,
        &HealthEvent {
            agent: "agent_alpha".to_string(),
            at: eight_days_ago,
            kind: "batch_parked".to_string(),
            event_key: "k_old_unresolved".to_string(),
            batch_id: Some(old_unresolved.to_string()),
            channel_id: None,
            class: None,
            payload: None,
        },
    )
    .unwrap();
    insert_event(
        &conn,
        &HealthEvent {
            agent: "agent_alpha".to_string(),
            at: eight_days_ago,
            kind: "batch_parked".to_string(),
            event_key: "k_old_resolved".to_string(),
            batch_id: Some(old_resolved.to_string()),
            channel_id: None,
            class: None,
            payload: None,
        },
    )
    .unwrap();
    insert_event(
        &conn,
        &HealthEvent {
            agent: "agent_alpha".to_string(),
            at: eight_days_ago + 100,
            kind: "batch_discarded".to_string(),
            event_key: "k_old_discard".to_string(),
            batch_id: Some(old_resolved.to_string()),
            channel_id: None,
            class: None,
            payload: None,
        },
    )
    .unwrap();

    // Requested window is 24h; both park events happened 8 days ago.
    let summaries = query_agent_health_summary(&conn, Some(24), now).unwrap();
    let s = summaries.iter().find(|s| s.agent == "agent_alpha").unwrap();
    assert_eq!(
        s.parked, 1,
        "the still-unresolved 8-day-old batch must count even outside the requested window; \
         the discarded one must not"
    );
}
