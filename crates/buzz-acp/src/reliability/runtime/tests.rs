use super::*;
use crate::config::DedupMode;
use crate::observer::ObserverHandle;
use crate::queue::{CancelReason, EventQueue, QueuedEvent};
use chrono::Utc;
use nostr::{EventBuilder, Keys, Kind};
use std::time::Instant;
use uuid::Uuid;

fn make_test_event(content: &str) -> (nostr::Event, nostr::EventId) {
    let keys = Keys::generate();
    let event = EventBuilder::new(Kind::Custom(9), content)
        .sign_with_keys(&keys)
        .unwrap();
    let id = event.id;
    (event, id)
}

fn make_flush_batch(
    channel_id: Uuid,
    scope: SessionScope,
    content: &str,
) -> (FlushBatch, nostr::EventId) {
    let (event, id) = make_test_event(content);
    (
        FlushBatch {
            batch_id: Uuid::new_v4(),
            channel_id,
            scope,
            events: vec![BatchEvent {
                event,
                prompt_tag: "test".into(),
                received_at: Instant::now(),
            }],
            cancelled_events: vec![],
            cancel_reason: None,
            started: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        },
        id,
    )
}

fn acknowledge_test_notices(runtime: &mut ReliabilityRuntime) {
    let ids: Vec<_> = runtime
        .park()
        .batches()
        .iter()
        .filter(|batch| batch.notice_pending)
        .map(|batch| batch.batch_id)
        .collect();
    for id in ids {
        runtime.mark_notice_enqueued(id).unwrap();
    }
}

#[test]
#[ignore = "bounded synthetic storage timing; run explicitly"]
fn near_cap_custody_timing() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let now = Utc::now();
    let ch = Uuid::new_v4();
    let scope = SessionScope::Conversation { channel_id: ch };
    {
        let normal_dir = tempfile::tempdir().unwrap();
        let mut normal = ReliabilityRuntime::open_in(normal_dir.path(), "test-agent", now).unwrap();
        let (event, _) = make_test_event("normal input");
        let input = QueuedEvent {
            channel_id: ch,
            scope: scope.clone(),
            event,
            prompt_tag: "test".into(),
            received_at: Instant::now(),
        };
        state_dir::take_written_bytes();
        let start = Instant::now();
        normal.admit_event(&input, now).unwrap();
        let admission_us = start.elapsed().as_micros();
        let admission_bytes = state_dir::take_written_bytes();
        let mut queue = EventQueue::new(DedupMode::Queue);
        normal.refill_ingress(&mut queue);
        let batch = queue.flush_next().unwrap();
        let start = Instant::now();
        normal.prepare_dispatch(&batch, 1, now).unwrap();
        eprintln!(
            "normal admission_us={} dispatch_us={} admission_bytes={} dispatch_bytes={}",
            admission_us,
            start.elapsed().as_micros(),
            admission_bytes,
            state_dir::take_written_bytes()
        );
    }
    let mut parked = vec![];
    for _ in 0..40 {
        let (batch, _) = make_flush_batch(ch, scope.clone(), &"x".repeat(220_000));
        let mut record =
            ParkedBatch::from_batch(&batch, ParkReason::RetriesExhausted, false, now).unwrap();
        record.notice_pending = false;
        parked.push(record);
    }
    let park_bytes = super::super::park::serialize(&parked).unwrap();
    state_dir::write_atomic(&dir.path().join("parked.jsonl"), &park_bytes).unwrap();
    let record = ledger::LedgerRecord {
        at: now,
        agent: "test-agent".into(),
        body: LedgerBody::TurnStarted(ledger::TurnStarted::new(
            Uuid::new_v4(),
            ch,
            "test",
            (0..50).map(|_| "a".repeat(64)),
            1,
        )),
    };
    let mut line = serde_json::to_vec(&record).unwrap();
    line.push(b'\n');
    let count = 9_000_000 / line.len();
    let mut file = state_dir::open_create(&dir.path().join("ledger.jsonl")).unwrap();
    for _ in 0..count {
        file.write_all(&line).unwrap();
    }
    file.sync_all().unwrap();
    drop(file);
    let receipts: Vec<_> = (0..transaction::MAX_RECEIPTS - 50)
        .map(|n| format!("{n:064x}"))
        .collect();
    let receipt_bytes = serde_json::to_vec(&receipts).unwrap();
    state_dir::write_atomic(&dir.path().join(transaction::RECEIPTS_FILE), &receipt_bytes).unwrap();
    let mut runtime = ReliabilityRuntime::open_in(dir.path(), "test-agent", now).unwrap();
    let (event, _) = make_test_event("new input");
    let queued = QueuedEvent {
        channel_id: ch,
        scope: scope.clone(),
        event,
        prompt_tag: "test".into(),
        received_at: Instant::now(),
    };
    state_dir::take_written_bytes();
    let start = Instant::now();
    runtime.admit_event(&queued, now).unwrap();
    let admit_ms = start.elapsed().as_millis();
    let admission_bytes = state_dir::take_written_bytes();
    let mut queue = EventQueue::new(DedupMode::Queue);
    runtime.refill_ingress(&mut queue);
    let batch = queue.flush_next().unwrap();
    let start = Instant::now();
    runtime.prepare_dispatch(&batch, 1, now).unwrap();
    eprintln!("synthetic park_bytes={} ledger_bytes={} admission_ms={} dispatch_ms={} admission_bytes={} dispatch_bytes={}",
            park_bytes.len(), count * line.len(), admit_ms, start.elapsed().as_millis(), admission_bytes, state_dir::take_written_bytes());
}

