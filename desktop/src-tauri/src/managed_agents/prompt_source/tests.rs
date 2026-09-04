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
        PromptSourceChange::Loaded { path, prompt } => commit_prompt_source_at(
            store,
            definition_id,
            Some((path.as_path(), prompt.as_str())),
        )?,
    }
    Ok(change)
}

/// The path stored for `id`, ignoring the digest beside it.
fn stored_path(store: &std::path::Path, id: &str) -> Option<String> {
    load_prompt_sources_at(store)
        .expect("sidecar reads")
        .get(id)
        .map(|stored| stored.path.clone())
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

    commit_prompt_source_at(&f.store, "pm", Some((path.as_path(), "Be the PM.")))
        .expect("commit the mapping");
    assert_eq!(
        stored_path(&f.store, "pm").as_deref(),
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

    assert_eq!(
        stored_path(&f.store, "pm").as_deref(),
        Some(path.to_str().expect("utf-8")),
        "the sidecar keeps the resolved absolute path"
    );
    assert_eq!(
        load_prompt_sources_at(&f.store)
            .expect("reload sidecar")
            .get("pm")
            .map(|stored| stored.prompt_sha256.clone()),
        Some(prompt_digest("Ship the roadmap.\n")),
        "the entry records the prompt it was read from, so a later read can tell \
         whether the definition still holds that text"
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

#[test]
fn a_stored_binding_reads_back_and_an_unbound_definition_reads_none() {
    let f = fixture();
    let path = f.home.join("pm.md");
    std::fs::write(&path, "Be the PM.").expect("write prompt");

    assert_eq!(
        prompt_source_binding_at(&f.store, "pm", Some("Be the PM."))
            .expect("a missing sidecar is no binding"),
        None,
        "nothing is bound before a commit"
    );

    prepare_and_commit(&f.store, "pm", Some(path.to_str().expect("utf-8")), &f.home)
        .expect("bind the prompt file");

    assert_eq!(
        prompt_source_binding_at(&f.store, "pm", Some("Be the PM.")).expect("sidecar reads"),
        Some(PromptSourceBinding {
            path: path.to_string_lossy().into_owned(),
            in_sync: true,
        }),
        "the stored path must read back, or the dialog has to make the user retype it"
    );
    assert_eq!(
        prompt_source_binding_at(&f.store, "designer", Some("Be the PM.")).expect("sidecar reads"),
        None,
        "a definition with no entry is unbound, not the first entry"
    );

    prepare_and_commit(&f.store, "pm", None, &f.home).expect("clear the binding");
    assert_eq!(
        prompt_source_binding_at(&f.store, "pm", Some("Be the PM.")).expect("sidecar reads"),
        None,
        "a cleared binding must read back as unbound"
    );
}

/// A binding is a claim about the definition's current text, so the read must
/// check it. Nothing else in the app tells the sidecar that some other path
/// rewrote the prompt.
#[test]
fn a_binding_whose_prompt_no_longer_matches_reads_back_out_of_sync() {
    let f = fixture();
    let path = f.home.join("pm.md");
    std::fs::write(&path, "Be the PM.").expect("write prompt");
    prepare_and_commit(&f.store, "pm", Some(path.to_str().expect("utf-8")), &f.home)
        .expect("bind the prompt file");

    assert!(
        !prompt_source_binding_at(&f.store, "pm", Some("Typed over by hand."))
            .expect("sidecar reads")
            .expect("the entry is still stored")
            .in_sync,
        "a prompt that no longer equals the file's text must not be reported as loaded from it"
    );
    assert!(
        !prompt_source_binding_at(&f.store, "pm", None)
            .expect("sidecar reads")
            .expect("an orphaned entry is still visible, so it can be cleared")
            .in_sync,
        "a binding whose definition is gone cannot be in sync with it"
    );
}

#[test]
fn reading_a_binding_from_a_corrupt_sidecar_is_an_error_not_unbound() {
    let f = fixture();
    std::fs::write(&f.store, "{ not json").expect("write corrupt sidecar");
    let error = prompt_source_binding_at(&f.store, "pm", Some("Be the PM."))
        .expect_err("a corrupt sidecar must not read as unbound");
    assert!(
        error.contains("prompt-sources"),
        "error should name the sidecar, got {error:?}"
    );
}

/// The way out of the state every other control refuses. `Clear` reads the
/// sidecar before it can remove one entry, so on a malformed file it fails
/// exactly as the seed did; the reset moves the file aside instead.
#[test]
fn resetting_quarantines_an_unreadable_sidecar_and_leaves_a_readable_one_alone() {
    let f = fixture();
    let path = f.home.join("pm.md");
    std::fs::write(&path, "Be the PM.").expect("write prompt");

    assert!(
        quarantine_prompt_sources_at(&f.store).is_err(),
        "there is nothing to reset before a sidecar exists"
    );

    prepare_and_commit(&f.store, "pm", Some(path.to_str().expect("utf-8")), &f.home)
        .expect("bind the prompt file");
    let refused = quarantine_prompt_sources_at(&f.store)
        .expect_err("a readable sidecar must not be resettable");
    assert!(
        refused.contains("Clear"),
        "the refusal should point at the per-agent action, got {refused:?}"
    );
    assert!(
        f.store.exists(),
        "a readable sidecar must survive untouched"
    );

    std::fs::write(&f.store, "{ not json").expect("corrupt the sidecar");
    assert!(
        commit_prompt_source_at(&f.store, "pm", None).is_err(),
        "Clear cannot recover a corrupt sidecar — that is why the reset exists"
    );

    let quarantined = quarantine_prompt_sources_at(&f.store).expect("the reset succeeds");
    assert!(
        quarantined.exists(),
        "the unreadable file is moved aside, never deleted"
    );
    assert_eq!(
        std::fs::read_to_string(&quarantined).expect("the quarantined file reads"),
        "{ not json",
        "the operator can still inspect exactly what was on disk"
    );
    assert_eq!(
        load_prompt_sources_at(&f.store).expect("the store reads again"),
        PromptSourceMap::new(),
        "after the reset the store is usable again, with every binding gone"
    );
}

/// The retraction half, which the three deletion paths call and `Clear` does
/// not: it must not fail on a sidecar nothing can parse.
///
/// The split is the point. `Clear` is a user action on one agent, so
/// `commit_prompt_source_at` still refuses an unreadable file and the dialog
/// offers the reset (asserted just above). `retract_prompt_source_at` is
/// bookkeeping in front of an authoritative deletion, and there the same
/// refusal would let optional machine-local metadata veto the deletion itself.
///
/// Skipping is only safe because the leftover entry is unreachable: every read
/// path refuses the file, so nothing can resolve it into a binding on the next
/// definition to take a reused id.
#[test]
fn retraction_tolerates_a_corrupt_sidecar_while_clear_still_refuses_one() {
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

    // On a readable sidecar the retraction behaves exactly like Clear.
    retract_prompt_source_at(&f.store, "pm").expect("retracting a stored binding succeeds");
    assert_eq!(stored_path(&f.store, "pm"), None, "the entry is gone");
    assert!(
        stored_path(&f.store, "other").is_some(),
        "retracting one id must not disturb another"
    );

    std::fs::write(&f.store, "{ not json").expect("corrupt the sidecar");
    assert!(
        commit_prompt_source_at(&f.store, "other", None).is_err(),
        "Clear must still report a sidecar it cannot read — the reset is the way out"
    );
    retract_prompt_source_at(&f.store, "other")
        .expect("a deletion's retraction must not be vetoed by an unreadable sidecar");
    assert_eq!(
        std::fs::read_to_string(&f.store).expect("the sidecar is still there"),
        "{ not json",
        "the unreadable file is left untouched for the reset, never rewritten \
         from an assumed-empty map"
    );
}
