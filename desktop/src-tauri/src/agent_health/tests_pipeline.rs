//! Cross-language production pipeline, using synthetic keys and isolated files.
//! This exercises the harness emitter/ledger, NIP44 signed frames, native decrypt
//! command, shipped TS parser, SQLite adapters, reconciliation and summary query.
//! It does not simulate a relay socket, Tauri roster admission or browser routing.
use super::*;
use buzz_acp_pkg::reliability::{
    ledger::{AgentPaused, AgentResumed, TurnFinished},
    ReliabilityRuntime,
};
use buzz_acp_pkg::ObserverHandle;
use buzz_core_pkg::observer::encrypt_observer_payload;
use nostr::{JsonUtil, Keys};
use std::io::Write;
use std::process::{Command, Stdio};
use uuid::Uuid;

fn shipped_parser(envelope: &serde_json::Value) -> HealthFrame {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let module = root
        .join("desktop/src/features/agents/agentHealthFrames.ts")
        .canonicalize()
        .unwrap();
    let mut child = Command::new(root.join("bin/node"))
        .args(["--experimental-strip-types", "--input-type=module", "-e",
            "import {readFileSync} from 'node:fs'; import {pathToFileURL} from 'node:url'; const {parseHealthFrame}=await import(pathToFileURL(process.argv[1]).href); const frame=parseHealthFrame(JSON.parse(readFileSync(0,'utf8'))); if(!frame)throw Error('production parser rejected harness frame'); process.stdout.write(JSON.stringify(frame));"])
        .arg(module)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().expect("the repository-pinned Node runtime must be available");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(envelope).unwrap())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "shipped TS parser failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
async fn health_pipeline_real_ledger_signed_observer_typescript_and_sqlite() {
    let directory = tempfile::tempdir().unwrap();
    let ledger_dir = directory.path().join("harness-state");
    let owner = Keys::generate();
    let agent = Keys::generate();
    let agent_hex = agent.public_key().to_hex();
    let owner_hex = owner.public_key().to_hex();
    let now = chrono::Utc::now() - chrono::Duration::seconds(1);
    let observer = ObserverHandle::in_process();
    let mut runtime = ReliabilityRuntime::open_in(&ledger_dir, &agent_hex, now)
        .unwrap()
        .with_observer(observer.clone());
    let batch = Uuid::new_v4();
    let channel = Uuid::new_v4();
    for (offset, body) in [
        LedgerBody::AgentPaused(AgentPaused {
            class: "capacity_exhausted".into(),
            until: now + chrono::Duration::minutes(5),
            waiting: 1,
        }),
        LedgerBody::AgentResumed(AgentResumed {}),
        LedgerBody::TurnFinished(TurnFinished {
            batch_id: batch,
            channel_id: channel,
            outcome: TurnOutcome::error(
                "provider_error",
                "private diagnostic must not enter observer telemetry",
            ),
        }),
        LedgerBody::TurnFinished(TurnFinished {
            batch_id: Uuid::new_v4(),
            channel_id: channel,
            outcome: TurnOutcome::Ok,
        }),
    ]
    .into_iter()
    .enumerate()
    {
        assert!(runtime.record(now + chrono::Duration::milliseconds(offset as i64), body));
    }
    let ledger_path = ledger_dir.join(LEDGER_FILE);
    let original_ledger = std::fs::read(&ledger_path).unwrap();
    assert!(!original_ledger.is_empty());
    let emitted = observer.snapshot();
    assert_eq!(
        emitted.len(),
        3,
        "successful turns belong only to the ledger, not failure telemetry"
    );

    let state = crate::app_state::build_app_state();
    *state.keys.lock().unwrap() = owner.clone();
    let app = tauri::test::mock_builder()
        .manage(state)
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    let relay = "ws://127.0.0.1:1";
    let path = crate::managed_agents::retention::scoped_db_path(
        directory.path(),
        "agent-health",
        relay,
        &owner_hex,
    );
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let conn = open_db(&path).unwrap();
    let second_path = directory.path().join("ledger-first.sqlite");
    let second = open_db(&second_path).unwrap();
    assert_eq!(
        sync_ledger(&second, &agent_hex, &ledger_path, chrono::Utc::now()).unwrap(),
        4
    );

    for emitted in emitted {
        let encrypted = encrypt_observer_payload(&agent, &owner.public_key(), &emitted).unwrap();
        let signed = buzz_sdk_pkg::build_agent_observer_frame(
            &owner_hex,
            &agent_hex,
            "telemetry",
            &encrypted,
        )
        .unwrap()
        .sign_with_keys(&agent)
        .unwrap();
        assert!(signed.verify().is_ok());
        let decrypted = crate::commands::decrypt_observer_event(signed.as_json(), app.state())
            .await
            .unwrap();
        assert!(!decrypted.to_string().contains("private diagnostic"));
        let parsed = shipped_parser(&decrypted);
        let health = frame_to_health_event(&signed.pubkey.to_hex(), &parsed).unwrap();
        assert!(insert_event(&conn, &health).unwrap());
        assert!(
            !insert_event(&conn, &health).unwrap(),
            "duplicate relay delivery is idempotent"
        );
        assert!(
            !insert_event(&second, &health).unwrap(),
            "ledger-first then observer must deduplicate"
        );
        let mut forged = signed;
        forged.content.push('x');
        assert!(
            crate::commands::decrypt_observer_event(forged.as_json(), app.state())
                .await
                .is_err()
        );
    }
    assert_eq!(sync_ledger(&conn, &agent_hex, &ledger_path, chrono::Utc::now()).unwrap(), 1,
        "observer-first sync adds only the successful turn, not duplicate failures/state transitions");
    assert_eq!(
        sync_ledger(&conn, &agent_hex, &ledger_path, chrono::Utc::now()).unwrap(),
        0
    );
    assert_eq!(
        std::fs::read(&ledger_path).unwrap(),
        original_ledger,
        "desktop reconciliation must not rewrite harness custody"
    );
    drop(conn);
    let reopened = open_db(&path).unwrap();
    let summaries =
        query_agent_health_summary(&reopened, Some(24), chrono::Utc::now().timestamp()).unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].agent, agent_hex);
    assert_eq!(summaries[0].turns, 2);
    assert_eq!(summaries[0].failed, 1);
    assert_eq!(
        summaries[0].last_failure_class.as_deref(),
        Some("provider_error")
    );
    assert_eq!(summaries[0].latest_paused_until, None);
    let other_scope = crate::managed_agents::retention::scoped_db_path(
        directory.path(),
        "agent-health",
        "ws://127.0.0.1:2",
        &owner_hex,
    );
    assert_ne!(path, other_scope);
    assert!(query_agent_health_summary(
        &open_db(&other_scope).unwrap(),
        Some(24),
        chrono::Utc::now().timestamp()
    )
    .unwrap()
    .is_empty());
}