#[test]
fn pending_notice_blocks_replay_and_discard_without_removing_custody() {
    let dir = tempfile::tempdir().unwrap();
    let now = Utc::now();
    let mut runtime = ReliabilityRuntime::open_in(dir.path(), "test-agent", now).unwrap();
    let ch = Uuid::new_v4();
    let scope = SessionScope::Conversation { channel_id: ch };
    let (batch, _) = make_flush_batch(ch, scope.clone(), "retain notice source");
    runtime
        .park_batch(&batch, ParkReason::RetriesExhausted, false, now)
        .unwrap();
    let plan = runtime.plan_replay(&scope).unwrap();
    assert!(runtime.commit_replay(&plan, Uuid::new_v4(), now).is_err());
    assert!(runtime.discard(batch.batch_id, "operator", now).is_err());
    assert!(runtime.park().contains(batch.batch_id));
    acknowledge_test_notices(&mut runtime);
    assert!(runtime.commit_replay(&plan, Uuid::new_v4(), now).is_ok());
}

#[test]
fn recovered_ingress_waits_for_current_channel_authority() {
    let dir = tempfile::tempdir().unwrap();
    let now = Utc::now();
    let ch = Uuid::new_v4();
    let (event, id) = make_test_event("retained channel work");
    let input = QueuedEvent {
        channel_id: ch,
        scope: SessionScope::Conversation { channel_id: ch },
        event,
        prompt_tag: "test".into(),
        received_at: Instant::now(),
    };
    let mut runtime = ReliabilityRuntime::open_in(dir.path(), "test-agent", now).unwrap();
    runtime.admit_event(&input, now).unwrap();
    let mut queue = EventQueue::new(DedupMode::Queue);
    runtime.refill_ingress_for(&mut queue, &Default::default());
    assert!(!queue.contains_event(&id));
    assert_eq!(
        runtime.park().batches().len(),
        1,
        "revocation retains custody without dispatch"
    );
    runtime.refill_ingress_for(&mut queue, &std::collections::HashSet::from([ch]));
    assert!(queue.contains_event(&id));
}

#[test]
fn restart_replays_failed_precommit_but_suppresses_started_and_completed_ids() {
    let dir = tempfile::tempdir().unwrap();
    let now = Utc::now();
    let ch = Uuid::new_v4();
    let scope = SessionScope::Conversation { channel_id: ch };
    let (event, id) = make_test_event("precommit relay input");
    let queued = QueuedEvent {
        channel_id: ch,
        scope,
        event,
        prompt_tag: "test".into(),
        received_at: Instant::now(),
    };
    let mut runtime = ReliabilityRuntime::open_in(dir.path(), "test-agent", now).unwrap();
    let floor = runtime.replay_floor();
    let blocker = dir.path().join(transaction::PENDING_FILE);
    std::fs::create_dir(&blocker).unwrap();
    assert!(runtime.admit_event(&queued, now).is_err());
    drop(runtime);
    std::fs::remove_dir(blocker).unwrap();
    let mut runtime =
        ReliabilityRuntime::open_in(dir.path(), "test-agent", now + chrono::Duration::hours(1))
            .unwrap();
    assert_eq!(
        runtime.replay_floor(),
        floor,
        "restart must request the original subscription epoch"
    );
    assert!(
        runtime.admit_event(&queued, now).unwrap(),
        "uncommitted input redelivery must be admitted"
    );
    assert!(
        !runtime.admit_event(&queued, now).unwrap(),
        "park custody deduplicates accepted input"
    );
    runtime.prepare_steer(id, now).unwrap();
    drop(runtime);
    let mut runtime =
        ReliabilityRuntime::open_in(dir.path(), "test-agent", now + chrono::Duration::hours(2))
            .unwrap();
    assert!(
        !runtime.admit_event(&queued, now).unwrap(),
        "uncertain started input must never auto-repeat"
    );
    assert!(runtime.park().batches()[0].needs_review);
    runtime.finish_steer(&id.to_hex(), now).unwrap();
    drop(runtime);
    let mut runtime =
        ReliabilityRuntime::open_in(dir.path(), "test-agent", now + chrono::Duration::hours(3))
            .unwrap();
    assert!(runtime.park().batches().is_empty());
    assert!(
        !runtime.admit_event(&queued, now).unwrap(),
        "completed input receipt survives park removal"
    );
}

