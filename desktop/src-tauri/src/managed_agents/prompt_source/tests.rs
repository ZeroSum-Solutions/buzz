use std::collections::BTreeMap;

use super::*;
use crate::managed_agents::{
    persona_events::{build_persona_event, persona_content_hash, persona_event_content},
    AgentDefinition,
};

/// A home directory plus the sidecar path inside it, canonicalized the same way
/// production canonicalizes so `/var` vs `/private/var` cannot skew a test.
struct Fixture {
    _dir: tempfile::TempDir,
    home: std::path::PathBuf,
    store: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = std::fs::canonicalize(dir.path()).expect("canonicalize home");
    let store = home.join("prompt-sources.json");
    Fixture {
        _dir: dir,
        home,
        store,
    }
}

fn definition(system_prompt: &str) -> AgentDefinition {
    AgentDefinition {
        description: None,
        id: "pm".to_string(),
        display_name: "PM".to_string(),
        avatar_url: None,
        system_prompt: system_prompt.to_string(),
        runtime: None,
        model: None,
        provider: None,
        name_pool: Vec::new(),
        is_builtin: false,
        is_active: true,
        shared: false,
        source_team: None,
        source_team_persona_slug: None,
        catalog_source: None,
        team_catalog_source: None,
        env_vars: BTreeMap::new(),
        respond_to: None,
        respond_to_allowlist: Vec::new(),
        parallelism: None,
        created_at: "2026-09-04T00:00:00Z".to_string(),
        updated_at: "2026-09-04T00:00:00Z".to_string(),
    }
}

/// Prepare a prompt source and, when it resolves, commit its mapping — the two
/// production steps in the order the command runs them, minus the persona save
/// that sits between. Every path-validation test goes through this so a rule
/// that only holds in preparation cannot pass by accident.
fn prepare_and_commit(
    store: &std::path::Path,
    definition_id: &str,
    raw_path: Option<&str>,
    home: &std::path::Path,
) -> Result<PromptSourceChange, String> {
    let change = prepare_prompt_source(raw_path, home)?;
    match &change {
        PromptSourceChange::Cleared => commit_prompt_source_at(store, definition_id, None)?,
        PromptSourceChange::Loaded { path, .. } => {
            commit_prompt_source_at(store, definition_id, Some(path))?
        }
    }
    Ok(change)
}

#[test]
fn preparing_a_prompt_source_writes_nothing() {
    let f = fixture();
    let path = f.home.join("pm.md");
    std::fs::write(&path, "Be the PM.").expect("write prompt");

    let change =
        prepare_prompt_source(Some(path.to_str().expect("utf-8")), &f.home).expect("prepare");
    assert!(matches!(change, PromptSourceChange::Loaded { .. }));
    assert!(
        !f.store.exists(),
        "preparation must not write the sidecar: the mapping is committed only \
         after the persona save, so an intervening failure leaves no mapping"
    );

    commit_prompt_source_at(&f.store, "pm", Some(&path)).expect("commit the mapping");
    assert_eq!(
        load_prompt_sources_at(&f.store)
            .expect("reload")
            .get("pm")
            .map(String::as_str),
        Some(path.to_str().expect("utf-8")),
        "committing after the save is what stores the mapping"
    );
}

#[test]
fn committing_a_clear_for_an_unmapped_id_is_a_no_op() {
    let f = fixture();
    // The dialog offers Clear without knowing whether a mapping exists, so an
    // unbind of an id that has none must succeed quietly rather than error.
    commit_prompt_source_at(&f.store, "pm", None).expect("clearing an unmapped id is allowed");
    assert!(
        !f.store.exists(),
        "clearing nothing must not create the sidecar"
    );
}

#[test]
fn missing_file_is_rejected() {
    let f = fixture();
    let missing = f.home.join("agent-prompts").join("pm.md");
    let error = prepare_and_commit(
        &f.store,
        "pm",
        Some(missing.to_str().expect("utf-8 path")),
        &f.home,
    )
    .expect_err("a missing prompt file must be refused");
    assert!(
        error.contains("not found"),
        "error should name the missing file, got {error:?}"
    );
    assert!(
        !f.store.exists(),
        "a refused path must not create the sidecar"
    );
}

