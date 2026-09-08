//! Second half of the agent-health tests: pause/breaker state, ingest
//! validation, storage budgets, parked-batch reads and alert acknowledgements.

use super::acks::validate_alert_ack_count;
use super::tests::db;
use super::*;

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
        notice_pending: false,
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