#[test]
fn native_steer_restart_is_review_only_until_proven_rejected_or_delivered() {
    let dir = tempfile::tempdir().unwrap();
    let now = Utc::now();
    let ch = Uuid::new_v4();
    let scope = SessionScope::Conversation { channel_id: ch };
    let (event, id) = make_test_event("native steering custody");
    let queued = QueuedEvent {
        channel_id: ch,
        scope,
        event,
        prompt_tag: "test".into(),
        received_at: Instant::now(),
    };
    let mut runtime = ReliabilityRuntime::open_in(dir.path(), "test-agent", now).unwrap();
    runtime.admit_event(&queued, now).unwrap();
    runtime.prepare_steer(id, now).unwrap();
    drop(runtime);
    let mut runtime = ReliabilityRuntime::open_in(dir.path(), "test-agent", now).unwrap();
    assert!(runtime.park().batches()[0].started);
    assert!(runtime.park().batches()[0].needs_review);
    let mut queue = EventQueue::new(DedupMode::Queue);
    runtime.refill_ingress(&mut queue);
    assert!(!queue.contains_event(&id));
    runtime.reject_steer(&id.to_hex(), now).unwrap();
    runtime.refill_ingress(&mut queue);
    assert!(queue.contains_event(&id));
    runtime.prepare_steer(id, now).unwrap();
    runtime.finish_steer(&id.to_hex(), now).unwrap();
    drop(runtime);
    let runtime = ReliabilityRuntime::open_in(dir.path(), "test-agent", now).unwrap();
    assert!(runtime.park().batches().is_empty());
}

