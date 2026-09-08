use super::*;

fn make_test_prompt_context() -> PromptContext {
    let agent_keys = nostr::Keys::generate();
    PromptContext {
        mcp_servers: crate::McpServerSet::from_servers(vec![]),
        initial_message: None,
        idle_timeout: std::time::Duration::from_secs(60),
        max_turn_duration: std::time::Duration::from_secs(120),
        turn_liveness_interval: std::time::Duration::ZERO,
        dedup_mode: config::DedupMode::Drop,
        system_prompt: None,
        session_title: None,
        team_instructions: None,
        heartbeat_prompt: None,
        base_prompt: None,
        cwd: ".".to_string(),
        rest_client: relay::RestClient {
            http: reqwest::Client::new(),
            base_url: "http://127.0.0.1:0".to_string(),
            keys: agent_keys.clone(),
            auth_tag_json: None,
        },
        channel_info: pool::ChannelInfoResolver::new(
            std::collections::HashMap::new(),
            relay::RestClient {
                http: reqwest::Client::new(),
                base_url: "http://127.0.0.1:0".to_string(),
                keys: agent_keys.clone(),
                auth_tag_json: None,
            },
        ),
        context_message_limit: 0,
        max_turns_per_session: 0,
        permission_mode: config::PermissionMode::Default,
        agent_keys,
        agent_owner_pubkey: None,
        memory_enabled: false,
        harness_name: "test".to_string(),
        relay_url: "http://127.0.0.1:0".to_string(),
    }
}

#[tokio::test]
async fn parked_only_due_probe_reaches_production_dispatch_without_new_traffic() {
    for pause in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let now = chrono::Utc::now();
        let past = now - chrono::Duration::minutes(40);
        let mut runtime =
            reliability::ReliabilityRuntime::open_in(dir.path(), "test-agent", now).unwrap();
        let ch = Uuid::new_v4();
        let scope = scope::SessionScope::Conversation { channel_id: ch };
        let mut queue = EventQueue::new(config::DedupMode::Queue);
        let event = nostr::EventBuilder::new(nostr::Kind::Custom(9), "parked probe input")
            .sign_with_keys(&nostr::Keys::generate())
            .unwrap();
        queue.push(QueuedEvent {
            channel_id: ch,
            scope: scope.clone(),
            event,
            prompt_tag: "test".into(),
            received_at: std::time::Instant::now(),
        });
        let batch = queue.flush_next().unwrap();
        queue.mark_complete(scope.clone());
        let reason = if pause {
            runtime.state().on_failure(
                &scope,
                reliability::ErrorClass::CapacityExhausted {
                    resets_at: Some(past + chrono::Duration::minutes(30)),
                },
                past,
            );
            reliability::ParkReason::Pause
        } else {
            for _ in 0..3 {
                runtime
                    .state()
                    .on_failure(&scope, reliability::ErrorClass::ProviderInternal, past);
            }
            reliability::ParkReason::BreakerOpen
        };
        runtime.park_batch(&batch, reason, false, now).unwrap();
        runtime.mark_notice_enqueued(batch.batch_id).unwrap();
        assert!(!queue.has_flushable_work(), "fixture has only parked work");
        runtime
            .stage_due_probes(&mut queue, &HashSet::from([ch]), now)
            .unwrap();
        let agent = crate::error_outcome_emission_tests::dummy_agent(0).await;
        let mut pool = AgentPool::from_slots(vec![Some(agent)]);
        let mut last_activity = tokio::time::Instant::now();
        let dispatched = dispatch_pending(
            &mut pool,
            &mut queue,
            &std::sync::Arc::new(make_test_prompt_context()),
            &mut last_activity,
            Some(&mut runtime),
        );
        assert_eq!(
            dispatched.len(),
            1,
            "due parked probe must actually dispatch with no new relay event"
        );
        pool.join_set.abort_all();
    }
}

#[test]
fn replay_control_exposes_the_uuid_used_by_actual_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let now = chrono::Utc::now();
    let mut runtime =
        reliability::ReliabilityRuntime::open_in(dir.path(), "test-agent", now).unwrap();
    let ch = Uuid::new_v4();
    let scope = scope::SessionScope::Conversation { channel_id: ch };
    let event = nostr::EventBuilder::new(nostr::Kind::Custom(9), "operator retry")
        .sign_with_keys(&nostr::Keys::generate())
        .unwrap();
    let mut queue = EventQueue::new(config::DedupMode::Queue);
    queue.push(QueuedEvent {
        channel_id: ch,
        scope: scope.clone(),
        event,
        prompt_tag: "test".into(),
        received_at: std::time::Instant::now(),
    });
    let original = queue.flush_next().unwrap();
    queue.mark_complete(scope);
    runtime
        .park_batch(&original, reliability::ParkReason::HardTimeout, true, now)
        .unwrap();
    runtime.mark_notice_enqueued(original.batch_id).unwrap();
    let observer = observer::ObserverHandle::in_process();
    let request_id = Uuid::new_v4();
    handle_reliability_control(
        "replay_batch",
        &serde_json::json!({"batchId":original.batch_id,"requestId":request_id}),
        Some(&mut runtime),
        &mut queue,
        Some(&observer),
    );
    let frame = observer.snapshot().pop().unwrap();
    assert_eq!(frame.payload["status"], "scheduled");
    assert_eq!(frame.payload["requestId"], request_id.to_string());
    let dispatched = queue
        .flush_next()
        .expect("operator replay must be staged immediately");
    assert_eq!(
        frame.payload["replayBatchId"],
        dispatched.batch_id.to_string()
    );
    runtime.prepare_dispatch(&dispatched, 1, now).unwrap();
    let records = reliability::ledger::read_ledger_file(&dir.path().join("ledger.jsonl")).unwrap();
    assert!(records.iter().any(|record| matches!(&record.body,
            reliability::ledger::LedgerBody::TurnStarted(turn) if turn.batch_id == dispatched.batch_id)));
}

