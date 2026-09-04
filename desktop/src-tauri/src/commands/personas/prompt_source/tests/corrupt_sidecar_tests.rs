//! A sidecar nothing can parse must not veto an authoritative deletion.
//!
//! `prompt-sources.json` is optional machine-local convenience metadata. Every
//! path that destroys a definition retracts its binding first
//! (`forget_prompt_source`, Review-Proven Rule 2) — and until
//! `retract_prompt_source_at` existed, that retraction read the whole map
//! before it could drop one key, so a torn write, a hand edit or a third-party
//! tool that left the file unparseable propagated `?` straight back out of:
//!
//! - `delete_persona` — the agent could not be deleted at all;
//! - the inbound kind:5 tombstone (covered by
//!   `inbound::tombstone_prompt_source_tests`) — a signed deletion from another
//!   device never applied, and failed again on every boot, so the agent kept
//!   coming back;
//! - `delete_team_with_cascade` — the cascade died part-way.
//!
//! These drive the real commands, not the helper: restoring
//! `commit_prompt_source_at` in `forget_prompt_source` turns both RED.
//!
//! The corrupt file itself is deliberately left on disk. It is not lost data
//! the deletion is entitled to discard, and skipping the retraction is only
//! safe because *no* read path resolves a binding out of an unparseable
//! sidecar — so the leftover entry cannot rebind the next definition to take a
//! reused id, which is the hazard the retraction exists to prevent. The
//! operator's way back is still `reset_prompt_sources`, which renames it.

use super::*;

/// Bytes that are not JSON, written where the sidecar lives.
fn corrupt_sidecar(app: &tauri::AppHandle<tauri::test::MockRuntime>) -> PathBuf {
    let path = managed_agents_base_dir(app)
        .expect("base dir")
        .join("prompt-sources.json");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("create the store dir");
    std::fs::write(&path, "{ not json").expect("corrupt the sidecar");
    path
}

#[tokio::test]
async fn deleting_an_agent_survives_a_sidecar_no_reader_can_parse() {
    let _home = TempHome::new();
    let app = mock_app();
    let handle = app.handle().clone();
    save_personas(&handle, &[definition("pm", "Old instructions.", false)])
        .expect("seed the definition");
    let sidecar = corrupt_sidecar(&handle);

    crate::commands::personas::delete_persona("pm".to_string(), handle.clone())
        .await
        .expect("an unreadable convenience sidecar must not block deleting an agent");

    assert!(
        load_personas(&handle)
            .expect("load personas")
            .iter()
            .all(|record| record.id != "pm"),
        "the deletion the user asked for must land"
    );
    assert_eq!(
        std::fs::read_to_string(&sidecar).expect("the sidecar is still there"),
        "{ not json",
        "the unreadable file is left exactly as it was — rewriting it from an \
         assumed-empty map would destroy every other agent's binding, and the \
         reset is the only path allowed to move it"
    );
}

#[test]
fn a_team_cascade_survives_a_sidecar_no_reader_can_parse() {
    let _home = TempHome::new();
    let app = mock_app();
    let handle = app.handle().clone();

    let mut member = definition("corrupt-pack-pm", "Instructions from the file.\n", false);
    member.source_team = Some("corrupt-pack".to_string());
    save_personas(&handle, &[member]).expect("seed the member definition");
    crate::managed_agents::save_teams(
        &handle,
        &[crate::managed_agents::TeamRecord {
            id: "corrupt-pack-team".to_string(),
            name: "Corrupt Pack".to_string(),
            description: None,
            instructions: None,
            persona_ids: vec!["corrupt-pack-pm".to_string()],
            is_builtin: false,
            shared: false,
            catalog_source: None,
            // Never created on disk: the cascade removes the directory only
            // when it exists, and this test is about the sidecar.
            source_dir: Some(
                managed_agents_base_dir(&handle)
                    .expect("base dir")
                    .join("corrupt-pack"),
            ),
            is_symlink: false,
            symlink_target: None,
            version: None,
            created_at: "2026-09-04T00:00:00Z".to_string(),
            updated_at: "2026-09-04T00:00:00Z".to_string(),
        }],
    )
    .expect("seed the team");
    corrupt_sidecar(&handle);

    crate::managed_agents::delete_team_with_cascade(&handle, "corrupt-pack-team")
        .expect("an unreadable convenience sidecar must not block a team cascade");

    assert!(
        load_personas(&handle)
            .expect("load personas")
            .iter()
            .all(|record| record.id != "corrupt-pack-pm"),
        "the cascade must remove the member definition"
    );
    assert!(
        crate::managed_agents::load_teams(&handle)
            .expect("load teams")
            .iter()
            .all(|record| record.id != "corrupt-pack-team"),
        "and the team itself"
    );
}