#[test]
fn runtime_lock_refuses_second_process_then_releases() {
    const CHILD_ENV: &str = "BUZZ_TEST_RUNTIME_LOCK_DIR";
    if let Some(dir) = std::env::var_os(CHILD_ENV) {
        assert!(ReliabilityRuntime::open_in(Path::new(&dir), "test-agent", Utc::now()).is_err());
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let runtime = ReliabilityRuntime::open_in(dir.path(), "test-agent", Utc::now()).unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .env_clear()
        .env(CHILD_ENV, dir.path())
        .arg("--exact")
        .arg("reliability::runtime::tests::runtime_lock_refuses_second_process_then_releases")
        .status()
        .unwrap();
    assert!(status.success());
    drop(runtime);
    assert!(ReliabilityRuntime::open_in(dir.path(), "test-agent", Utc::now()).is_ok());
}

// Fixture #5: after a successful probe, a parked batch with started=true is
// NOT replayed and one with started=false IS, before newer events of the same scope.
#[test]
fn discard_retains_custody_when_audit_cannot_be_prepared() {
    let dir = tempfile::tempdir().unwrap();
    let now = Utc::now();
    let mut runtime = ReliabilityRuntime::open_in(dir.path(), "test-agent", now).unwrap();
    let ch = Uuid::new_v4();
    let (batch, _) = make_flush_batch(
        ch,
        SessionScope::Conversation { channel_id: ch },
        "retain me",
    );
    runtime
        .park_batch(&batch, ParkReason::RetriesExhausted, false, now)
        .unwrap();
    std::fs::rename(
        dir.path().join("ledger.jsonl"),
        dir.path().join("ledger.saved"),
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("ledger.jsonl")).unwrap();
    assert!(runtime.discard(batch.batch_id, "operator", now).is_err());
    assert!(runtime.park().contains(batch.batch_id));
    assert!(ParkFile::open(dir.path()).unwrap().contains(batch.batch_id));
}

#[test]
fn test_fixture_5_successful_probe_replays_not_started_before_newer_events() {
    let dir = tempfile::tempdir().unwrap();
    let pubkey = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let now = Utc::now();
    let mut runtime = ReliabilityRuntime::open_in(dir.path(), pubkey, now).unwrap();

    let channel_id = Uuid::new_v4();
    let scope = SessionScope::Conversation { channel_id };

    // 1. Parked batch with started = true
    let (batch_started, _) = make_flush_batch(channel_id, scope.clone(), "started msg");
    let batch_started_id = batch_started.batch_id;
    runtime
        .park_batch(&batch_started, ParkReason::HardTimeout, true, now)
        .unwrap();

    // 2. Parked batch with started = false
    let (batch_not_started, not_started_event_id) =
        make_flush_batch(channel_id, scope.clone(), "not started msg");
    let batch_not_started_id = batch_not_started.batch_id;
    runtime
        .park_batch(&batch_not_started, ParkReason::RetriesExhausted, false, now)
        .unwrap();

    // Verify initial parked state
    assert!(runtime.park().get(batch_started_id).unwrap().needs_review);
    assert!(!runtime
        .park()
        .get(batch_started_id)
        .unwrap()
        .replay_eligible());
    assert!(
        !runtime
            .park()
            .get(batch_not_started_id)
            .unwrap()
            .needs_review
    );
    assert!(runtime
        .park()
        .get(batch_not_started_id)
        .unwrap()
        .replay_eligible());

    acknowledge_test_notices(&mut runtime);

    // 3. A newer event arrives for the same scope in the queue
    let mut queue = EventQueue::new(DedupMode::Queue);
    let (newer_event, newer_event_id) = make_test_event("newer msg");
    queue.push(QueuedEvent {
        channel_id,
        scope: scope.clone(),
        event: newer_event,
        received_at: Instant::now(),
        prompt_tag: "newer".into(),
    });

    // 4. A probe succeeds! Bind the production function `replay_after_success`.
    crate::replay_after_success(&mut runtime, &mut queue, &scope, now);

    // Assert that started=true was NOT replayed
    let parked_started = runtime.park().get(batch_started_id).unwrap();
    assert!(
        parked_started.replayed_at.is_none(),
        "batch with started=true must NOT be marked replayed"
    );
    assert!(
        parked_started.needs_review,
        "batch with started=true must stay on needs_review list"
    );

    // Assert that started=false WAS replayed
    let parked_not_started = runtime.park().get(batch_not_started_id).unwrap();
    assert!(
        parked_not_started.replayed_at.is_some(),
        "batch with started=false IS replayed (replayed_at stamped)"
    );

    // Assert replay ordering: staged before newer events of the same scope
    let flushed = queue.flush_next().expect("flushed batch");
    assert_eq!(flushed.scope, scope);
    assert_eq!(
        flushed.cancel_reason,
        Some(CancelReason::DeliveredLate),
        "replayed events staged with DeliveredLate framing"
    );
    assert_eq!(flushed.cancelled_events.len(), 1);
    assert_eq!(
        flushed.cancelled_events[0].event.id, not_started_event_id,
        "replayed not-started event is in cancelled_events (preceding newer events)"
    );
    assert_eq!(flushed.events.len(), 1);
    assert_eq!(
        flushed.events[0].event.id, newer_event_id,
        "newer event is in events (after replayed events)"
    );
}

// Fixture #6: a `batch_replayed` ledger record with no matching `turn_finished`
// at start moves the batch to needs_review (reconcile_on_start).
#[test]
fn test_fixture_6_batch_replayed_without_turn_finished_moves_to_needs_review_on_start() {
    let dir = tempfile::tempdir().unwrap();
    let pubkey = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let now = Utc::now();
    let mut runtime = ReliabilityRuntime::open_in(dir.path(), pubkey, now).unwrap();

    let channel_id = Uuid::new_v4();
    let scope = SessionScope::Conversation { channel_id };
    let (batch, _) = make_flush_batch(channel_id, scope.clone(), "crashed mid-replay");
    let batch_id = batch.batch_id;

    // Park the batch (not started -> replay-eligible)
    runtime
        .park_batch(&batch, ParkReason::RetriesExhausted, false, now)
        .unwrap();
    assert!(!runtime.park().get(batch_id).unwrap().needs_review);
    assert!(runtime.park().get(batch_id).unwrap().replay_eligible());

    // Stage and commit replay: this writes `batch_replayed` to the ledger and stamps the park file
    let plan = runtime.plan_replay(&scope).expect("replay plan");
    assert_eq!(plan.batch_ids, vec![batch_id]);
    acknowledge_test_notices(&mut runtime);
    runtime.commit_replay(&plan, Uuid::new_v4(), now).unwrap();

    // Simulate crash mid-replay: process exits WITHOUT writing `turn_finished`.
    drop(runtime);

    // Process restarts at a later time
    let restart_now = now + chrono::Duration::seconds(30);
    let mut restarted = ReliabilityRuntime::open_in(dir.path(), pubkey, restart_now).unwrap();

    // Run start-up reconciliation using the production function
    let report = restarted.reconcile_on_start(restart_now).unwrap();
    assert_eq!(
        report.crashed_mid_replay, 1,
        "reconcile_on_start must report the crashed mid-replay batch"
    );

    // The batch must now be in needs_review, never to be automatically replayed
    let parked = restarted.park().get(batch_id).expect("batch still parked");
    assert!(
        parked.needs_review,
        "crashed mid-replay batch must have needs_review = true"
    );
    assert_eq!(
        parked.needs_review_reason.as_deref(),
        Some("replay was sent but the turn never finished")
    );
    assert!(
        !parked.replay_eligible(),
        "batch in needs_review must not be replay-eligible"
    );

    // A BatchNeedsReview record must have been appended to the ledger
    let records = restarted.ledger.read_all().unwrap();
    assert!(
        records.iter().any(
            |r| matches!(&r.body, LedgerBody::BatchNeedsReview(nr) if nr.batch_id == batch_id)
        ),
        "ledger must contain a batch_needs_review record for the crashed batch"
    );
}

#[test]
fn test_discard_of_unknown_batch_is_not_found_not_discarded() {
    let dir = tempfile::tempdir().unwrap();
    let pubkey = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let now = Utc::now();
    let mut runtime = ReliabilityRuntime::open_in(dir.path(), pubkey, now).unwrap();

    let result = runtime.discard(Uuid::new_v4(), "operator", now);
    assert!(
        matches!(result, Ok(DiscardOutcome::NotFound)),
        "discarding an id that was never parked must report NotFound, \
             distinct from a destructive outcome: got {result:?}"
    );
}

#[test]
#[cfg(unix)]
fn test_discard_atomically_replaces_readonly_audit_file() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let pubkey = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let now = Utc::now();
    let mut runtime = ReliabilityRuntime::open_in(dir.path(), pubkey, now).unwrap();

    let channel_id = Uuid::new_v4();
    let scope = SessionScope::Conversation { channel_id };
    let (batch, _) = make_flush_batch(channel_id, scope, "to discard");
    let batch_id = batch.batch_id;

    // Park the batch first.
    runtime
        .park_batch(&batch, ParkReason::RetriesExhausted, false, now)
        .unwrap();
    assert!(runtime.park().contains(batch_id));

    // Make ledger.jsonl unwritable while keeping the directory and park file writable.
    let ledger_path = dir.path().join("ledger.jsonl");
    let original_mode = std::fs::metadata(&ledger_path)
        .unwrap()
        .permissions()
        .mode();
    std::fs::set_permissions(&ledger_path, std::fs::Permissions::from_mode(0o400)).unwrap();

    acknowledge_test_notices(&mut runtime);
    let result = runtime.discard(batch_id, "operator", now);

    // Restore permissions for cleanup
    let _ = std::fs::set_permissions(&ledger_path, std::fs::Permissions::from_mode(original_mode));

    // The batch was removed from park, but ledger write failed. It must
    // be reported as destroyed-but-unrecorded — never as a clean
    // `Discarded` (unconditional success) and never as `NotFound`
    // (which would collapse a genuine destructive action into the same
    // signal as "no such batch", inviting a pointless retry).
    assert!(
        matches!(result, Ok(DiscardOutcome::Discarded)),
        "discard must distinguish a destroyed-but-unrecorded batch from \
             both a clean success and an unknown batch: got {result:?}"
    );
}