#[cfg(unix)]
#[test]
fn symlink_escaping_home_is_rejected() {
    let f = fixture();
    let outside_dir = tempfile::tempdir().expect("outside tempdir");
    let outside = std::fs::canonicalize(outside_dir.path())
        .expect("canonicalize outside")
        .join("secret.md");
    std::fs::write(&outside, "secret").expect("write outside file");

    let link = f.home.join("pm.md");
    std::os::unix::fs::symlink(&outside, &link).expect("symlink");

    let error = prepare_and_commit(&f.store, "pm", Some(link.to_str().expect("utf-8")), &f.home)
        .expect_err("a symlink resolving outside home must be refused");
    assert!(
        error.contains("home"),
        "error should explain the home boundary, got {error:?}"
    );
}

#[test]
fn over_limit_file_is_rejected() {
    let f = fixture();
    let path = f.home.join("pm.md");
    let oversized = "a".repeat(MAX_PROMPT_SOURCE_BYTES + 1);
    std::fs::write(&path, &oversized).expect("write oversized file");

    let error = prepare_and_commit(&f.store, "pm", Some(path.to_str().expect("utf-8")), &f.home)
        .expect_err("a file over the size cap must be refused");
    assert!(
        error.contains("too large"),
        "error should name the size cap, got {error:?}"
    );
}

#[test]
fn exactly_at_the_limit_is_accepted() {
    let f = fixture();
    let path = f.home.join("pm.md");
    let at_cap = "a".repeat(MAX_PROMPT_SOURCE_BYTES);
    std::fs::write(&path, &at_cap).expect("write at-cap file");

    let change = prepare_and_commit(&f.store, "pm", Some(path.to_str().expect("utf-8")), &f.home)
        .expect("a file exactly at the cap is within 'at most 64 KiB'");
    match change {
        PromptSourceChange::Loaded { prompt, .. } => assert_eq!(prompt.len(), at_cap.len()),
        PromptSourceChange::Cleared => panic!("expected a loaded prompt"),
    }
}

#[test]
fn invalid_utf8_is_rejected() {
    let f = fixture();
    let path = f.home.join("pm.md");
    std::fs::write(&path, [0x68, 0x69, 0xff, 0xfe]).expect("write invalid utf-8");

    let error = prepare_and_commit(&f.store, "pm", Some(path.to_str().expect("utf-8")), &f.home)
        .expect_err("a non-UTF-8 prompt file must be refused");
    assert!(
        error.contains("UTF-8"),
        "error should name the encoding, got {error:?}"
    );
}

#[test]
fn clearing_removes_the_mapping() {
    let f = fixture();
    let path = f.home.join("pm.md");
    std::fs::write(&path, "Be the PM.").expect("write prompt");
    prepare_and_commit(&f.store, "pm", Some(path.to_str().expect("utf-8")), &f.home)
        .expect("store the mapping");
    prepare_and_commit(
        &f.store,
        "other",
        Some(path.to_str().expect("utf-8")),
        &f.home,
    )
    .expect("store a second mapping");

    let change = prepare_and_commit(&f.store, "pm", None, &f.home).expect("clear the mapping");
    assert!(matches!(change, PromptSourceChange::Cleared));

    let stored = load_prompt_sources_at(&f.store).expect("reload sidecar");
    assert!(!stored.contains_key("pm"), "the cleared id must be gone");
    assert!(
        stored.contains_key("other"),
        "clearing one id must not disturb another"
    );
}

