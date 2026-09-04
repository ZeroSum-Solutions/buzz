//! Command-level tests for `set_prompt_source_and_reload`.
//!
//! These drive the real `#[tauri::command]` over a `MockRuntime` handle — the
//! same function the invoke handler dispatches to — rather than its helpers, so
//! they bind to the production path and cover what only the whole command can
//! show: the result schema the dialog reads, and the ticket's invariant that a
//! failure at **any** boundary leaves the stored mapping and the effective
//! prompt in agreement.
//!
//! The four boundaries, each with its own test:
//!
//! | Boundary | Injected failure | Test |
//! |---|---|---|
//! | validate | CRLF prompt file, refused by `validate_agent_definition_text` | [`a_prompt_the_definition_validator_refuses_leaves_no_mapping`] |
//! | read | the path names no file | [`a_missing_prompt_file_leaves_no_mapping`] |
//! | persona save | the definition is gone / built-in | [`an_unknown_definition_leaves_no_mapping`], [`a_builtin_definition_is_refused`] |
//! | mapping write | the sidecar path is a directory | [`a_failed_mapping_write_is_reported_and_stores_nothing`] |
//!
//! The concurrency guard that the store lock cannot provide on its own — the
//! command reads the definition in one lock hold and saves it in another — is
//! covered by [`a_concurrent_edit_is_refused_rather_than_clobbered`].

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

use super::*;
use crate::app_state::build_app_state;
use crate::managed_agents::{
    load_personas, managed_agents_base_dir, prompt_source::load_prompt_sources_at, save_personas,
    AgentDefinition,
};

/// A temporary `$HOME` plus the process-env lock that makes it safe.
///
/// Tauri resolves `app_data_dir` from `dirs::data_dir()` and the command
/// resolves the home boundary from `dirs::home_dir()`; both read `$HOME`
/// (macOS) / `$XDG_DATA_HOME` (Linux), so the whole test runs inside the
/// tempdir. `Drop` restores the previous values even when a test panics.
struct TempHome {
    _guard: std::sync::MutexGuard<'static, ()>,
    _temp: tempfile::TempDir,
    home: PathBuf,
    old_home: Option<OsString>,
    old_xdg: Option<OsString>,
}

impl TempHome {
    fn new() -> Self {
        let guard = crate::managed_agents::lock_path_mutex();
        let temp = tempfile::tempdir().expect("tempdir");
        let home = std::fs::canonicalize(temp.path())
            .expect("canonicalize temp home")
            .join("home");
        std::fs::create_dir_all(&home).expect("create home");
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_DATA_HOME", &home);
        Self {
            _guard: guard,
            _temp: temp,
            home,
            old_home,
            old_xdg,
        }
    }