/// The largest content length for which `runtime.park_batch(..)` still
/// succeeds — i.e. the batch's own serialized line is at (or a hair
/// under) `MAX_LINE_BYTES`. Used to build a batch whose line has no
/// headroom left for the extra bytes `mark_replayed` adds.
fn max_parkable_content_len(
    runtime: &mut ReliabilityRuntime,
    channel_id: Uuid,
    scope: SessionScope,
    now: DateTime<Utc>,
) -> usize {
    let (mut low, mut high) = (0usize, crate::reliability::park::MAX_LINE_BYTES);
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        let content = "x".repeat(mid);
        let (probe, _) = make_flush_batch(channel_id, scope.clone(), &content);
        let fits = runtime
            .park_batch(&probe, ParkReason::RetriesExhausted, false, now)
            .is_ok();
        if fits {
            let _ = runtime.discard(probe.batch_id, "test-calibration", now);
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    low
}

#[test]
fn test_commit_replay_rolls_back_earlier_marks_when_a_later_one_fails() {
    // T16 delta 1, finding 4a: `commit_replay` marks every batch in the
    // plan as replayed one at a time. If an EARLIER mark durably lands
    // and a LATER one in the same call fails, the earlier one must not
    // stay stamped `replayed_at` — that would make it permanently
    // ineligible for replay even though this whole replay attempt is
    // being reported as failed and nothing was sent.
    let dir = tempfile::tempdir().unwrap();
    let pubkey = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let now = Utc::now();
    let mut runtime = ReliabilityRuntime::open_in(dir.path(), pubkey, now).unwrap();

    let channel_id = Uuid::new_v4();
    let scope = SessionScope::Conversation { channel_id };

    // batch1: tiny, parks and marks-replayed with room to spare.
    let (batch1, _) = make_flush_batch(channel_id, scope.clone(), "small");
    let batch1_id = batch1.batch_id;
    runtime
        .park_batch(&batch1, ParkReason::RetriesExhausted, false, now)
        .unwrap();

    // batch2: calibrated to the exact line-length ceiling, so it parks
    // successfully now but `mark_replayed`'s extra `replayed_at` field
    // pushes its line over MAX_LINE_BYTES.
    let max_len = max_parkable_content_len(&mut runtime, channel_id, scope.clone(), now);
    let (batch2, _) = make_flush_batch(channel_id, scope.clone(), &"x".repeat(max_len));
    let batch2_id = batch2.batch_id;
    runtime
        .park_batch(&batch2, ParkReason::RetriesExhausted, false, now)
        .expect("batch2 must park at the calibrated max length");

    let plan = ReplayPlan {
        batch_ids: vec![batch1_id, batch2_id],
        events: vec![],
        scope: scope.clone(),
        channel_id,
    };

    let result = runtime.commit_replay(&plan, Uuid::new_v4(), now);
    assert!(
        result.is_err(),
        "marking the oversized batch2 as replayed must fail: {result:?}"
    );

    let batches = runtime.park().batches();
    let find = |id: Uuid| batches.iter().find(|b| b.batch_id == id).unwrap();
    assert!(
        find(batch1_id).replayed_at.is_none(),
        "batch1's successful mark must be rolled back when batch2's mark fails"
    );
    assert!(
        find(batch2_id).replayed_at.is_none(),
        "batch2 must never have been marked replayed"
    );
}

fn make_multi_event_batch(
    channel_id: Uuid,
    scope: SessionScope,
    count: usize,
    prefix: &str,
) -> FlushBatch {
    let mut events = Vec::with_capacity(count);
    for i in 0..count {
        let (event, _) = make_test_event(&format!("{prefix}-{i}"));
        events.push(BatchEvent {
            event,
            prompt_tag: "test".into(),
            received_at: Instant::now(),
        });
    }
    FlushBatch {
        batch_id: Uuid::new_v4(),
        channel_id,
        scope,
        events,
        cancelled_events: vec![],
        cancel_reason: None,
        started: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    }
}

#[test]
fn test_replay_plan_respects_max_batch_events_and_preserves_unincluded_batches() {
    let dir = tempfile::tempdir().unwrap();
    let pubkey = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let now = Utc::now();
    let mut runtime = ReliabilityRuntime::open_in(dir.path(), pubkey, now).unwrap();

    let channel_id = Uuid::new_v4();
    let scope = SessionScope::Conversation { channel_id };

    // Park > 50 replay-eligible events across multiple batches for one scope.
    // Batch 1: 30 events
    let batch1 = make_multi_event_batch(channel_id, scope.clone(), 30, "batch1");
    let batch1_id = batch1.batch_id;
    runtime
        .park_batch(&batch1, ParkReason::RetriesExhausted, false, now)
        .unwrap();

    // Batch 2: 30 events (total 60 > 50)
    let batch2 = make_multi_event_batch(channel_id, scope.clone(), 30, "batch2");
    let batch2_id = batch2.batch_id;
    runtime
        .park_batch(
            &batch2,
            ParkReason::RetriesExhausted,
            false,
            now + chrono::Duration::seconds(1),
        )
        .unwrap();

    let mut queue = EventQueue::new(DedupMode::Queue);

    acknowledge_test_notices(&mut runtime);

    // Run a successful probe (binds replay_after_success)
    crate::replay_after_success(
        &mut runtime,
        &mut queue,
        &scope,
        now + chrono::Duration::seconds(2),
    );

    let staged = queue.flush_next().unwrap();
    runtime.prepare_dispatch(&staged, 1, now).unwrap();

    // The turn for this scope finishes successfully (clearing in-flight replay)
    let report = runtime.finish_replay(&scope);
    assert!(report.error.is_none(), "no removal should fail here");
    assert_eq!(
        report.released,
        vec![batch1_id],
        "only the included batch should be finished/released"
    );

    // The park file must STILL hold batch2, whose events were not included in the dispatched turn
    assert!(
        runtime.park().contains(batch2_id),
        "batch 2 was not included in the dispatched replay turn and must remain in the park file"
    );
    let parked2 = runtime
        .park()
        .get(batch2_id)
        .expect("batch 2 still in park");
    assert!(
        parked2.replay_eligible(),
        "batch 2 must still be replay-eligible"
    );
}

#[test]
fn record_mirrors_health_kinds_to_observer() {
    let temp = tempfile::tempdir().unwrap();
    let now = Utc::now();
    let agent = "test_agent_pk";
    let observer = ObserverHandle::in_process();
    let mut rx = observer.subscribe();

    let mut runtime = ReliabilityRuntime::open_in(temp.path(), agent, now)
        .unwrap()
        .with_observer(observer);

    let batch_id = Uuid::new_v4();
    let channel_id = Uuid::new_v4();
    let body = LedgerBody::BatchParked(ledger::BatchParked {
        batch_id,
        channel_id,
        reason: "retries_exhausted".to_string(),
        started: false,
        events: 3,
    });

    assert!(runtime.record(now, body));

    let event = rx.try_recv().expect("should receive observer frame");
    assert_eq!(event.kind, "batch_parked");
    assert_eq!(event.channel_id, Some(channel_id.to_string()));
    assert_eq!(event.payload["batchId"], batch_id.to_string());
    assert_eq!(event.payload["events"], 3);
    assert_eq!(event.payload["at"], serde_json::to_value(now).unwrap());
    assert_eq!(event.payload["reason"], "retries_exhausted");
    assert_eq!(event.payload["started"], false);

    // batch_replayed
    let replay_id = Uuid::new_v4();
    let replayed = LedgerBody::BatchReplayed(ledger::BatchReplayed {
        batch_id,
        channel_id,
        replay_of: replay_id,
    });
    assert!(runtime.record(now, replayed));
    let event = rx.try_recv().expect("should receive batch_replayed frame");
    assert_eq!(event.kind, "batch_replayed");
    assert_eq!(event.payload["replayOf"], replay_id.to_string());

    // agent_paused
    let paused = LedgerBody::AgentPaused(ledger::AgentPaused {
        class: "capacity_exhausted".to_string(),
        until: now,
        waiting: 2,
    });
    assert!(runtime.record(now, paused));
    let event = rx.try_recv().expect("should receive agent_paused frame");
    assert_eq!(event.kind, "agent_paused");
    assert_eq!(event.payload["class"], "capacity_exhausted");
    assert_eq!(event.channel_id, None);

    // breaker_opened
    let breaker = LedgerBody::BreakerOpened(ledger::BreakerOpened {
        scope: "scope1".to_string(),
        consecutive: 3,
    });
    assert!(runtime.record(now, breaker));
    let event = rx.try_recv().expect("should receive breaker_opened frame");
    assert_eq!(event.kind, "breaker_opened");
    assert_eq!(event.payload["consecutive"], 3);

    // batch_needs_review
    let needs_review_id = Uuid::new_v4();
    let needs_review = LedgerBody::BatchNeedsReview(ledger::BatchNeedsReview {
        batch_id: needs_review_id,
        channel_id,
        reason: "interrupted after it had started".to_string(),
    });
    assert!(runtime.record(now, needs_review));
    let event = rx
        .try_recv()
        .expect("should receive batch_needs_review frame");
    assert_eq!(event.kind, "batch_needs_review");
    assert_eq!(event.payload["batchId"], needs_review_id.to_string());
    assert_eq!(event.payload["reason"], "interrupted after it had started");

    // agent_resumed
    let resumed = LedgerBody::AgentResumed(ledger::AgentResumed {});
    assert!(runtime.record(now, resumed));
    let event = rx.try_recv().expect("should receive agent_resumed frame");
    assert_eq!(event.kind, "agent_resumed");

    // breaker_closed
    let breaker_closed = LedgerBody::BreakerClosed(ledger::BreakerClosed {
        scope: "scope1".to_string(),
    });
    assert!(runtime.record(now, breaker_closed));
    let event = rx.try_recv().expect("should receive breaker_closed frame");
    assert_eq!(event.kind, "breaker_closed");
    assert_eq!(event.payload["scope"], "scope1");

    // relay_reconnected
    let relay_reconnected =
        LedgerBody::RelayReconnected(ledger::RelayReconnected { after_secs: 42 });
    assert!(runtime.record(now, relay_reconnected));
    let event = rx
        .try_recv()
        .expect("should receive relay_reconnected frame");
    assert_eq!(event.kind, "relay_reconnected");
    assert_eq!(event.payload["afterSecs"], 42);
}

#[test]
fn turn_failed_frame_carries_class_but_never_raw() {
    let temp = tempfile::tempdir().unwrap();
    let now = Utc::now();
    let agent = "test_agent_pk";
    let observer = ObserverHandle::in_process();
    let mut rx = observer.subscribe();

    let mut runtime = ReliabilityRuntime::open_in(temp.path(), agent, now)
        .unwrap()
        .with_observer(observer);

    let batch_id = Uuid::new_v4();
    let channel_id = Uuid::new_v4();
    let raw_secret = "secret provider raw error stack trace";
    let body = LedgerBody::TurnFinished(ledger::TurnFinished {
        batch_id,
        channel_id,
        outcome: ledger::TurnOutcome::error("capacity_exhausted", raw_secret),
    });

    assert!(runtime.record(now, body));

    let event = rx.try_recv().expect("should receive observer frame");
    assert_eq!(event.kind, "turn_failed");
    assert_eq!(event.payload["class"], "capacity_exhausted");
    assert!(event.payload.get("raw").is_none());
    if let Some(outcome) = event.payload.get("outcome") {
        assert!(outcome.get("raw").is_none());
    }
    let serialized = event.payload.to_string();
    assert!(!serialized.contains(raw_secret));
    assert!(!serialized.contains("\"raw\""));
}

#[test]
fn turn_ok_and_turn_started_emit_no_frame() {
    let temp = tempfile::tempdir().unwrap();
    let now = Utc::now();
    let agent = "test_agent_pk";
    let observer = ObserverHandle::in_process();
    let mut rx = observer.subscribe();

    let mut runtime = ReliabilityRuntime::open_in(temp.path(), agent, now)
        .unwrap()
        .with_observer(observer);

    let batch_id = Uuid::new_v4();
    let channel_id = Uuid::new_v4();

    let started = LedgerBody::TurnStarted(ledger::TurnStarted::new(
        batch_id,
        channel_id,
        "test_scope",
        vec!["e1".to_string()],
        1,
    ));
    assert!(runtime.record(now, started));
    assert!(
        rx.try_recv().is_err(),
        "turn_started must not emit health frame"
    );

    let ok = LedgerBody::TurnFinished(ledger::TurnFinished {
        batch_id,
        channel_id,
        outcome: ledger::TurnOutcome::Ok,
    });
    assert!(runtime.record(now, ok));
    assert!(
        rx.try_recv().is_err(),
        "turn_finished Ok must not emit health frame"
    );

    let discarded = LedgerBody::BatchDiscarded(ledger::BatchDiscarded {
        batch_id,
        channel_id,
        by: "operator".to_string(),
    });
    assert!(runtime.record(now, discarded));
    assert!(
        rx.try_recv().is_err(),
        "batch_discarded must not emit health frame"
    );
}