fn lease_pause_probe(
    runtime: &mut reliability::ReliabilityRuntime,
    scope: &scope::SessionScope,
    now: chrono::DateTime<chrono::Utc>,
) {
    let past = now - chrono::Duration::minutes(40);
    runtime.state().on_failure(
        scope,
        reliability::ErrorClass::CapacityExhausted {
            resets_at: Some(past + chrono::Duration::minutes(30)),
        },
        past,
    );
    assert_eq!(
        runtime.state().pause_gate(now),
        reliability::PauseGate::Probe
    );
    runtime.state().set_pause_probe_scope(scope.clone());
}

#[tokio::test]
async fn test_dispatch_pending_does_not_leak_probe_permit_on_hold() {
    let dir = tempfile::tempdir().unwrap();
    let now = chrono::Utc::now();
    let mut runtime =
        reliability::ReliabilityRuntime::open_in(dir.path(), "test-agent", now).unwrap();

    let channel_id = uuid::Uuid::new_v4();
    let scope = scope::SessionScope::Conversation { channel_id };

    // Set pause expired in the past: 40 mins ago, reset was at 30 mins (10 mins ago).
    let t0 = now - chrono::Duration::minutes(40);
    runtime.state().on_failure(
        &scope,
        reliability::ErrorClass::CapacityExhausted {
            resets_at: Some(t0 + chrono::Duration::minutes(30)),
        },
        t0,
    );

    // Queue has a batch ready to dispatch.
    let mut queue = EventQueue::new(config::DedupMode::Queue);
    let event = nostr::EventBuilder::new(nostr::Kind::Custom(9), "probe event")
        .tags([])
        .sign_with_keys(&nostr::Keys::generate())
        .unwrap();
    queue.push(queue::QueuedEvent {
        channel_id,
        scope: scope.clone(),
        event,
        received_at: std::time::Instant::now(),
        prompt_tag: "p".into(),
    });

    // Pool has no available workers (simulating a hold / pool exhausted).
    let mut pool = AgentPool::from_slots(vec![]);
    let ctx = std::sync::Arc::new(make_test_prompt_context());
    let mut last_activity = tokio::time::Instant::now();

    // Calling dispatch_pending encounters the probe, but holds because pool is exhausted.
    let dispatched = dispatch_pending(
        &mut pool,
        &mut queue,
        &ctx,
        &mut last_activity,
        Some(&mut runtime),
    );
    assert!(
        dispatched.is_empty(),
        "no tasks should be dispatched with empty pool"
    );

    // A subsequent pause_gate(now) call MUST still return Probe (not stuck Held).
    assert_eq!(
        runtime.state().pause_gate(now),
        reliability::PauseGate::Probe,
        "probe permit must not be leaked on hold"
    );
}

#[tokio::test]
#[cfg(unix)]
async fn test_park_failure_does_not_discard_batch_on_hard_timeout_or_auth() {
    use crate::error_outcome_emission_tests::{dummy_agent, test_config};
    use crate::queue::BatchEvent;

    let check = |outcome: PromptOutcome| async move {
        let dir = tempfile::tempdir().unwrap();
        let now = chrono::Utc::now();
        let mut runtime =
            reliability::ReliabilityRuntime::open_in(dir.path(), "test-agent", now).unwrap();

        let channel_id = uuid::Uuid::new_v4();
        let scope = scope::SessionScope::Conversation { channel_id };

        let keys = nostr::Keys::generate();
        let event = nostr::EventBuilder::new(nostr::Kind::Custom(9), "test")
            .tags([])
            .sign_with_keys(&keys)
            .unwrap();
        let batch = FlushBatch {
            batch_id: uuid::Uuid::new_v4(),
            channel_id,
            scope: scope.clone(),
            events: vec![BatchEvent {
                event,
                prompt_tag: "test".into(),
                received_at: std::time::Instant::now(),
            }],
            cancelled_events: vec![],
            cancel_reason: None,
            started: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };

        let agent = dummy_agent(0).await;
        let mut pool = AgentPool::from_slots(vec![None]);
        let task_id = pool.join_set.spawn(async {}).id();
        pool.task_map_mut().insert(
            task_id,
            crate::pool::TaskMeta {
                agent_index: 0,
                channel_id: None,
                scope: None,
                turn_id: "test-turn-id".to_string(),
                recoverable_batch: None,
                control_tx: None,
                steer_tx: None,
                successful_steer_deliveries: std::collections::HashSet::new(),
            },
        );

        let mut queue = EventQueue::new(config::DedupMode::Queue);
        let config = test_config();
        let mut heartbeat_in_flight = false;
        let removed_channels = std::collections::HashSet::new();
        let mut crash_history = vec![SlotCircuit {
            crash_times: Vec::new(),
            open_until: None,
            respawn_in_flight: false,
        }];
        let (respawn_tx, _respawn_rx) = tokio::sync::mpsc::channel(8);
        let mut respawn_tasks = tokio::task::JoinSet::new();

        let result = PromptResult {
            started: false,
            agent,
            source: PromptSource::Channel(scope.clone()),
            turn_id: "test-turn-id".to_string(),
            outcome,
            batch: Some(batch),
        };

        // A directory at the park-file destination makes the actual
        // atomic replacement fail, even when directory modes are hardened.
        let park_path = dir.path().join("parked.jsonl");
        std::fs::rename(&park_path, dir.path().join("parked.saved")).unwrap();
        std::fs::create_dir(&park_path).unwrap();

        lease_pause_probe(&mut runtime, &scope, now);

        handle_prompt_result(
            &mut pool,
            &mut queue,
            &config,
            result,
            &mut heartbeat_in_flight,
            &removed_channels,
            &mut crash_history,
            &respawn_tx,
            &mut respawn_tasks,
            None,
            None,
            Some(&mut runtime),
        );

        assert_eq!(
            runtime.state().pause_gate(now),
            reliability::PauseGate::Probe,
            "production failed-result handling must release its probe even if parking fails"
        );
        // Assert the batch is still present in the queue or park hand-off, never dropped.
        let in_queue = queue.queued_event_count(channel_id) > 0;
        let in_handoff = queue.has_parked_handoff();
        assert!(
            in_queue || in_handoff,
            "batch must be present in queue or park hand-off, but was dropped"
        );
    };

    // Case 1: Hard timeout with recently_active = false
    check(PromptOutcome::Timeout(pool::TimeoutKind::Hard {
        recently_active: false,
    }))
    .await;

    // Case 2: Bare auth error
    check(PromptOutcome::Error(acp::AcpError::AgentError {
        code: -32000,
        message: "API Error: 401 Unauthorized".to_string(),
    }))
    .await;
}