    /// Write a prompt file inside the temp home and return its path.
    fn prompt_file(&self, name: &str, contents: &str) -> PathBuf {
        let dir = self.home.join("agent-prompts");
        std::fs::create_dir_all(&dir).expect("create prompt dir");
        let path = dir.join(name);
        std::fs::write(&path, contents).expect("write prompt file");
        path
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        match self.old_home.take() {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match self.old_xdg.take() {
            Some(value) => std::env::set_var("XDG_DATA_HOME", value),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
    }
}

/// A headless app handle whose relay points at a closed local port, so the
/// publish step fails fast and deterministically instead of reaching the
/// network. The persona save it follows is unaffected either way.
fn mock_app() -> tauri::App<tauri::test::MockRuntime> {
    let state = build_app_state();
    *state.keys.lock().unwrap() = nostr::Keys::generate();
    *state.relay_url_override.lock().unwrap() = Some("ws://127.0.0.1:1".to_string());
    tauri::test::mock_builder()
        .manage(state)
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .expect("mock app builds headless")
}

fn definition(id: &str, prompt: &str, is_builtin: bool) -> AgentDefinition {
    AgentDefinition {
        description: None,
        id: id.to_string(),
        display_name: "PM".to_string(),
        avatar_url: None,
        system_prompt: prompt.to_string(),
        runtime: None,
        model: None,
        provider: None,
        name_pool: Vec::new(),
        is_builtin,
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

/// The stored prompt of `id`, read back from disk.
fn stored_prompt(app: &tauri::AppHandle<tauri::test::MockRuntime>, id: &str) -> String {
    load_personas(app)
        .expect("load personas")
        .into_iter()
        .find(|record| record.id == id)
        .expect("definition exists")
        .system_prompt
}

/// The sidecar's mapping for `id`, or `None` when nothing is stored.
fn stored_mapping(app: &tauri::AppHandle<tauri::test::MockRuntime>, id: &str) -> Option<String> {
    let path = managed_agents_base_dir(app)
        .expect("base dir")
        .join("prompt-sources.json");
    load_prompt_sources_at(&path)
        .expect("sidecar reads")
        .get(id)
        .cloned()
}

#[tokio::test]
async fn reload_saves_the_prompt_then_the_mapping_and_reports_the_result() {
    let home = TempHome::new();
    let app = mock_app();
    save_personas(
        app.handle(),
        &[definition("pm", "Old instructions.", false)],
    )
    .expect("seed the definition");
    let file = home.prompt_file("pm.md", "Ship the roadmap.\n");

    let result = set_prompt_source_and_reload(
        "pm".to_string(),
        Some(file.to_string_lossy().into_owned()),
        app.handle().clone(),
    )
    .await
    .expect("a readable prompt file inside home reloads");

    assert!(result.local_updated);
    assert_eq!(result.prompt.as_deref(), Some("Ship the roadmap.\n"));
    assert_eq!(result.path.as_deref(), file.to_str());
    assert_eq!(result.mapping_error, None);
    let publish = result.publish.clone().expect("a reload submits a head");
    assert!(
        publish == "published" || publish == "queued" || publish.starts_with("failed:"),
        "publish must be one of the three documented outcomes, got {publish:?}"
    );

    assert_eq!(stored_prompt(app.handle(), "pm"), "Ship the roadmap.\n");
    assert_eq!(stored_mapping(app.handle(), "pm").as_deref(), file.to_str());

    // The schema the dialog destructures: camelCase, and absent rather than
    // null for the fields a reload does not produce.
    let json = serde_json::to_value(&result).expect("result serializes");
    assert_eq!(json["localUpdated"], serde_json::json!(true));
    assert_eq!(json["prompt"], serde_json::json!("Ship the roadmap.\n"));
    assert!(
        json.get("path").is_some(),
        "a stored mapping reports its path"
    );
    assert!(
        json.get("mappingError").is_none(),
        "mappingError is absent when the mapping was stored"
    );
}

#[tokio::test]
async fn a_prompt_the_definition_validator_refuses_leaves_no_mapping() {
    let home = TempHome::new();
    let app = mock_app();
    save_personas(
        app.handle(),
        &[definition("pm", "Old instructions.", false)],
    )
    .expect("seed the definition");
    // CRLF: `validate_visible_text` allows only \n and \t as layout controls,
    // so this file passes the path/size/UTF-8 gate and is refused by the
    // definition validator inside the persona save — the boundary that used to
    // run *after* the mapping was written.
    let file = home.prompt_file("pm.md", "Ship the roadmap.\r\n");

    let error = set_prompt_source_and_reload(
        "pm".to_string(),
        Some(file.to_string_lossy().into_owned()),
        app.handle().clone(),
    )
    .await
    .expect_err("a prompt the definition validator refuses must fail the command");
    assert!(!error.is_empty());

    assert_eq!(
        stored_prompt(app.handle(), "pm"),
        "Old instructions.",
        "a refused prompt must leave the effective prompt alone"
    );
    assert_eq!(
        stored_mapping(app.handle(), "pm"),
        None,
        "a refused prompt must leave no mapping claiming the file is live"
    );
}

#[tokio::test]
async fn a_missing_prompt_file_leaves_no_mapping() {
    let home = TempHome::new();
    let app = mock_app();
    save_personas(
        app.handle(),
        &[definition("pm", "Old instructions.", false)],
    )
    .expect("seed the definition");
    let missing = home.home.join("agent-prompts").join("absent.md");

    let error = set_prompt_source_and_reload(
        "pm".to_string(),
        Some(missing.to_string_lossy().into_owned()),
        app.handle().clone(),
    )
    .await
    .expect_err("a missing file must fail the command");
    assert!(error.contains("not found"), "got {error:?}");

    assert_eq!(stored_prompt(app.handle(), "pm"), "Old instructions.");
    assert_eq!(stored_mapping(app.handle(), "pm"), None);
}

#[tokio::test]
async fn an_unknown_definition_leaves_no_mapping() {
    let home = TempHome::new();
    let app = mock_app();
    save_personas(
        app.handle(),
        &[definition("pm", "Old instructions.", false)],
    )
    .expect("seed the definition");
    let file = home.prompt_file("pm.md", "Ship the roadmap.\n");

    let error = set_prompt_source_and_reload(
        "ghost".to_string(),
        Some(file.to_string_lossy().into_owned()),
        app.handle().clone(),
    )
    .await
    .expect_err("an unknown definition must fail the command");
    assert!(error.contains("not found"), "got {error:?}");
    assert_eq!(stored_mapping(app.handle(), "ghost"), None);
}

#[tokio::test]
async fn a_builtin_definition_is_refused() {
    let home = TempHome::new();
    let app = mock_app();
    save_personas(
        app.handle(),
        &[definition("pm", "Old instructions.", false)],
    )
    .expect("seed the definition");
    let file = home.prompt_file("pm.md", "Ship the roadmap.\n");

    // Built-ins are seeded by the store itself and are relay-authoritative, so
    // the id is taken from what actually loaded rather than hand-made: only a
    // real built-in id survives `save_personas` with `is_builtin` set.
    let builtin = load_personas(app.handle())
        .expect("load personas")
        .into_iter()
        .find(|record| record.is_builtin)
        .expect("the store seeds built-in definitions");

    let error = set_prompt_source_and_reload(
        builtin.id.clone(),
        Some(file.to_string_lossy().into_owned()),
        app.handle().clone(),
    )
    .await
    .expect_err("built-in definitions are relay-authoritative and cannot be edited");
    assert!(error.contains("Built-in"), "got {error:?}");
    assert_eq!(
        stored_prompt(app.handle(), &builtin.id),
        builtin.system_prompt,
        "a refused built-in keeps its instructions"
    );
    assert_eq!(stored_mapping(app.handle(), &builtin.id), None);
}

#[tokio::test]
async fn a_failed_mapping_write_is_reported_and_stores_nothing() {
    let home = TempHome::new();
    let app = mock_app();
    save_personas(
        app.handle(),
        &[definition("pm", "Old instructions.", false)],
    )
    .expect("seed the definition");
    let file = home.prompt_file("pm.md", "Ship the roadmap.\n");

    // Make the sidecar path unwritable by putting a directory where the file
    // belongs. This is the last boundary: the prompt is already durable.
    let sidecar = managed_agents_base_dir(app.handle())
        .expect("base dir")
        .join("prompt-sources.json");
    std::fs::create_dir_all(&sidecar).expect("occupy the sidecar path");

    let result = set_prompt_source_and_reload(
        "pm".to_string(),
        Some(file.to_string_lossy().into_owned()),
        app.handle().clone(),
    )
    .await
    .expect("the prompt save landed, so the command reports rather than fails");

    assert!(result.local_updated);
    assert_eq!(
        stored_prompt(app.handle(), "pm"),
        "Ship the roadmap.\n",
        "the prompt is durable before the mapping is attempted"
    );
    assert_eq!(
        result.path, None,
        "path is reported only when a mapping was actually stored"
    );
    assert!(
        result
            .mapping_error
            .as_deref()
            .is_some_and(|error| error.contains("prompt-sources")),
        "the failed mapping write must be reported, got {:?}",
        result.mapping_error
    );
}

#[tokio::test]
async fn clearing_removes_the_mapping_and_leaves_the_prompt() {
    let home = TempHome::new();
    let app = mock_app();
    save_personas(
        app.handle(),
        &[definition("pm", "Old instructions.", false)],
    )
    .expect("seed the definition");
    let file = home.prompt_file("pm.md", "Ship the roadmap.\n");
    set_prompt_source_and_reload(
        "pm".to_string(),
        Some(file.to_string_lossy().into_owned()),
        app.handle().clone(),
    )
    .await
    .expect("bind the prompt file");

    let result = set_prompt_source_and_reload("pm".to_string(), None, app.handle().clone())
        .await
        .expect("clearing always succeeds");

    assert!(!result.local_updated);
    assert_eq!(result.publish, None, "a clear submits nothing");
    assert_eq!(result.path, None);
    assert_eq!(stored_mapping(app.handle(), "pm"), None);
    assert_eq!(
        stored_prompt(app.handle(), "pm"),
        "Ship the roadmap.\n",
        "unbinding drops the claim, not the instructions"
    );
}

#[tokio::test]
async fn clearing_a_definition_that_was_never_bound_is_a_no_op() {
    let _home = TempHome::new();
    let app = mock_app();
    save_personas(
        app.handle(),
        &[definition("pm", "Old instructions.", false)],
    )
    .expect("seed the definition");

    // The dialog cannot read the sidecar back, so Clear is offered without
    // knowing whether a binding exists. Clearing nothing must succeed, which is
    // what makes a dead or moved prompt source removable from the UI.
    let result = set_prompt_source_and_reload("pm".to_string(), None, app.handle().clone())
        .await
        .expect("clearing an unbound definition is allowed");
    assert!(!result.local_updated);
    assert_eq!(stored_mapping(app.handle(), "pm"), None);
}

#[tokio::test]
async fn a_concurrent_edit_is_refused_rather_than_clobbered() {
    let _home = TempHome::new();
    let app = mock_app();
    save_personas(
        app.handle(),
        &[definition("pm", "Old instructions.", false)],
    )
    .expect("seed the definition");

    // What the command's read pass captures.
    let read = load_personas(app.handle())
        .expect("read the definition")
        .into_iter()
        .find(|record| record.id == "pm")
        .expect("definition exists");
    let request = crate::managed_agents::prompt_source::update_request_from_definition(
        &crate::managed_agents::prompt_source::definition_with_prompt(&read, "From the file.\n"),
    );

    // A concurrent dialog save lands in the window between the two lock holds.
    let mut concurrent = read.clone();
    concurrent.display_name = "Renamed by someone else".to_string();
    concurrent.updated_at = crate::util::now_iso();
    save_personas(app.handle(), &[concurrent]).expect("concurrent edit lands");

    let error = super::super::update::update_persona_with_precondition(
        request,
        Some(read.updated_at.clone()),
        app.handle().clone(),
        |_app, _state, _persona| Ok(()),
    )
    .await
    .expect_err("a request built from a stale read must not overwrite a newer edit");
    assert!(error.contains("was edited while"), "got {error:?}");

    let after = load_personas(app.handle())
        .expect("reload")
        .into_iter()
        .find(|record| record.id == "pm")
        .expect("definition exists");
    assert_eq!(
        after.display_name, "Renamed by someone else",
        "the concurrent edit must survive"
    );
    assert_eq!(
        after.system_prompt, "Old instructions.",
        "the refused request must write nothing at all"
    );
}