#[test]
fn happy_path_stores_the_mapping_and_returns_the_file_text() {
    let f = fixture();
    let dir = f.home.join("agent-prompts");
    std::fs::create_dir_all(&dir).expect("create prompt dir");
    let path = dir.join("pm.md");
    std::fs::write(&path, "Ship the roadmap.\n").expect("write prompt");

    let change = prepare_and_commit(&f.store, "pm", Some(path.to_str().expect("utf-8")), &f.home)
        .expect("store and read the prompt");
    let PromptSourceChange::Loaded {
        prompt,
        path: resolved,
    } = change
    else {
        panic!("expected a loaded prompt");
    };
    assert_eq!(prompt, "Ship the roadmap.\n");
    assert_eq!(resolved, path);

    let stored = load_prompt_sources_at(&f.store).expect("reload sidecar");
    assert_eq!(
        stored.get("pm").map(String::as_str),
        Some(path.to_str().expect("utf-8")),
        "the sidecar keeps the resolved absolute path"
    );
}

#[test]
fn applying_the_prompt_updates_system_prompt_and_the_content_hash() {
    let before = definition("Old instructions.");
    let after = definition_with_prompt(&before, "Ship the roadmap.\n");

    assert_eq!(after.system_prompt, "Ship the roadmap.\n");
    assert_ne!(
        persona_content_hash(&persona_event_content(&before)),
        persona_content_hash(&persona_event_content(&after)),
        "a reloaded prompt must move the drift hash so linked agents badge a restart"
    );

    let request = update_request_from_definition(&after);
    assert_eq!(
        request.system_prompt, after.system_prompt,
        "the update request the command submits must carry the reloaded prompt"
    );
    assert!(
        request.env_vars.is_none() && request.behavior.is_none(),
        "a prompt reload must not rewrite stored env vars or the behavior group"
    );
}

#[test]
fn published_prompt_source_event_carries_the_prompt_and_not_the_path() {
    let path_text = "/Users/someone/agent-prompts/pm.md";
    let after = definition_with_prompt(&definition("Old."), "Ship the roadmap.\n");

    let keys = nostr::Keys::generate();
    let event = build_persona_event(&after)
        .expect("build persona event")
        .sign_with_keys(&keys)
        .expect("sign persona event");

    assert_eq!(event.kind.as_u16(), 30175);
    let content: crate::managed_agents::persona_events::PersonaEventContent =
        serde_json::from_str(&event.content).expect("content deserializes as PersonaEventContent");
    assert_eq!(
        content.system_prompt.as_deref(),
        Some("Ship the roadmap.\n")
    );
    assert!(
        !event.content.contains(path_text) && !event.content.contains("agent-prompts"),
        "the published event must carry the prompt text, never the local file path"
    );
}

#[test]
fn relative_and_blank_paths_are_rejected() {
    let f = fixture();
    let relative = prepare_and_commit(&f.store, "pm", Some("agent-prompts/pm.md"), &f.home)
        .expect_err("a relative path must be refused");
    assert!(
        relative.contains("absolute"),
        "error should ask for an absolute path, got {relative:?}"
    );

    let blank = prepare_and_commit(&f.store, "pm", Some("   "), &f.home)
        .expect_err("a blank path must be refused");
    assert!(
        blank.contains("required"),
        "error should say a path is required, got {blank:?}"
    );
}

#[test]
fn a_directory_is_rejected() {
    let f = fixture();
    let dir = f.home.join("agent-prompts");
    std::fs::create_dir_all(&dir).expect("create dir");

    let error = prepare_and_commit(&f.store, "pm", Some(dir.to_str().expect("utf-8")), &f.home)
        .expect_err("a directory is not a prompt file");
    assert!(
        error.contains("file"),
        "error should say a regular file is required, got {error:?}"
    );
}

#[test]
fn a_corrupt_sidecar_is_reported_not_silently_discarded() {
    let f = fixture();
    std::fs::write(&f.store, "{ not json").expect("write corrupt sidecar");
    let error = load_prompt_sources_at(&f.store).expect_err("corrupt sidecar must surface");
    assert!(
        error.contains("prompt-sources"),
        "error should name the sidecar, got {error:?}"
    );
}

