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

#[test]
fn agent_paused_cleared_by_agent_resumed() {
    let (_d, conn) = db();
    let now = 1_725_600_000i64;

    insert_event(
        &conn,
        &HealthEvent {
            agent: "agent_alpha".to_string(),
            at: now - 500,
            kind: "agent_paused".to_string(),
            event_key: "k_pause".to_string(),
            batch_id: None,
            channel_id: None,
            class: Some("capacity_exhausted".to_string()),
            payload: Some(r#"{"until":"2026-09-08T00:00:00Z"}"#.to_string()),
        },
    )
    .unwrap();

    let s1 = query_agent_health_summary(&conn, Some(24), now).unwrap();
    assert_eq!(
        s1[0].latest_paused_until.as_deref(),
        Some("2026-09-08T00:00:00Z")
    );

    insert_event(
        &conn,
        &HealthEvent {
            agent: "agent_alpha".to_string(),
            at: now - 100,
            kind: "agent_resumed".to_string(),
            event_key: "k_resume".to_string(),
            batch_id: None,
            channel_id: None,
            class: None,
            payload: None,
        },
    )
    .unwrap();

    let s2 = query_agent_health_summary(&conn, Some(24), now).unwrap();
    assert_eq!(
        s2[0].latest_paused_until, None,
        "agent_resumed must clear latest_paused_until"
    );
}

#[test]
fn breaker_open_persists_outside_counters_window() {
    let (_d, conn) = db();
    let now = 1_725_600_000i64;
    let eight_days_ago = now - 8 * 24 * 3600;

    insert_event(
        &conn,
        &HealthEvent {
            agent: "agent_alpha".to_string(),
            at: eight_days_ago,
            kind: "breaker_opened".to_string(),
            event_key: "k_bo".to_string(),
            batch_id: None,
            channel_id: None,
            class: None,
            payload: Some(r#"{"scope":"scope_x"}"#.to_string()),
        },
    )
    .unwrap();

    // 24h summary
    let s24 = query_agent_health_summary(&conn, Some(24), now).unwrap();
    let a = s24.iter().find(|s| s.agent == "agent_alpha").unwrap();
    assert!(
        a.breaker_open,
        "breaker_opened 8 days ago must still report breaker_open in 24h summary"
    );

    // 7d summary
    let s7d = query_agent_health_summary(&conn, Some(7 * 24), now).unwrap();
    let a7 = s7d.iter().find(|s| s.agent == "agent_alpha").unwrap();
    assert!(
        a7.breaker_open,
        "breaker_opened 8 days ago must still report breaker_open in 7d summary"
    );
}

#[test]
fn corrupted_payload_marks_degraded() {
    let (_d, conn) = db();
    let now = 1_725_600_000i64;

    insert_event(
        &conn,
        &HealthEvent {
            agent: "agent_alpha".to_string(),
            at: now - 500,
            kind: "agent_paused".to_string(),
            event_key: "k_corrupted".to_string(),
            batch_id: None,
            channel_id: None,
            class: None,
            payload: Some("not json".to_string()),
        },
    )
    .unwrap();

    let s = query_agent_health_summary(&conn, Some(24), now).unwrap();
    assert_ne!(
        s[0].latest_paused_until, None,
        "corrupted payload must not silently report latest_paused_until as None"
    );
    assert!(
        s[0].latest_paused_until
            .as_ref()
            .unwrap()
            .contains("degraded")
            || s[0]
                .latest_paused_until
                .as_ref()
                .unwrap()
                .contains("corrupt"),
        "distinguishable degraded marker expected"
    );
}

/// A corrupt `breaker_opened` payload must not silently default to scope ""
/// — it must still read as an open breaker (fail safe, not fail healthy).
/// A corrupt `breaker_closed` payload must not guess scope "" and close
/// whatever legitimate breaker happens to be tracked under it.
#[test]
fn corrupted_breaker_payload_fails_safe_not_silently_healthy() {
    let (_d, conn) = db();
    let now = 1_725_600_000i64;

    // A real breaker is open under a real scope.
    insert_event(
        &conn,
        &HealthEvent {
            agent: "agent_alpha".to_string(),
            at: now - 500,
            kind: "breaker_opened".to_string(),
            event_key: "k_real_open".to_string(),
            batch_id: None,
            channel_id: None,
            class: None,
            payload: Some(r#"{"scope":"real-scope","consecutive":3}"#.to_string()),
        },
    )
    .unwrap();

    // A corrupted breaker_closed arrives (unparseable JSON) — must not be
    // able to guess-close the real breaker above.
    insert_event(
        &conn,
        &HealthEvent {
            agent: "agent_alpha".to_string(),
            at: now - 400,
            kind: "breaker_closed".to_string(),
            event_key: "k_corrupt_close".to_string(),
            batch_id: None,
            channel_id: None,
            class: None,
            payload: Some("not json".to_string()),
        },
    )
    .unwrap();

    let s = query_agent_health_summary(&conn, Some(24), now).unwrap();
    assert!(
        s[0].breaker_open,
        "the real, unrelated breaker must still read open after a corrupt close"
    );

    // Separately: a corrupted breaker_opened (missing scope) for a fresh
    // agent must still surface as an open breaker, not a silently healthy one.
    insert_event(
        &conn,
        &HealthEvent {
            agent: "agent_beta".to_string(),
            at: now - 500,
            kind: "breaker_opened".to_string(),
            event_key: "k_corrupt_open".to_string(),
            batch_id: None,
            channel_id: None,
            class: None,
            payload: Some(r#"{"consecutive":3}"#.to_string()),
        },
    )
    .unwrap();
    let s2 = query_agent_health_summary(&conn, Some(24), now).unwrap();
    let beta = s2.iter().find(|c| c.agent == "agent_beta").unwrap();
    assert!(
        beta.breaker_open,
        "a corrupt breaker_opened payload (missing scope) must fail open, not silently healthy"
    );
}

#[test]
fn duplicate_events_with_different_rfc3339_timezone_notations_deduplicate() {
    let (_d, conn) = db();
    let agent = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let at_z = "2026-09-06T15:00:00Z";
    let at_offset = "2026-09-06T15:00:00+00:00";

    let event1 = HealthEvent {
        agent: agent.to_string(),
        at: 1788706800,
        kind: "turn_finished".to_string(),
        event_key: compute_event_key(at_z, "turn_finished", Some("b1")),
        batch_id: Some("b1".to_string()),
        channel_id: None,
        class: None,
        payload: None,
    };

    let event2 = HealthEvent {
        agent: agent.to_string(),
        at: 1788706800,
        kind: "turn_finished".to_string(),
        event_key: compute_event_key(at_offset, "turn_finished", Some("b1")),
        batch_id: Some("b1".to_string()),
        channel_id: None,
        class: None,
        payload: None,
    };

    assert_eq!(event1.event_key, event2.event_key);
    assert!(insert_event(&conn, &event1).unwrap());
    assert!(
        !insert_event(&conn, &event2).unwrap(),
        "second insert with equivalent offset must be ignored"
    );

    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM health_events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1, "exactly one row must exist");
}

#[test]
fn distinct_subsecond_events_do_not_collapse() {
    let (_d, conn) = db();
    let agent = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    // Same agent, kind, and target, same UTC second, different milliseconds:
    // two real, distinct breaker_opened events for the same scope (a flap)
    // must both persist, not collapse onto one `event_key` via
    // whole-second truncation.
    let at_a = "2026-09-06T15:00:00.100Z";
    let at_b = "2026-09-06T15:00:00.900Z";

    let event_a = HealthEvent {
        agent: agent.to_string(),
        at: 1788706800,
        kind: "breaker_opened".to_string(),
        event_key: compute_event_key(at_a, "breaker_opened", Some("scope-1")),
        batch_id: None,
        channel_id: None,
        class: None,
        payload: None,
    };
    let event_b = HealthEvent {
        agent: agent.to_string(),
        at: 1788706800,
        kind: "breaker_opened".to_string(),
        event_key: compute_event_key(at_b, "breaker_opened", Some("scope-1")),
        batch_id: None,
        channel_id: None,
        class: None,
        payload: None,
    };

    assert_ne!(
        event_a.event_key, event_b.event_key,
        "distinct sub-second instants must produce distinct event keys"
    );
    assert!(insert_event(&conn, &event_a).unwrap());
    assert!(
        insert_event(&conn, &event_b).unwrap(),
        "second distinct sub-second event must not be ignored as a duplicate"
    );

    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM health_events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 2, "both distinct sub-second events must persist");
}

#[test]
fn ingest_rejects_unknown_kind_oversized_class_and_invalid_agent() {
    let unknown_kind = serde_json::json!({
        "at": "2026-09-06T15:00:00Z",
        "kind": "unknown_random_kind",
    });
    let frame: Result<HealthFrame, _> = serde_json::from_value(unknown_kind);
    if let Ok(f) = frame {
        assert!(frame_to_health_event(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            &f
        )
        .is_err());
    }

    let oversized_class = serde_json::json!({
        "at": "2026-09-06T15:00:00Z",
        "kind": "turn_failed",
        "class": "x".repeat(300),
    });
    let f: HealthFrame = serde_json::from_value(oversized_class).unwrap();
    assert!(frame_to_health_event(
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        &f
    )
    .is_err());

    assert!(validate_hex64("not-a-hex-64-string").is_err());
    assert!(
        validate_hex64("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef").is_ok()
    );
}

/// A frame's `payload` has no other length or depth bound — a syntactically
/// valid, known-kind frame with an arbitrarily large payload must still be
/// rejected before it reaches SQLite.
#[test]
fn oversized_payload_frame_is_rejected() {
    let huge_payload = serde_json::json!({
        "at": "2026-09-06T15:00:00Z",
        "kind": "turn_failed",
        "payload": { "note": "x".repeat(MAX_PAYLOAD_CHARS) },
    });
    let f: HealthFrame = serde_json::from_value(huge_payload).unwrap();
    assert!(frame_to_health_event(
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        &f
    )
    .is_err());

    let small_payload = serde_json::json!({
        "at": "2026-09-06T15:00:00Z",
        "kind": "turn_failed",
        "payload": { "note": "fine" },
    });
    let f2: HealthFrame = serde_json::from_value(small_payload).unwrap();
    assert!(frame_to_health_event(
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        &f2
    )
    .is_ok());
}

#[test]
fn future_timestamp_frame_is_rejected() {
    let one_year_future = chrono::Utc::now() + chrono::Duration::days(365);
    let frame = HealthFrame {
        at: one_year_future.to_rfc3339(),
        kind: "turn_failed".to_string(),
        batch_id: None,
        channel_id: None,
        class: None,
        payload: None,
    };
    assert!(frame_to_health_event(
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        &frame
    )
    .is_err());
}

#[test]
fn prune_enforces_row_and_byte_budget_oldest_first() {
    let (_d, conn) = db();
    let now = 1_725_600_000i64;

    // Insert 1050 events (budget is 1000)
    {
        let tx = conn.unchecked_transaction().unwrap();
        let mut stmt = tx
            .prepare(
                "INSERT INTO health_events (agent, at, kind, event_key)
                     VALUES (?1, ?2, ?3, ?4)",
            )
            .unwrap();
        for i in 0..1050 {
            stmt.execute(params![
                "agent_budget",
                now - 2000 + i,
                "turn_finished",
                format!("budget_{i}")
            ])
            .unwrap();
        }
        drop(stmt);
        tx.commit().unwrap();
    }

    let count_before: i64 = conn
        .query_row("SELECT COUNT(*) FROM health_events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count_before, 1050);

    let pruned = prune(&conn, now).unwrap();
    assert_eq!(pruned, 50, "50 excess rows must be pruned");

    let count_after: i64 = conn
        .query_row("SELECT COUNT(*) FROM health_events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count_after, 1000, "row count must be capped to 1000");

    // The 50 oldest rows (i = 0..50, at = now - 2000 .. now - 1950) should be gone
    let oldest_remaining_at: i64 = conn
        .query_row(
            "SELECT MIN(at) FROM health_events WHERE agent = 'agent_budget'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        oldest_remaining_at,
        now - 2000 + 50,
        "oldest rows must be pruned first"
    );
}

#[test]
fn read_parked_batches_distinguishes_missing_from_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let non_existent = dir.path().join("does_not_exist");
    // Missing directory returns Ok(empty vec)
    let res = read_parked_batches(&non_existent).unwrap();
    assert!(res.is_empty(), "missing dir should return empty vec");

    // Existing directory without parked.jsonl returns Ok(empty vec)
    let empty_dir = dir.path().join("empty_dir");
    std::fs::create_dir_all(&empty_dir).unwrap();
    let res2 = read_parked_batches(&empty_dir).unwrap();
    assert!(
        res2.is_empty(),
        "dir without parked.jsonl should return empty vec"
    );

    // Existing directory with corrupt parked.jsonl returns Err
    let corrupt_dir = dir.path().join("corrupt_dir");
    std::fs::create_dir_all(&corrupt_dir).unwrap();
    let park_file = corrupt_dir.join(buzz_acp_pkg::reliability::park::PARK_FILE);
    std::fs::write(&park_file, "{corrupt json\n").unwrap();
    let err = read_parked_batches(&corrupt_dir);
    assert!(err.is_err(), "corrupt parked.jsonl must return Err");
}

fn minimal_parked_batch_line(batch_id: uuid::Uuid) -> String {
    let channel_id = uuid::Uuid::new_v4();
    let batch = buzz_acp_pkg::reliability::park::ParkedBatch {
        batch_id,
        channel_id,
        scope: buzz_acp_pkg::reliability::park::ScopeRef {
            channel_id,
            root_event_id: None,
        },
        reason: buzz_acp_pkg::reliability::park::ParkReason::RetriesExhausted,
        started: true,
        needs_review: false,
        needs_review_reason: None,
        replayed_at: None,
        forced: false,
        parked_at: chrono::Utc::now(),
        events: Vec::new(),
    };
    serde_json::to_string(&batch).unwrap()
}

/// A single park-file line over the bounded reader's line-size cap must be
/// rejected, not buffered whole — binds `read_parked_batches` to the same
/// `MAX_LINE_BYTES` cap the harness's own park reader enforces.
#[test]
fn read_parked_batches_rejects_line_over_max_line_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let park_file = dir.path().join(buzz_acp_pkg::reliability::park::PARK_FILE);
    let oversized_line = "x".repeat(buzz_acp_pkg::reliability::park::MAX_LINE_BYTES + 1);
    std::fs::write(&park_file, format!("{oversized_line}\n")).unwrap();

    let err = read_parked_batches(dir.path());
    assert!(err.is_err(), "a line over MAX_LINE_BYTES must be rejected");
}

/// More records than the bounded reader's total-count cap must be rejected
/// rather than read in full — binds `read_parked_batches` to the same
/// `MAX_PARKED_TOTAL` cap the harness's own park reader enforces.
#[test]
fn read_parked_batches_rejects_more_than_max_parked_total() {
    let dir = tempfile::tempdir().unwrap();
    let park_file = dir.path().join(buzz_acp_pkg::reliability::park::PARK_FILE);
    let total = buzz_acp_pkg::reliability::park::MAX_PARKED_TOTAL + 1;
    let mut contents = String::new();
    for _ in 0..total {
        contents.push_str(&minimal_parked_batch_line(uuid::Uuid::new_v4()));
        contents.push('\n');
    }
    std::fs::write(&park_file, contents).unwrap();

    let err = read_parked_batches(dir.path());
    assert!(
        err.is_err(),
        "more than MAX_PARKED_TOTAL records must be rejected, not silently read in full"
    );
}

/// Exactly the cap, and one under it, must both still read successfully —
/// the boundary itself must not be off-by-one in the strict direction.
#[test]
fn read_parked_batches_accepts_exactly_max_parked_total() {
    let dir = tempfile::tempdir().unwrap();
    let park_file = dir.path().join(buzz_acp_pkg::reliability::park::PARK_FILE);
    let total = buzz_acp_pkg::reliability::park::MAX_PARKED_TOTAL;
    let mut contents = String::new();
    for _ in 0..total {
        contents.push_str(&minimal_parked_batch_line(uuid::Uuid::new_v4()));
        contents.push('\n');
    }
    std::fs::write(&park_file, contents).unwrap();

    let views = read_parked_batches(dir.path()).expect("exactly the cap must still succeed");
    assert_eq!(views.len(), total);
}

#[test]
fn health_ingest_result_serde_errors() {
    let empty_result = HealthIngestResult {
        inserted: 1,
        alerts: vec![],
        errors: vec![],
    };
    let empty_json = serde_json::to_string(&empty_result).unwrap();
    assert!(
        !empty_json.contains("errors"),
        "errors should be skipped when empty: {empty_json}"
    );

    let with_errors = HealthIngestResult {
        inserted: 0,
        alerts: vec![],
        errors: vec![("agent_foo".into(), "failed to read ledger".into())],
    };
    let with_errors_json = serde_json::to_string(&with_errors).unwrap();
    assert!(
        with_errors_json.contains("errors"),
        "errors must be serialized when present: {with_errors_json}"
    );

    let deserialized: HealthIngestResult = serde_json::from_str(&empty_json).unwrap();
    assert!(deserialized.errors.is_empty());
}

#[test]
fn in_flight_sync_guard_coalesces_concurrent_calls() {
    let in_flight = Arc::new(Mutex::new(HashSet::new()));
    let dirty = Arc::new(Mutex::new(HashSet::new()));
    let key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string();

    // First attempt claims the slot and starts a run.
    assert!(matches!(
        claim_sync_slot(&in_flight, &dirty, &key),
        SyncClaim::Start
    ));
    let guard = InFlightGuard {
        in_flight: Arc::clone(&in_flight),
        dirty: Arc::clone(&dirty),
        key: key.clone(),
    };

    // Concurrent attempt while the run is active is coalesced, not started.
    assert!(matches!(
        claim_sync_slot(&in_flight, &dirty, &key),
        SyncClaim::AlreadyRunning
    ));

    // With no further request, finishing the run releases the slot.
    dirty.lock().unwrap().remove(&key); // simulate no dirty mark this time
    assert!(!finish_sync_or_rerun(&in_flight, &dirty, &key));
    assert!(!in_flight.lock().unwrap().contains(&key));
    drop(guard);

    // A fresh attempt can claim the slot again.
    assert!(matches!(
        claim_sync_slot(&in_flight, &dirty, &key),
        SyncClaim::Start
    ));
}

/// This is the exact concurrency defect T17 delta round 2 flagged: a
/// `sync_for_agent` request arriving while a sync is already in flight for
/// the same key must not be silently dropped — it must cause one more run
/// after the active one finishes. Binds `claim_sync_slot` +
/// `finish_sync_or_rerun`, the production functions `sync_for_agent` itself
/// calls with no other coordination logic of its own — bypassing them (e.g.
/// manipulating the `HashSet`s directly, as the pre-delta version of this
/// test did) would not exercise this guarantee at all.
#[test]
fn late_request_during_active_sync_causes_a_rerun_not_a_drop() {
    let in_flight = Arc::new(Mutex::new(HashSet::new()));
    let dirty = Arc::new(Mutex::new(HashSet::new()));
    let key = "agent-under-test".to_string();

    // The first request starts a run.
    assert!(matches!(
        claim_sync_slot(&in_flight, &dirty, &key),
        SyncClaim::Start
    ));

    // While that run is still active, a second request for the same key
    // arrives (e.g. a new ledger event landed mid-sync).
    assert!(matches!(
        claim_sync_slot(&in_flight, &dirty, &key),
        SyncClaim::AlreadyRunning
    ));

    // The active run finishes: it must be told to run again, not exit —
    // the late event must still be picked up.
    assert!(
        finish_sync_or_rerun(&in_flight, &dirty, &key),
        "a request received during the active run must cause a rerun"
    );
    assert!(
        in_flight.lock().unwrap().contains(&key),
        "the slot must remain held across the rerun"
    );

    // The rerun completes with no further requests: now it releases.
    assert!(!finish_sync_or_rerun(&in_flight, &dirty, &key));
    assert!(!in_flight.lock().unwrap().contains(&key));
    assert!(!dirty.lock().unwrap().contains(&key));
}

#[test]
fn record_alerts_transactional_failure_rolls_back_all() {
    let (_d, conn) = db();
    let now = 1_725_600_000i64;
    let alert1 = crate::agent_health_alerts::Alert {
        agent: "agent_tx_test".to_string(),
        rule: "repeated_failure".to_string(),
        title: "T1".to_string(),
        body: "B1".to_string(),
    };
    let alert2 = crate::agent_health_alerts::Alert {
        agent: "agent_tx_test".to_string(),
        rule: "fail_trigger".to_string(),
        title: "T2".to_string(),
        body: "B2".to_string(),
    };

    // Create a trigger that aborts on rule = 'fail_trigger'
    conn.execute(
        "CREATE TRIGGER abort_fail_trigger BEFORE INSERT ON alert_state
             WHEN NEW.rule = 'fail_trigger'
             BEGIN
                 SELECT RAISE(ABORT, 'injected transaction failure');
             END;",
        [],
    )
    .unwrap();

    let result = record_alerts(&conn, &[alert1, alert2], now);
    assert!(result.is_err(), "second alert should trigger failure");

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM alert_state WHERE agent = 'agent_tx_test'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 0,
        "transaction must roll back completely on failure: zero rows committed"
    );
}

#[test]
fn filter_valid_alert_acks_drops_forged_agent_and_forged_rule() {
    let real_agent = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let known = vec![managed_record(real_agent)];

    let legit = crate::agent_health_alerts::Alert {
        agent: real_agent.to_string(),
        rule: crate::agent_health_alerts::RULE_BREAKER_OPENED.to_string(),
        title: "Breaker opened".to_string(),
        body: "body".to_string(),
    };
    let forged_agent = crate::agent_health_alerts::Alert {
        agent: "intruder_not_hex64".to_string(),
        rule: crate::agent_health_alerts::RULE_BREAKER_OPENED.to_string(),
        title: "Forged".to_string(),
        body: "body".to_string(),
    };
    let unmanaged_agent = crate::agent_health_alerts::Alert {
        agent: "fedcba9876543210fedcba9876543210fedcba9876543210fedcba987654321f".to_string(),
        rule: crate::agent_health_alerts::RULE_BREAKER_OPENED.to_string(),
        title: "Not mine".to_string(),
        body: "body".to_string(),
    };
    let forged_rule = crate::agent_health_alerts::Alert {
        agent: real_agent.to_string(),
        rule: "made_up_rule".to_string(),
        title: "Forged rule".to_string(),
        body: "body".to_string(),
    };

    let valid = filter_valid_alert_acks(
        &known,
        vec![legit.clone(), forged_agent, unmanaged_agent, forged_rule],
    );

    assert_eq!(
        valid,
        vec![legit],
        "only the real agent + known rule combination must survive"
    );
}

#[test]
fn record_delivered_alerts_rejects_over_count_acks() {
    let alerts: Vec<crate::agent_health_alerts::Alert> = (0..(MAX_ALERTS_PER_ACK + 1))
        .map(|i| crate::agent_health_alerts::Alert {
            agent: format!("agent_{i}"),
            rule: crate::agent_health_alerts::RULE_BREAKER_OPENED.to_string(),
            title: "t".to_string(),
            body: "b".to_string(),
        })
        .collect();
    assert!(validate_alert_ack_count(alerts.len()).is_err());
    assert!(validate_alert_ack_count(MAX_ALERTS_PER_ACK).is_ok());
}

#[test]
fn sanitize_last_error_redacts_bearer_and_api_key() {
    let raw = "failed to connect: Bearer sk-ant-api03-abcdef123456789 and api_key=secret_123456789";
    let sanitized = sanitize_last_error(raw, &[]);
    assert!(
        !sanitized.contains("sk-ant-api03-abcdef123456789"),
        "sk token must be redacted: {sanitized}"
    );
    assert!(
        !sanitized.contains("secret_123456789"),
        "api_key must be redacted: {sanitized}"
    );
    assert!(
        sanitized.contains("[REDACTED]"),
        "redaction marker must be present"
    );
}

/// A secret that does not match any built-in shape (not "Bearer ...", not a
/// known API-key or GitHub-token prefix) — e.g. a custom provider key this
/// agent's own `env_vars` holds, or its nsec — still leaks through the
/// pattern-only redaction. Callers must pass every literal secret value the
/// agent's own configuration holds as `known_secrets`.
#[test]
fn sanitize_last_error_redacts_known_secrets_with_no_recognizable_shape() {
    let custom_secret = "correct-horse-battery-staple-9f8e7d6c";
    let raw = format!("provider rejected credential {custom_secret} for host example.com");

    let unsanitized = sanitize_last_error(&raw, &[]);
    assert!(
        unsanitized.contains(custom_secret),
        "sanity check: a shapeless secret is NOT caught by built-in patterns alone"
    );

    let sanitized = sanitize_last_error(&raw, &[custom_secret]);
    assert!(
        !sanitized.contains(custom_secret),
        "a known configured secret must be redacted even with no recognizable shape: {sanitized}"
    );
    assert!(sanitized.contains("[REDACTED]"));
}

/// A full-sync call (`targeted_agent: None`) has no other source of truth
/// for which agents exist, so an unreadable/corrupt managed-agents store
/// must be a hard error — never collapse into `Ok((vec![], vec![]))`, which
/// `sync_agent_health` would then report as a clean `HealthIngestResult { inserted: 0, .. }`,
/// indistinguishable from "no agents configured yet." Binds the exact
/// decision `sync_agent_health` calls (not a duplicated copy): reverting
/// `sync_agent_health` to call `.unwrap_or_default()` again instead of this
/// helper does not touch this test's pass/fail, but reintroducing the old
/// silent-success bug inside `resolve_managed_agents_for_sync` itself does.
#[test]
fn resolve_managed_agents_for_sync_errors_hard_on_full_sync() {
    let err = resolve_managed_agents_for_sync(Err("disk full".to_string()), None);
    assert!(
        err.is_err(),
        "a broken store with no targeted agent must be a hard error, not Ok(0)"
    );
}

/// A single-agent sync already knows which agent it's syncing, so a broken
/// store degrades to "no local agents known" plus a recorded error for that
/// agent, rather than aborting the whole command.
#[test]
fn resolve_managed_agents_for_sync_degrades_on_targeted_sync() {
    let (agents, errors) =
        resolve_managed_agents_for_sync(Err("disk full".to_string()), Some("agent_x"))
            .expect("a targeted sync must not hard-fail on a broken store");
    assert!(agents.is_empty());
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, "agent_x");
    assert!(errors[0].1.contains("disk full"));
}