#[tokio::test]
async fn test_pause_held_batch_is_durable_across_restart() {
    use crate::error_outcome_emission_tests::{dummy_agent, test_config};
    use crate::queue::BatchEvent;

    let dir = tempfile::tempdir().unwrap();
    let pubkey = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let now = chrono::Utc::now();
    let mut runtime = reliability::ReliabilityRuntime::open_in(dir.path(), pubkey, now).unwrap();

    let channel_id = uuid::Uuid::new_v4();
    let scope = scope::SessionScope::Conversation { channel_id };

    let keys = nostr::Keys::generate();
    let event = nostr::EventBuilder::new(nostr::Kind::Custom(9), "held message")
        .tags([])
        .sign_with_keys(&keys)
        .unwrap();
    let batch_id = uuid::Uuid::new_v4();
    let batch = FlushBatch {
        batch_id,
        channel_id,
        scope: scope.clone(),
        events: vec![BatchEvent {
            event,
            prompt_tag: "test".into(),
            received_at: std::time::Instant::now(),
        }],
        cancelled_events: vec![],
        cancel_reason: None,
        started: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    let agent = dummy_agent(0).await;
    let mut pool = AgentPool::from_slots(vec![None]);
    let task_id = pool.join_set.spawn(async {}).id();
    pool.task_map_mut().insert(
        task_id,
        crate::pool::TaskMeta {
            agent_index: 0,
            channel_id: None,
            scope: None,
            turn_id: "test-turn-id".to_string(),
            recoverable_batch: None,
            control_tx: None,
            steer_tx: None,
            successful_steer_deliveries: std::collections::HashSet::new(),
        },
    );

    let mut queue = EventQueue::new(config::DedupMode::Queue);
    let config = test_config();
    let mut heartbeat_in_flight = false;
    let removed_channels = std::collections::HashSet::new();
    let mut crash_history = vec![SlotCircuit {
        crash_times: Vec::new(),
        open_until: None,
        respawn_in_flight: false,
    }];
    let (respawn_tx, _respawn_rx) = tokio::sync::mpsc::channel(8);
    let mut respawn_tasks = tokio::task::JoinSet::new();

    // Error that triggers Action::Pause: session limit resets at 4:20am
    let outcome = PromptOutcome::Error(acp::AcpError::AgentError {
        code: -32603,
        message:
            "Internal error: You've hit your session limit · resets 4:20am (America/Los_Angeles)"
                .to_string(),
    });

    let result = PromptResult {
        started: false,
        agent,
        source: PromptSource::Channel(scope.clone()),
        turn_id: "test-turn-id".to_string(),
        outcome,
        batch: Some(batch),
    };

    handle_prompt_result(
        &mut pool,
        &mut queue,
        &config,
        result,
        &mut heartbeat_in_flight,
        &removed_channels,
        &mut crash_history,
        &respawn_tx,
        &mut respawn_tasks,
        None,
        None,
        Some(&mut runtime),
    );

    // Process restarts: drop runtime, queue, and simulated relay state
    drop(runtime);
    drop(queue);

    let restart_now = now + chrono::Duration::seconds(10);
    let restarted =
        reliability::ReliabilityRuntime::open_in(dir.path(), pubkey, restart_now).unwrap();

    // The held message must be recoverable (present in park file)
    assert!(
        restarted.park().contains(batch_id),
        "held message must be durable in park file across restart, not silently gone"
    );
}

#[tokio::test]
#[cfg(unix)]
async fn test_state_dir_failure_refuses_work_and_picks_up_on_reopen() {
    use crate::error_outcome_emission_tests::dummy_agent;
    use std::os::unix::fs::PermissionsExt;

    let parent = tempfile::tempdir().unwrap();
    let state_dir = parent.path().join("state");
    let pubkey = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let now = chrono::Utc::now();

    // 1. Start with an unwritable state dir (parent is read-only)
    std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
    let initial_open = reliability::ReliabilityRuntime::open_in(&state_dir, pubkey, now);
    assert!(
        initial_open.is_err(),
        "open must fail when state dir is unwritable"
    );

    // 2. Setup queue with pending work and an agent ready in the pool
    let mut queue = EventQueue::new(config::DedupMode::Queue);
    let channel_id = uuid::Uuid::new_v4();
    let scope = scope::SessionScope::Conversation { channel_id };
    let event = nostr::EventBuilder::new(nostr::Kind::Custom(9), "work")
        .tags([])
        .sign_with_keys(&nostr::Keys::generate())
        .unwrap();
    queue.push(queue::QueuedEvent {
        channel_id,
        scope: scope.clone(),
        event,
        received_at: std::time::Instant::now(),
        prompt_tag: "p".into(),
    });

    let agent = dummy_agent(0).await;
    let mut pool = AgentPool::from_slots(vec![Some(agent)]);
    let ctx = std::sync::Arc::new(make_test_prompt_context());
    let mut last_activity = tokio::time::Instant::now();

    // 3. Dispatching with reliability = None MUST refuse to dispatch work
    let dispatched = dispatch_pending(&mut pool, &mut queue, &ctx, &mut last_activity, None);
    assert!(
        dispatched.is_empty(),
        "must not dispatch work when reliability state is unavailable"
    );
    assert_eq!(
        queue.queued_event_count(channel_id),
        1,
        "work must remain in the queue rather than being accepted/discarded"
    );

    // 4. Later, the directory is made writable
    std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut runtime = reliability::ReliabilityRuntime::open_in(&state_dir, pubkey, now)
        .expect("reopen must succeed once dir is writable");

    // 5. Work is now picked up and dispatched
    let dispatched = dispatch_pending(
        &mut pool,
        &mut queue,
        &ctx,
        &mut last_activity,
        Some(&mut runtime),
    );
    assert!(
        !dispatched.is_empty(),
        "work must be dispatched once reliability state is open"
    );
    assert_eq!(queue.queued_event_count(channel_id), 0);
}

#[tokio::test]
async fn test_probe_timer_fires_without_external_relay_event() {
    use crate::error_outcome_emission_tests::dummy_agent;
    let dir = tempfile::tempdir().unwrap();
    let now = chrono::Utc::now();
    let pubkey = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let mut runtime = reliability::ReliabilityRuntime::open_in(dir.path(), pubkey, now).unwrap();

    let channel_id = uuid::Uuid::new_v4();
    let scope = scope::SessionScope::Conversation { channel_id };

    // 1. Enter Paused with a short until (50ms in future)
    let until = now + chrono::Duration::milliseconds(50);
    runtime.state().on_failure(
        &scope,
        reliability::ErrorClass::CapacityExhausted {
            resets_at: Some(until),
        },
        now,
    );

    // 2. Queue work for scope
    let mut queue = EventQueue::new(config::DedupMode::Queue);
    let event = nostr::EventBuilder::new(nostr::Kind::Custom(9), "work")
        .tags([])
        .sign_with_keys(&nostr::Keys::generate())
        .unwrap();
    queue.push(queue::QueuedEvent {
        channel_id,
        scope: scope.clone(),
        event,
        received_at: std::time::Instant::now(),
        prompt_tag: "p".into(),
    });

    // 3. Pool has an agent ready
    let agent = dummy_agent(0).await;
    let mut pool = AgentPool::from_slots(vec![Some(agent)]);
    let ctx = std::sync::Arc::new(make_test_prompt_context());
    let mut last_activity = tokio::time::Instant::now();

    // 4. All optional timers disabled:
    let mut heartbeat: Option<tokio::time::Interval> = None;
    let mut presence_heartbeat: Option<tokio::time::Interval> = None;
    let mut typing_refresh: Option<tokio::time::Interval> = None;
    let mut inactivity_reaper: Option<tokio::time::Interval> = None;
    let mut idle_pool_sleep_reaper: Option<tokio::time::Interval> = None;

    // 5. Probe timer arm is armed
    let mut probe_timer = ProbeTimerArm::new();
    probe_timer.rearm(Some(&mut runtime));

    // 6. Run select with no external relay event
    let dispatched = tokio::time::timeout(std::time::Duration::from_millis(500), async {
        tokio::select! {
            _ = async {
                match heartbeat.as_mut() {
                    Some(t) => t.tick().await,
                    None => std::future::pending().await,
                }
            } => false,
            _ = async {
                match presence_heartbeat.as_mut() {
                    Some(t) => t.tick().await,
                    None => std::future::pending().await,
                }
            } => false,
            _ = async {
                match typing_refresh.as_mut() {
                    Some(t) => t.tick().await,
                    None => std::future::pending().await,
                }
            } => false,
            _ = async {
                match inactivity_reaper.as_mut() {
                    Some(t) => t.tick().await,
                    None => std::future::pending().await,
                }
            } => false,
            _ = async {
                match idle_pool_sleep_reaper.as_mut() {
                    Some(t) => t.tick().await,
                    None => std::future::pending().await,
                }
            } => false,
            _ = std::future::pending::<()>() => false, // no external relay event
            _ = probe_timer.tick() => {
                if probe_timer.is_valid_wake(Some(&runtime)) {
                    let res = dispatch_pending(
                        &mut pool,
                        &mut queue,
                        &ctx,
                        &mut last_activity,
                        Some(&mut runtime),
                    );
                    !res.is_empty()
                } else {
                    false
                }
            }
        }
    })
    .await
    .expect("probe timer must fire without external relay event before timeout");

    assert!(dispatched, "probe must have been dispatched");
    assert_eq!(
        queue.queued_event_count(channel_id),
        0,
        "queued event should have been dispatched"
    );
}

#[tokio::test]
async fn test_dispatch_pending_short_circuits_global_pause_in_o1() {
    use crate::error_outcome_emission_tests::dummy_agent;
    let dir = tempfile::tempdir().unwrap();
    let now = chrono::Utc::now();
    let pubkey = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let mut runtime = reliability::ReliabilityRuntime::open_in(dir.path(), pubkey, now).unwrap();

    // 1. Enter Paused with until in the future (30 minutes)
    let until = now + chrono::Duration::minutes(30);
    let channel_id_0 = uuid::Uuid::new_v4();
    let scope_0 = scope::SessionScope::Conversation {
        channel_id: channel_id_0,
    };
    runtime.state().on_failure(
        &scope_0,
        reliability::ErrorClass::CapacityExhausted {
            resets_at: Some(until),
        },
        now,
    );

    // 2. Queue work for 50 distinct scopes
    let mut queue = EventQueue::new(config::DedupMode::Queue);
    for _ in 0..50 {
        let ch = uuid::Uuid::new_v4();
        let sc = scope::SessionScope::Conversation { channel_id: ch };
        let event = nostr::EventBuilder::new(nostr::Kind::Custom(9), "work")
            .tags([])
            .sign_with_keys(&nostr::Keys::generate())
            .unwrap();
        queue.push(queue::QueuedEvent {
            channel_id: ch,
            scope: sc,
            event,
            received_at: std::time::Instant::now(),
            prompt_tag: "p".into(),
        });
    }
    assert_eq!(queue.pending_channels(), 50);

    let agent = dummy_agent(0).await;
    let mut pool = AgentPool::from_slots(vec![Some(agent)]);
    let ctx = std::sync::Arc::new(make_test_prompt_context());
    let mut last_activity = tokio::time::Instant::now();

    let flushes_before = queue.flush_count();
    let dispatched = dispatch_pending(
        &mut pool,
        &mut queue,
        &ctx,
        &mut last_activity,
        Some(&mut runtime),
    );

    assert!(
        dispatched.is_empty(),
        "no work should be dispatched during pause"
    );
    let flushes_after = queue.flush_count();
    // Without fix, flush_count increments by 51 (O(scopes)). With O(1) short-circuit, it increments by 0.
    assert_eq!(
        flushes_after - flushes_before,
        0,
        "dispatch_pending must short-circuit without calling flush_next when paused"
    );
    assert_eq!(queue.pending_channels(), 50, "all 50 scopes remain queued");
}

#[tokio::test]
async fn test_panicked_agent_after_output_parks_with_started_true_and_needs_review() {
    let dir = tempfile::tempdir().unwrap();
    let now = chrono::Utc::now();
    let pubkey = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let mut runtime = reliability::ReliabilityRuntime::open_in(dir.path(), pubkey, now).unwrap();

    let mut queue = EventQueue::new(config::DedupMode::Queue);
    let channel_id = uuid::Uuid::new_v4();
    let scope = scope::SessionScope::Conversation { channel_id };

    let event = nostr::EventBuilder::new(nostr::Kind::Custom(9), "work")
        .tags([])
        .sign_with_keys(&nostr::Keys::generate())
        .unwrap();
    queue.push(queue::QueuedEvent {
        channel_id,
        scope: scope.clone(),
        event,
        received_at: std::time::Instant::now(),
        prompt_tag: "p".into(),
    });

    let batch = queue.flush_next().unwrap();
    // Agent started turn and emitted output before panicking
    batch.mark_started();
    assert!(batch.is_started());

    // Exhaust retries: MAX_RETRIES attempts, then the next requeue moves it to parked_out
    for _ in 0..queue::MAX_RETRIES {
        let _ = queue.requeue(batch.clone());
    }
    let exhausted = queue.requeue(batch.clone());
    assert!(
        exhausted.is_none(),
        "requeue must return None when retries are exhausted"
    );

    // Drain park handoff
    drain_park_handoff(&mut runtime, &mut queue, None, now);

    let parked = runtime.park().batches();
    assert_eq!(parked.len(), 1, "exactly one batch should be parked");
    let parked_batch = &parked[0];
    assert!(
        parked_batch.started,
        "parked batch must have started == true"
    );
    assert!(
        parked_batch.needs_review,
        "parked batch must have needs_review == true"
    );
    assert_eq!(
        parked_batch.needs_review_reason.as_deref(),
        Some("interrupted after it had started")
    );
    assert!(
        !parked_batch.replay_eligible(),
        "parked batch that started must not be replay-eligible"
    );
}

// T16 delta 1, finding 10 (prior #8): the production panic-recovery seam
// itself — not a hand-rolled `mark_started` + `queue.requeue` sequence —
// must carry `started` through to the park file. Before the fix,
// `recover_panicked_agent` called plain `queue.requeue(batch)`, which
// deconstructs the batch into `QueuedEvent`s and drops the shared
// `started` `Arc` entirely; the next flush built a fresh batch with
// `started` defaulting back to `false`.
#[tokio::test]
async fn panicked_agent_with_output_is_parked_directly_as_needs_review() {
    let dir = tempfile::tempdir().unwrap();
    let now = chrono::Utc::now();
    let pubkey = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef012345678";
    let mut runtime = reliability::ReliabilityRuntime::open_in(dir.path(), pubkey, now).unwrap();

    let mut pool = AgentPool::from_slots(vec![]);
    let mut queue = EventQueue::new(config::DedupMode::Queue);
    let channel_id = Uuid::new_v4();
    let scope = scope::SessionScope::Conversation { channel_id };

    let event = nostr::EventBuilder::new(nostr::Kind::Custom(9), "work")
        .tags([])
        .sign_with_keys(&nostr::Keys::generate())
        .unwrap();
    queue.push(queue::QueuedEvent {
        channel_id,
        scope: scope.clone(),
        event,
        received_at: std::time::Instant::now(),
        prompt_tag: "p".into(),
    });
    let batch = queue.flush_next().expect("flush batch");
    // The agent produced output/a tool call before it panicked.
    batch.mark_started();
    assert!(batch.is_started());

    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let abort_handle = pool.join_set.spawn(async move {
        let _ = started_tx.send(());
        std::future::pending::<()>().await;
    });
    pool.task_map_mut().insert(
        abort_handle.id(),
        crate::pool::TaskMeta {
            agent_index: 0,
            channel_id: Some(channel_id),
            scope: Some(scope.clone()),
            turn_id: "panic-turn-id".to_string(),
            recoverable_batch: Some(batch),
            control_tx: None,
            steer_tx: None,
            successful_steer_deliveries: HashSet::new(),
        },
    );
    started_rx.await.unwrap();
    abort_handle.abort();
    let join_error = pool.join_set.join_next().await.unwrap().unwrap_err();

    let config = crate::error_outcome_emission_tests::test_config();
    let mut heartbeat_in_flight = false;
    let removed_channels = HashSet::new();
    let mut typing_channels = HashMap::new();
    let mut crash_history = vec![SlotCircuit {
        crash_times: Vec::new(),
        open_until: Some(std::time::Instant::now() + Duration::from_secs(3600)),
        respawn_in_flight: false,
    }];
    let (respawn_tx, _respawn_rx) = mpsc::channel(8);
    let mut respawn_tasks = tokio::task::JoinSet::new();

    lease_pause_probe(&mut runtime, &scope, now);

    recover_panicked_agent(
        &mut pool,
        &mut queue,
        &config,
        join_error,
        &mut heartbeat_in_flight,
        &removed_channels,
        &mut typing_channels,
        &mut crash_history,
        &respawn_tx,
        &mut respawn_tasks,
        None,
        None,
        Some(&mut runtime),
    );

    assert_eq!(
        runtime.state().pause_gate(now),
        reliability::PauseGate::Probe,
        "production panic recovery must release the dispatched probe lease"
    );
    assert!(
        !queue.has_undispatched_work(),
        "an already-started batch must never re-enter the ordinary retry \
             queue — it was parked directly instead"
    );
    let parked = runtime.park().batches();
    assert_eq!(
        parked.len(),
        1,
        "the panicked batch must be parked, not requeued"
    );
    assert!(
        parked[0].started,
        "batch that produced output before panicking must park with started == true"
    );
    assert!(
        parked[0].needs_review,
        "an already-started parked batch must be held for operator review"
    );
    assert!(
        !parked[0].replay_eligible(),
        "an already-started parked batch must not be auto-replay-eligible"
    );
}

// T16 delta 1, finding 13 (prior #14b): `park_batch` durably writes the
// park file even when its OWN follow-up `batch_parked` ledger record
// fails to append. The batch is not lost — but nothing beyond a log line
// told the operator the audit trail was incomplete. `park_or_fallthrough`
// now checks `write_failures()` and sends the (previously dead-code)
// `state_write_failures` notice on exactly this gap.
#[test]
#[cfg(unix)]
fn park_or_fallthrough_retains_custody_when_audit_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let now = chrono::Utc::now();
    let pubkey = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let mut runtime = reliability::ReliabilityRuntime::open_in(dir.path(), pubkey, now).unwrap();

    let channel_id = Uuid::new_v4();
    let scope = scope::SessionScope::Conversation { channel_id };
    let event = nostr::EventBuilder::new(nostr::Kind::Custom(9), "x")
        .tags([])
        .sign_with_keys(&nostr::Keys::generate())
        .unwrap();
    let batch = FlushBatch {
        batch_id: Uuid::new_v4(),
        channel_id,
        scope,
        events: vec![queue::BatchEvent {
            event,
            prompt_tag: "t".into(),
            received_at: std::time::Instant::now(),
        }],
        cancelled_events: vec![],
        cancel_reason: None,
        started: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    // Ledger unwritable, park file (and its directory) stay writable.
    let ledger_path = dir.path().join("ledger.jsonl");
    std::fs::rename(&ledger_path, dir.path().join("ledger.saved")).unwrap();
    std::fs::create_dir(&ledger_path).unwrap();

    let failures_before = runtime.write_failures();
    let disposition = park_or_fallthrough(
        &mut runtime,
        batch,
        reliability::ParkReason::RetriesExhausted,
        false,
        None,
        now,
    );

    assert!(
        matches!(disposition, Disposition::Fallthrough(_)),
        "failed audit must retain live custody"
    );
    assert!(runtime.park().batches().is_empty());
    let failures_after = runtime.write_failures();
    assert!(
        failures_after.0 > failures_before.0,
        "a ledger append failure inside park_batch must be visible through \
             write_failures(), which is what gates the state_write_failures notice"
    );
}

#[tokio::test]
async fn test_failure_notice_not_consumed_until_ack_received() {
    use crate::error_outcome_emission_tests::dummy_agent;

    let dir = tempfile::tempdir().unwrap();
    let now = chrono::Utc::now();
    let pubkey = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let mut runtime = reliability::ReliabilityRuntime::open_in(dir.path(), pubkey, now).unwrap();

    let channel_id = uuid::Uuid::new_v4();
    let scope = scope::SessionScope::Conversation { channel_id };

    let mut queue = EventQueue::new(config::DedupMode::Queue);
    let event = nostr::EventBuilder::new(nostr::Kind::Custom(9), "work")
        .tags([])
        .sign_with_keys(&nostr::Keys::generate())
        .unwrap();
    queue.push(queue::QueuedEvent {
        channel_id,
        scope: scope.clone(),
        event,
        received_at: std::time::Instant::now(),
        prompt_tag: "p".into(),
    });
    let batch = queue.flush_next().unwrap();

    let mut pool = AgentPool::from_slots(vec![None]);
    let task_id = pool.join_set.spawn(async {}).id();
    pool.task_map_mut().insert(
        task_id,
        crate::pool::TaskMeta {
            agent_index: 0,
            channel_id: Some(channel_id),
            scope: Some(scope.clone()),
            turn_id: "test-turn-id".to_string(),
            recoverable_batch: None,
            control_tx: None,
            steer_tx: None,
            successful_steer_deliveries: std::collections::HashSet::new(),
        },
    );
    let (ack_tx, mut _ack_rx) = tokio::sync::mpsc::unbounded_channel();
    pool.set_notice_ack_tx(ack_tx);

    let agent_for_result = dummy_agent(0).await;
    let result = PromptResult {
        started: false,
        agent: agent_for_result,
        source: PromptSource::Channel(scope.clone()),
        turn_id: "test-turn-id".to_string(),
        outcome: PromptOutcome::Error(acp::AcpError::AgentError {
            code: 429,
            message: "rate limit exceeded".into(),
        }),
        batch: Some(batch),
    };

    let config = super::error_outcome_emission_tests::test_config();
    let mut heartbeat_in_flight = false;
    let removed_channels = HashSet::new();
    let mut crash_history = Vec::new();
    let (respawn_tx, _respawn_rx) = tokio::sync::mpsc::channel(1);
    let mut respawn_tasks = tokio::task::JoinSet::new();

    handle_prompt_result(
        &mut pool,
        &mut queue,
        &config,
        result,
        &mut heartbeat_in_flight,
        &removed_channels,
        &mut crash_history,
        &respawn_tx,
        &mut respawn_tasks,
        None,
        None,
        Some(&mut runtime),
    );

    // Before ack is consumed, pause_needs_notice must still be true!
    assert!(
        runtime.state().pause_needs_notice(channel_id),
        "pause_needs_notice must remain true until notice is successfully posted and acked"
    );

    // Once ack arrives, consume notice and verify pause_needs_notice becomes false
    runtime.state().mark_pause_notice_consumed(channel_id);
    assert!(
        !runtime.state().pause_needs_notice(channel_id),
        "pause_needs_notice must be false after notice is acked and consumed"
    );

    // Verify Breaker notice behavior:
    let breaker_scope = scope::SessionScope::Conversation {
        channel_id: uuid::Uuid::new_v4(),
    };
    for _ in 0..reliability::state::BREAKER_THRESHOLD {
        runtime.state().on_failure(
            &breaker_scope,
            reliability::ErrorClass::ProviderInternal,
            now,
        );
    }
    assert!(
        runtime.state().breaker_needs_notice(&breaker_scope),
        "breaker_needs_notice must be true when breaker opens"
    );
    runtime.state().mark_breaker_notice_consumed(&breaker_scope);
    assert!(
        !runtime.state().breaker_needs_notice(&breaker_scope),
        "breaker_needs_notice must be false after mark_breaker_notice_consumed"
    );
}

#[tokio::test]
async fn test_retry_counts_preserved_across_pause_and_breaker() {
    use crate::error_outcome_emission_tests::dummy_agent;

    let dir = tempfile::tempdir().unwrap();
    let now = chrono::Utc::now();
    let pubkey = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let mut runtime = reliability::ReliabilityRuntime::open_in(dir.path(), pubkey, now).unwrap();

    let channel_id = uuid::Uuid::new_v4();
    let scope = scope::SessionScope::Conversation { channel_id };

    let mut queue = EventQueue::new(config::DedupMode::Queue);

    // 1. Accumulate 2 retries on `scope`
    queue.set_retry_count_for_test(&scope, 2);
    assert_eq!(queue.retry_count(&scope), 2);

    // 2. Trigger Pause on the 3rd attempt
    let event = nostr::EventBuilder::new(nostr::Kind::Custom(9), "work")
        .tags([])
        .sign_with_keys(&nostr::Keys::generate())
        .unwrap();
    queue.push(queue::QueuedEvent {
        channel_id,
        scope: scope.clone(),
        event: event.clone(),
        received_at: std::time::Instant::now(),
        prompt_tag: "p".into(),
    });
    let batch3 = queue.flush_next().unwrap();

    let mut pool = AgentPool::from_slots(vec![None]);
    let task_id = pool.join_set.spawn(async {}).id();
    pool.task_map_mut().insert(
        task_id,
        crate::pool::TaskMeta {
            agent_index: 0,
            channel_id: Some(channel_id),
            scope: Some(scope.clone()),
            turn_id: "test-turn-id".to_string(),
            recoverable_batch: None,
            control_tx: None,
            steer_tx: None,
            successful_steer_deliveries: std::collections::HashSet::new(),
        },
    );

    let agent_for_result = dummy_agent(0).await;
    let result = PromptResult {
        started: false,
        agent: agent_for_result,
        source: PromptSource::Channel(scope.clone()),
        turn_id: "test-turn-id".to_string(),
        outcome: PromptOutcome::Error(acp::AcpError::AgentError {
            code: 429,
            message: "rate limit exceeded".into(),
        }),
        batch: Some(batch3),
    };

    let config = super::error_outcome_emission_tests::test_config();
    let mut heartbeat_in_flight = false;
    let removed_channels = HashSet::new();
    let mut crash_history = Vec::new();
    let (respawn_tx, _respawn_rx) = tokio::sync::mpsc::channel(1);
    let mut respawn_tasks = tokio::task::JoinSet::new();

    handle_prompt_result(
        &mut pool,
        &mut queue,
        &config,
        result,
        &mut heartbeat_in_flight,
        &removed_channels,
        &mut crash_history,
        &respawn_tx,
        &mut respawn_tasks,
        None,
        None,
        Some(&mut runtime),
    );

    // Assert retry count is PRESERVED across Pause (still 2, not reset to 0)
    assert_eq!(
        queue.retry_count(&scope),
        2,
        "retry_count must be preserved across Pause"
    );

    // 3. Resume and trigger another failure: assert retry count continues from 3
    queue.push(queue::QueuedEvent {
        channel_id,
        scope: scope.clone(),
        event: event.clone(),
        received_at: std::time::Instant::now(),
        prompt_tag: "p".into(),
    });
    let batch4 = queue.flush_next().unwrap();
    queue.requeue(batch4);
    assert_eq!(
        queue.retry_count(&scope),
        3,
        "retry_count must continue from 3 after Pause, not reset to 1"
    );

    // 4. Now verify BreakerOpen preserves retry_counts on another scope
    let breaker_channel_id = uuid::Uuid::new_v4();
    let breaker_scope = scope::SessionScope::Conversation {
        channel_id: breaker_channel_id,
    };
    queue.set_retry_count_for_test(&breaker_scope, 2);
    assert_eq!(queue.retry_count(&breaker_scope), 2);

    // Fail until breaker opens: first BREAKER_THRESHOLD - 1 failures
    for _ in 0..(reliability::state::BREAKER_THRESHOLD - 1) {
        runtime.state().on_failure(
            &breaker_scope,
            reliability::ErrorClass::ProviderInternal,
            now,
        );
    }

    // Push and flush a batch that triggers BreakerOpen
    queue.push(queue::QueuedEvent {
        channel_id: breaker_channel_id,
        scope: breaker_scope.clone(),
        event: event.clone(),
        received_at: std::time::Instant::now(),
        prompt_tag: "p".into(),
    });
    let batch_breaker = queue.flush_next().unwrap();

    let task_id2 = pool.join_set.spawn(async {}).id();
    pool.task_map_mut().insert(
        task_id2,
        crate::pool::TaskMeta {
            agent_index: 0,
            channel_id: Some(breaker_channel_id),
            scope: Some(breaker_scope.clone()),
            turn_id: "test-turn-id-2".to_string(),
            recoverable_batch: None,
            control_tx: None,
            steer_tx: None,
            successful_steer_deliveries: std::collections::HashSet::new(),
        },
    );

    let agent2 = dummy_agent(0).await;
    let result2 = PromptResult {
        started: false,
        agent: agent2,
        source: PromptSource::Channel(breaker_scope.clone()),
        turn_id: "test-turn-id-2".to_string(),
        outcome: PromptOutcome::Error(acp::AcpError::AgentError {
            code: 500,
            message: "internal server error".into(),
        }),
        batch: Some(batch_breaker),
    };

    handle_prompt_result(
        &mut pool,
        &mut queue,
        &config,
        result2,
        &mut heartbeat_in_flight,
        &removed_channels,
        &mut crash_history,
        &respawn_tx,
        &mut respawn_tasks,
        None,
        None,
        Some(&mut runtime),
    );

    // Assert retry count is PRESERVED across BreakerOpen (still 2, not reset to 0)
    assert_eq!(
        queue.retry_count(&breaker_scope),
        2,
        "retry_count must be preserved across BreakerOpen"
    );

    // Resume / next failure continues from 3
    queue.push(queue::QueuedEvent {
        channel_id: breaker_channel_id,
        scope: breaker_scope.clone(),
        event: event.clone(),
        received_at: std::time::Instant::now(),
        prompt_tag: "p".into(),
    });
    let batch_breaker_next = queue.flush_next().unwrap();
    queue.requeue(batch_breaker_next);
    assert_eq!(
        queue.retry_count(&breaker_scope),
        3,
        "retry_count must continue from 3 after BreakerOpen, not reset to 1"
    );
}

#[tokio::test]
async fn test_error_boundary_sanitizes_observer_and_ledger() {
    use crate::error_outcome_emission_tests::dummy_agent;

    let dir = tempfile::tempdir().unwrap();
    let now = chrono::Utc::now();
    let pubkey = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let mut runtime = reliability::ReliabilityRuntime::open_in(dir.path(), pubkey, now).unwrap();

    let channel_id = uuid::Uuid::new_v4();
    let scope = scope::SessionScope::Conversation { channel_id };

    let mut queue = EventQueue::new(config::DedupMode::Queue);
    let event = nostr::EventBuilder::new(nostr::Kind::Custom(9), "work")
        .tags([])
        .sign_with_keys(&nostr::Keys::generate())
        .unwrap();
    queue.push(queue::QueuedEvent {
        channel_id,
        scope: scope.clone(),
        event,
        received_at: std::time::Instant::now(),
        prompt_tag: "p".into(),
    });
    let batch = queue.flush_next().unwrap();

    let mut pool = AgentPool::from_slots(vec![None]);
    let task_id = pool.join_set.spawn(async {}).id();
    pool.task_map_mut().insert(
        task_id,
        crate::pool::TaskMeta {
            agent_index: 0,
            channel_id: Some(channel_id),
            scope: Some(scope.clone()),
            turn_id: "test-turn-id".to_string(),
            recoverable_batch: None,
            control_tx: None,
            steer_tx: None,
            successful_steer_deliveries: std::collections::HashSet::new(),
        },
    );

    let secret_key = "sk-ant-secretkey1234567890abcdef";
    let bearer_token = "my-secret-bearer-token";
    let massive_backtrace = "x".repeat(1000);
    let long_msg = format!(
            "provider error: X-API-Key: spaced-secret and token=secret123 and Bearer {} and {} and backtrace: {}",
            bearer_token, secret_key, massive_backtrace
        );
    let err = acp::AcpError::AgentError {
        code: 500,
        message: long_msg,
    };

    let agent = dummy_agent(0).await;
    let result = PromptResult {
        started: false,
        agent,
        source: PromptSource::Channel(scope.clone()),
        turn_id: "test-turn-id".to_string(),
        outcome: PromptOutcome::Error(err),
        batch: Some(batch),
    };

    let config = super::error_outcome_emission_tests::test_config();
    let mut heartbeat_in_flight = false;
    let removed_channels = HashSet::new();
    let mut crash_history = Vec::new();
    let (respawn_tx, _respawn_rx) = tokio::sync::mpsc::channel(1);
    let mut respawn_tasks = tokio::task::JoinSet::new();
    let observer = observer::ObserverHandle::in_process();

    handle_prompt_result(
        &mut pool,
        &mut queue,
        &config,
        result,
        &mut heartbeat_in_flight,
        &removed_channels,
        &mut crash_history,
        &respawn_tx,
        &mut respawn_tasks,
        Some(observer.clone()),
        None,
        Some(&mut runtime),
    );

    // 1. Emitted observer turn_error must be capped <= 512 chars and redacted
    let events = observer.snapshot();
    let turn_error = events
        .iter()
        .find(|e| e.kind == "turn_error")
        .expect("turn_error event must be emitted");
    let emitted_err = turn_error.payload["error"].as_str().unwrap();
    assert!(
        emitted_err.chars().count() <= 512,
        "emitted error must be <= 512 chars, got {}",
        emitted_err.chars().count()
    );
    assert!(
        !emitted_err.contains(secret_key),
        "emitted error must redact secret key"
    );
    assert!(
        !emitted_err.contains(bearer_token),
        "emitted error must redact bearer token"
    );
    assert!(
        !emitted_err.contains("token=secret123"),
        "emitted error must redact token parameter"
    );
    assert!(
        emitted_err.contains("<redacted>"),
        "emitted error must contain <redacted>"
    );

    assert!(!emitted_err.contains("spaced-secret"));

    // 2. Owner-local ledger diagnostics obey the same credential boundary.
    let ledger_content = std::fs::read_to_string(dir.path().join("ledger.jsonl")).unwrap();
    assert!(
        !ledger_content.contains("token=secret123"),
        "ledger must redact credential values"
    );
    assert!(
        !ledger_content.contains(secret_key),
        "ledger must redact secret keys"
    );
    assert!(!ledger_content.contains("spaced-secret"));
}