/// The delivery seam, desktop half: what a restart hands the harness.
///
/// A restart re-resolves the effective config and writes
/// `BUZZ_ACP_SYSTEM_PROMPT` from `effective_cfg.system_prompt`
/// (`runtime.rs:650`). This asserts that value is the prompt file's bytes,
/// unchanged — no trimming, no line-ending rewrite, and the instance's own
/// stale prompt bytes never win for a linked agent. The harness half of the
/// same seam (those bytes reaching the ACP `session/new` request) is
/// `session_new_delivers_reloaded_prompt_source_file_bytes` in
/// `crates/buzz-acp/src/acp.rs`.
#[test]
fn reloaded_prompt_bytes_reach_the_spawn_env_after_a_restart() {
    use crate::managed_agents::effective_config::resolve_effective_config;
    use crate::managed_agents::global_config::GlobalAgentConfig;

    let f = fixture();
    let file_text = "You are the PM.\n\n\tKeep a decision log — ünïcode, emoji 🐝.\n";
    let path = f.home.join("pm.md");
    std::fs::write(&path, file_text).expect("write prompt");

    let PromptSourceChange::Loaded { prompt, .. } =
        prepare_prompt_source(Some(path.to_str().expect("utf-8")), &f.home).expect("prepare")
    else {
        panic!("expected a loaded prompt");
    };
    let reloaded = definition_with_prompt(&definition("Old instructions."), &prompt);

    let mut record = linked_record("pm");
    // A linked instance carries its own legacy prompt bytes; the definition is
    // authoritative, so a reload must still be what the restart delivers.
    record.system_prompt = Some("Stale instance instructions.".to_string());

    let effective = resolve_effective_config(&record, &[reloaded], &GlobalAgentConfig::default())
        .require_resolved()
        .expect("a linked record with a live definition resolves");

    assert_eq!(
        effective.system_prompt.value.as_deref(),
        Some(file_text),
        "the value a restart writes to BUZZ_ACP_SYSTEM_PROMPT must be the file's bytes"
    );
}

/// A managed-agent record linked to `persona_id`, with only the fields this
/// module's assertions read set to anything meaningful.
fn linked_record(persona_id: &str) -> crate::managed_agents::ManagedAgentRecord {
    use crate::managed_agents::{BackendKind, ManagedAgentRecord, RespondTo};
    ManagedAgentRecord {
        description: None,
        pubkey: "agent-pk".to_string(),
        name: "PM".to_string(),
        persona_id: Some(persona_id.to_string()),
        private_key_nsec: String::new(),
        auth_tag: None,
        relay_url: "ws://localhost:3000".to_string(),
        avatar_url: None,
        acp_command: "buzz-acp".to_string(),
        agent_command: "goose".to_string(),
        agent_command_override: None,
        agent_args: vec![],
        mcp_command: String::new(),
        turn_timeout_seconds: 300,
        idle_timeout_seconds: None,
        max_turn_duration_seconds: None,
        parallelism: 1,
        system_prompt: None,
        model: None,
        provider: None,
        persona_source_version: None,
        env_vars: BTreeMap::new(),
        start_on_app_launch: false,
        runtime_pid: None,
        backend: BackendKind::Local,
        backend_agent_id: None,
        provider_policy_pending: false,
        provider_binary_path: None,
        team_id: None,
        persona_team_dir: None,
        persona_name_in_team: None,
        created_at: String::new(),
        updated_at: String::new(),
        last_started_at: None,
        last_stopped_at: None,
        last_exit_code: None,
        last_error: None,
        last_error_code: None,
        respond_to: RespondTo::OwnerOnly,
        respond_to_allowlist: vec![],
        display_name: None,
        slug: None,
        runtime: None,
        name_pool: vec![],
        is_builtin: false,
        is_active: true,
        shared: false,
        source_team: None,
        source_team_persona_slug: None,
        catalog_source: None,
        team_catalog_source: None,
        relay_mesh: None,
        effort_level: None,
        auto_restart_on_config_change: false,
        definition_respond_to: None,
        definition_respond_to_allowlist: vec![],
        definition_parallelism: None,
    }
}