/// The healthy path is untouched: a successful load passes the agents
/// through with no errors recorded.
#[test]
fn resolve_managed_agents_for_sync_passes_through_on_success() {
    let (agents, errors) = resolve_managed_agents_for_sync(Ok(Vec::new()), None).unwrap();
    assert!(agents.is_empty());
    assert!(errors.is_empty());
}

fn managed_record(pubkey: &str) -> ManagedAgentRecord {
    serde_json::from_str(&format!(
        r#"{{
            "pubkey": "{pubkey}",
            "name": "Test Agent",
            "relay_url": "wss://localhost:3000",
            "acp_command": "buzz-acp",
            "agent_command": "goose",
            "agent_args": [],
            "mcp_command": "",
            "turn_timeout_seconds": 320,
            "system_prompt": "You are a test agent.",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "last_started_at": null,
            "last_stopped_at": null,
            "last_exit_code": null,
            "last_error": null
        }}"#
    ))
    .unwrap()
}

/// `managed_agent_state_dir` unconditionally creates a directory for
/// whatever id it's given, so every call site gates on
/// `agent_may_have_local_state` first. This is the exact gate all 3 new T17
/// call sites (`sync_agent_health`, `ingest_agent_health_frame`,
/// `get_parked_batches`) use — binds that shared decision directly.
#[test]
fn agent_may_have_local_state_requires_hex64_and_membership() {
    let member = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let stranger = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
    let known = vec![managed_record(member)];

    assert!(
        agent_may_have_local_state(&known, member),
        "a hex64 id that is a member of the roster may have local state"
    );
    assert!(
        !agent_may_have_local_state(&known, stranger),
        "a hex64 id that is NOT a member of the roster must not have local state"
    );
    assert!(
        !agent_may_have_local_state(&known, "not-hex-at-all"),
        "a malformed id must never pass, member or not"
    );
    assert!(
        !agent_may_have_local_state(&[], member),
        "an empty (or unloadable) roster admits nobody"
    );
}
