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

/// A corrupt sidecar must not veto the deletions that retract a binding.
/// Split out to keep this file under the Rust size ratchet.
mod corrupt_sidecar_tests;

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
        .map(|stored| stored.path.clone())
}

/// Register a one-shot hook to run in the window between the command's read
/// pass and its save pass, keyed by definition id.
///
/// The command cannot hold the store lock across both passes, because the
/// update path takes it itself. That window is the whole reason
/// `update_persona_with_precondition` exists, and a test that cannot land a
/// writer inside it can only assert the precondition helper, never the command
/// — which is how the guard came to be removable without failing anything.
/// Keyed by id so tests running in parallel cannot arm each other's barrier.
fn arm_read_save_barrier(definition_id: &str, barrier: impl FnOnce() + Send + 'static) {
    arm_barrier(read_save_barriers(), definition_id, barrier);
}

/// Register a one-shot hook to run in the window between the command's persona
/// save and its sidecar write, keyed by definition id.
///
/// The second window the store lock cannot cover: the update path releases the
/// lock when it returns, and `commit_mapping` retakes it. A writer landing here
/// makes the effective prompt something this reload never read, which is the
/// only thing that can make the returned binding's `in_sync` disagree with the
/// sidecar's digest.
fn arm_save_mapping_barrier(definition_id: &str, barrier: impl FnOnce() + Send + 'static) {
    arm_barrier(save_mapping_barriers(), definition_id, barrier);
}

type Barrier = Box<dyn FnOnce() + Send>;
type BarrierRegistry = std::sync::Mutex<std::collections::HashMap<String, Barrier>>;

fn read_save_barriers() -> &'static BarrierRegistry {
    static BARRIERS: std::sync::OnceLock<BarrierRegistry> = std::sync::OnceLock::new();
    BARRIERS.get_or_init(Default::default)
}

fn save_mapping_barriers() -> &'static BarrierRegistry {
    static BARRIERS: std::sync::OnceLock<BarrierRegistry> = std::sync::OnceLock::new();
    BARRIERS.get_or_init(Default::default)
}

fn arm_barrier(
    registry: &'static BarrierRegistry,
    definition_id: &str,
    barrier: impl FnOnce() + Send + 'static,
) {
    registry
        .lock()
        .expect("barrier registry")
        .insert(definition_id.to_string(), Box::new(barrier));
}

fn run_barrier(registry: &'static BarrierRegistry, definition_id: &str) {
    let barrier = registry
        .lock()
        .expect("barrier registry")
        .remove(definition_id);
    if let Some(barrier) = barrier {
        barrier();
    }
}

/// Called by the command itself, between its read and save passes. A no-op
/// unless a test armed a barrier for this definition.
pub(super) fn run_read_save_barrier(definition_id: &str) {
    run_barrier(read_save_barriers(), definition_id);
}

/// Called by the command itself, between its persona save and its sidecar
/// write. A no-op unless a test armed a barrier for this definition.
pub(super) fn run_save_mapping_barrier(definition_id: &str) {
    run_barrier(save_mapping_barriers(), definition_id);
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
    let binding = result.binding.clone().expect("a reload stores a binding");
    assert_eq!(Some(binding.path.as_str()), file.to_str());
    assert!(
        binding.in_sync,
        "the prompt that just landed came from this file"
    );
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
    assert_eq!(json["binding"]["inSync"], serde_json::json!(true));
    assert!(
        json["binding"]["path"].is_string(),
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
        result.binding, None,
        "no binding survived, because none was ever stored"
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
    assert_eq!(result.binding, None);
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

/// The ticket's acceptance check, end to end: edit `agent-prompts/pm.md`, click
/// Reload, restart the agent, and the prompt the adapter receives carries the
/// file's bytes unchanged.
///
/// Every hop is production code, and each hop's output is the next hop's input,
/// so the seam is bound rather than sampled at two ends. The spawn hop included:
/// the environment is not set by the test, it is written by the production
/// writer ([`apply_system_prompt_env`], the line `runtime.rs` calls on every
/// spawn) onto a real `Command` and read back off it.
///
/// **How the spawn hop is bound.** This test cannot call `spawn_agent_child`
/// — that needs a live app handle, a records store and an actual child
/// process — so it drives `apply_system_prompt_env` the way
/// `effort_cmd_tests` drives `apply_effort_to_spawn_command`, and the
/// production *call* is bound by the type system instead: the function returns
/// a `#[must_use]` [`SystemPromptApplied`](crate::managed_agents::SystemPromptApplied)
/// that `spawn_with_effort_proof` consumes by value, so deleting the call from
/// `spawn_agent_child` is a compile error. Removing the write itself from
/// `apply_system_prompt_env` fails this test. Neither half can be deleted
/// silently; that pairing is the repo's established answer for this seam.
///
///
/// 1. the file on disk and the real `set_prompt_source_and_reload` command;
/// 2. the definition read back off disk and
///    [`resolve_effective_config`] — the same resolve
///    `runtime.rs` performs on a restart, with the record's own stale prompt
///    bytes losing to the definition;
/// 3. that value exported by [`apply_system_prompt_env`] under
///    [`SYSTEM_PROMPT_ENV`], the write the spawn performs, and read back by the harness's own
///    [`CliArgs`](buzz_acp_pkg::delivery_seam::CliArgs) /
///    [`Config`](buzz_acp_pkg::delivery_seam::Config) env parse — a rename on
///    either side fails here;
/// 4. the harness's standing-prompt composition
///    ([`combined_system_prompt`](buzz_acp_pkg::delivery_seam::combined_system_prompt))
///    and per-adapter transport choice
///    ([`session_new_system_prompt`](buzz_acp_pkg::delivery_seam::session_new_system_prompt));
/// 5. a real `session/new` request over a real child process, echoed back by
///    the adapter script and asserted as the adapter received it.
///
/// The load-bearing assertion is that the composed prompt *contains the file's
/// bytes verbatim* — trimming, a line-ending rewrite or a re-encode anywhere on
/// the path fails it. The prompt the adapter receives is not the bare file (the
/// harness frames it in `<system>` alongside the base prompt), which is why the
/// framed value is also compared against the harness's own composition.
///
/// What this cannot do in-process: relaunch the desktop app or exec the
/// `buzz-acp` binary. It restarts the harness half for real — the adapter is a
/// spawned child — and reproduces the desktop half's restart by re-resolving
/// from disk, which is exactly what the spawn path reads.
#[tokio::test]
async fn a_reloaded_prompt_file_reaches_the_adapter_after_a_restart() {
    use crate::managed_agents::effective_config::resolve_effective_config;
    use crate::managed_agents::global_config::GlobalAgentConfig;
    use crate::managed_agents::SYSTEM_PROMPT_ENV;
    use buzz_acp_pkg::delivery_seam::{
        combined_system_prompt, session_new_system_prompt, AcpClient, CliArgs, Config, Parser,
        CLAUDE_AGENT_ACP_NAME,
    };

    let home = TempHome::new();
    let app = mock_app();
    save_personas(
        app.handle(),
        &[definition("pm", "Old instructions.", false)],
    )
    .expect("seed the definition");

    // 1. The operator edits the prompt file. Layout characters, non-ASCII and a
    //    trailing newline: everything a hand-edited prompt carries that a naive
    //    trim or line-ending rewrite would eat.
    let file_text = "You are the PM.\n\n\tKeep a decision log — ünïcode, emoji 🐝.\n";
    let file = home.prompt_file("pm.md", file_text);

    // ...and clicks Reload.
    let result = set_prompt_source_and_reload(
        "pm".to_string(),
        Some(file.to_string_lossy().into_owned()),
        app.handle().clone(),
    )
    .await
    .expect("the reload succeeds");
    assert!(result.local_updated, "the reload must save the prompt");

    // 2. The restart re-resolves the effective config from what is on disk now.
    //    The instance carries its own legacy prompt bytes; the definition is
    //    authoritative for a linked agent, so the reload must still win.
    let personas = load_personas(app.handle()).expect("definitions read back from disk");
    let mut record = linked_record("pm");
    record.system_prompt = Some("Stale instance instructions.".to_string());
    let effective = resolve_effective_config(&record, &personas, &GlobalAgentConfig::default())
        .require_resolved()
        .expect("a linked record with a live definition resolves");
    let spawned = effective
        .system_prompt
        .value
        .expect("a restart writes a system prompt");

    // 3. The spawn exports it. The value under test is taken from the
    //    production writer, not from this test: `apply_system_prompt_env` is
    //    the line the spawn path runs, and what it puts on the command is what
    //    the harness process below is given. Delete that write and `exported`
    //    is `None` and this test fails; delete its *call* from
    //    `spawn_agent_child` and the crate stops compiling, because
    //    `spawn_with_effort_proof` consumes the token bound here.
    let mut spawn_command = std::process::Command::new("true");
    let prompt_applied =
        crate::managed_agents::apply_system_prompt_env(&mut spawn_command, Some(spawned.as_str()));
    // Consumed the same way the spawn site consumes it, so this test also
    // fails to compile if the proof token is dropped from the seam.
    let _: crate::managed_agents::SystemPromptApplied = prompt_applied;
    let exported = spawn_command
        .get_envs()
        .find(|(key, _)| *key == std::ffi::OsStr::new(SYSTEM_PROMPT_ENV))
        .and_then(|(_, value)| value)
        .map(|value| value.to_string_lossy().into_owned())
        .expect("the spawn path must export the resolved prompt");
    let _prompt_env = EnvVarGuard::set(SYSTEM_PROMPT_ENV, &exported);

    // The same writer removes a stale inherited value when nothing resolves, so
    // an agent configured back to "no prompt" cannot keep the previous one.
    let mut cleared = std::process::Command::new("true");
    let _ = crate::managed_agents::apply_system_prompt_env(&mut cleared, Some("stale"));
    let _ = crate::managed_agents::apply_system_prompt_env(&mut cleared, None);
    assert!(
        cleared
            .get_envs()
            .any(|(key, value)| key == std::ffi::OsStr::new(SYSTEM_PROMPT_ENV) && value.is_none()),
        "an unset prompt must remove the inherited variable, not leave it standing"
    );
    let private_key = nostr::Keys::generate().secret_key().to_secret_hex();
    let cli = CliArgs::try_parse_from(["buzz-acp", "--private-key", &private_key])
        .expect("the harness parses its own environment");
    let config = Config::from_args(cli).expect("the harness config resolves");
    assert_eq!(
        config.system_prompt.as_deref(),
        Some(file_text),
        "the harness must read the file's bytes out of {SYSTEM_PROMPT_ENV}"
    );

    // 4. The harness composes the standing prompt it sends on `session/new`.
    //    A base prompt is present, as it is on every spawn that does not pass
    //    `--no-base-prompt`, so the assertion below is about the file's bytes
    //    surviving the framing, not about the framing being absent.
    let composed = combined_system_prompt(
        "/tmp",
        Some("Base harness instructions."),
        config.system_prompt.as_deref(),
        None,
        None,
        None,
        None,
    )
    .expect("a system prompt composes");
    assert!(
        composed.contains(file_text),
        "the composed prompt must carry the file's bytes verbatim, got {composed:?}"
    );

    // 5. `session/new` to a real adapter process, which echoes the request it
    //    received. `read -r`, or bash eats the JSON string escapes and corrupts
    //    exactly the bytes under test.
    let script = r#"
        read -r -t 5 _init
        echo '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":2,"agentCapabilities":{}}}'
        read -r -t 5 REQ
        printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"sessionId":"ses_reload","_receivedRequest":'"$REQ"'}}'
        sleep 1
    "#;

    for (agent_name, pointer) in [
        ("buzz-agent", vec!["systemPrompt"]),
        (
            CLAUDE_AGENT_ACP_NAME,
            vec!["_meta", "systemPrompt", "append"],
        ),
    ] {
        let mut client = AcpClient::spawn("bash", &["-c".into(), script.into()], &[], false)
            .await
            .expect("the adapter process starts");
        let handshake = client.initialize().await.expect("initialize succeeds");
        let protocol_version = handshake["protocolVersion"]
            .as_u64()
            .expect("the adapter advertises a protocol version")
            as u32;

        let transport = session_new_system_prompt(
            agent_name == "goose",
            protocol_version,
            agent_name,
            Some(composed.as_str()),
        );
        let response = client
            .session_new_full("/tmp", vec![], transport, None)
            .await
            .expect("session/new succeeds");
        client.shutdown().await;

        let mut received = &response.raw["_receivedRequest"]["params"];
        for key in &pointer {
            received = &received[key];
        }
        let received = received
            .as_str()
            .unwrap_or_else(|| panic!("{agent_name} must receive a system prompt on session/new"));
        assert!(
            received.contains(file_text),
            "{agent_name} must receive the prompt file's bytes unchanged, got {received:?}"
        );
        assert_eq!(
            received, composed,
            "{agent_name} must receive exactly what the harness composed"
        );
    }
}

/// Set a process environment variable for the life of the guard, restoring the
/// previous value on drop — including when the test panics.
///
/// Safe here only because every test in this module holds the process-env lock
/// through [`TempHome`].
struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

/// A managed-agent record linked to `persona_id`, with only the fields this
/// module's assertions read set to anything meaningful.
fn linked_record(persona_id: &str) -> crate::managed_agents::ManagedAgentRecord {
    use crate::managed_agents::{BackendKind, ManagedAgentRecord, RespondTo};
    ManagedAgentRecord {
        mcp_servers: None,
        description: None,
        pubkey: "agent-pk".to_string(),
        name: "PM".to_string(),
        persona_id: Some(persona_id.to_string()),
        private_key_nsec: String::new(),
        auth_tag: None,
        relay_url: "ws://localhost:3000".to_string(),
        avatar_url: None,
        acp_command: "buzz-acp".to_string(),
        // Not "goose": goose takes its system prompt through its own extension
        // request, so `session_new_system_prompt` deliberately returns no
        // `session/new` transport for it.
        agent_command: "buzz-agent".to_string(),
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

#[tokio::test]
async fn a_reloaded_binding_reads_back_through_the_command_until_it_is_cleared() {
    let home = TempHome::new();
    let app = mock_app();
    save_personas(
        app.handle(),
        &[definition("pm", "Old instructions.", false)],
    )
    .expect("seed the definition");
    let file = home.prompt_file("pm.md", "Ship the roadmap.\n");

    assert_eq!(
        get_prompt_source("pm".to_string(), app.handle().clone())
            .await
            .expect("an empty sidecar reads"),
        None,
        "nothing is bound before the first reload"
    );

    set_prompt_source_and_reload(
        "pm".to_string(),
        Some(file.to_string_lossy().into_owned()),
        app.handle().clone(),
    )
    .await
    .expect("the reload lands");

    assert_eq!(
        get_prompt_source("pm".to_string(), app.handle().clone())
            .await
            .expect("sidecar reads"),
        Some(PromptSourceBinding {
            path: file.to_string_lossy().into_owned(),
            in_sync: true,
        }),
        "the binding a reload stored must be readable, or the sidecar is write-only \
         and the dialog opens knowing nothing about it"
    );
    assert_eq!(
        get_prompt_source("designer".to_string(), app.handle().clone())
            .await
            .expect("sidecar reads"),
        None,
        "another definition is unbound"
    );

    set_prompt_source_and_reload("pm".to_string(), None, app.handle().clone())
        .await
        .expect("the clear lands");

    assert_eq!(
        get_prompt_source("pm".to_string(), app.handle().clone())
            .await
            .expect("sidecar reads"),
        None,
        "a cleared binding reads back as unbound"
    );
}

#[tokio::test]
async fn a_binding_whose_definition_is_gone_still_reads_back_so_it_can_be_cleared() {
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
    .expect("the reload lands");

    // The agent is deleted; its mapping outlives it.
    save_personas(app.handle(), &[]).expect("drop the definition");

    assert_eq!(
        get_prompt_source("pm".to_string(), app.handle().clone())
            .await
            .expect("sidecar reads"),
        Some(PromptSourceBinding {
            path: file.to_string_lossy().into_owned(),
            in_sync: false,
        }),
        "the read half must not depend on the definition, or an orphan mapping \
         becomes invisible and unclearable — and with no definition there is no \
         prompt for it to be in sync with"
    );
}

/// A failed sidecar write must report the binding that is still on disk.
///
/// `commit_prompt_source_at` writes atomically, so a failure leaves the
/// *previous* entry — file A — in place. Reporting "no binding" there tells the
/// dialog one thing while the sidecar says another: the next open seeds A back,
/// the hint claims the instructions come from A, and the next Reload restores
/// A over the prompt the user just loaded from B. The result carries the
/// surviving entry instead, marked out of sync because the prompt that landed
/// did not come from it.
#[tokio::test]
async fn a_failed_mapping_write_reports_the_binding_that_survived() {
    let home = TempHome::new();
    let app = mock_app();
    save_personas(
        app.handle(),
        &[definition("pm", "Old instructions.", false)],
    )
    .expect("seed the definition");
    let file_a = home.prompt_file("a.md", "Instructions from A.\n");
    let file_b = home.prompt_file("b.md", "Instructions from B.\n");

    set_prompt_source_and_reload(
        "pm".to_string(),
        Some(file_a.to_string_lossy().into_owned()),
        app.handle().clone(),
    )
    .await
    .expect("the first reload binds A");
    assert_eq!(
        stored_mapping(app.handle(), "pm").as_deref(),
        file_a.to_str()
    );

    // Injected failure at the mapping boundary only, leaving what the sidecar
    // already holds intact: the atomic write stages through
    // `prompt-sources.json.tmp`, so a directory in that slot fails the staging
    // write while the sidecar itself stays exactly as it is — readable, and
    // still mapping A.
    let staged = managed_agents_base_dir(app.handle())
        .expect("base dir")
        .join("prompt-sources.json.tmp");
    std::fs::create_dir_all(&staged).expect("occupy the atomic write's staging path");

    let result = set_prompt_source_and_reload(
        "pm".to_string(),
        Some(file_b.to_string_lossy().into_owned()),
        app.handle().clone(),
    )
    .await
    .expect("the prompt save landed, so the command reports rather than fails");

    assert!(
        result.mapping_error.is_some(),
        "the failed sidecar write must be reported"
    );
    assert_eq!(
        stored_prompt(app.handle(), "pm"),
        "Instructions from B.\n",
        "the prompt itself landed before the mapping was attempted"
    );
    assert_eq!(
        stored_mapping(app.handle(), "pm").as_deref(),
        file_a.to_str(),
        "the atomic write left A on disk"
    );
    assert_eq!(
        result.binding,
        Some(PromptSourceBinding {
            path: file_a.to_string_lossy().into_owned(),
            in_sync: false,
        }),
        "the surviving binding must be reported, and reported as no longer matching \
         the prompt, or the dialog claims B's text is loaded from A"
    );
    assert_eq!(
        get_prompt_source("pm".to_string(), app.handle().clone())
            .await
            .expect("sidecar reads"),
        result.binding,
        "the next open must see exactly what the failed reload reported"
    );
}

/// The binding is a claim about the definition's current text, and three other
/// paths can make it false. None of them knows the sidecar exists, so the claim
/// is checked on read rather than maintained on write — and each path is
/// covered here, because "the read checks it" is only true if it checks the
/// thing that actually changed.
#[tokio::test]
async fn a_prompt_written_by_another_path_reads_back_out_of_sync() {
    let home = TempHome::new();
    let app = mock_app();
    save_personas(
        app.handle(),
        &[definition("pm", "Old instructions.", false)],
    )
    .expect("seed the definition");
    let file = home.prompt_file("pm.md", "Instructions from the file.\n");
    set_prompt_source_and_reload(
        "pm".to_string(),
        Some(file.to_string_lossy().into_owned()),
        app.handle().clone(),
    )
    .await
    .expect("the reload binds the file");

    let bound = |app: tauri::AppHandle<tauri::test::MockRuntime>| async move {
        get_prompt_source("pm".to_string(), app)
            .await
            .expect("sidecar reads")
    };
    assert!(
        bound(app.handle().clone())
            .await
            .expect("the binding is stored")
            .in_sync,
        "the reload's own prompt is in sync with the file it came from"
    );

    // 1. A hand-typed edit through the ordinary update command.
    let typed = crate::managed_agents::prompt_source::update_request_from_definition(
        &crate::managed_agents::prompt_source::definition_with_prompt(
            &load_personas(app.handle())
                .expect("load")
                .into_iter()
                .find(|record| record.id == "pm")
                .expect("definition exists"),
            "Typed straight into the dialog.",
        ),
    );
    crate::commands::personas::update_persona(typed, app.handle().clone())
        .await
        .expect("the typed edit saves");
    assert_eq!(
        bound(app.handle().clone())
            .await
            .expect("the entry is still stored"),
        PromptSourceBinding {
            path: file.to_string_lossy().into_owned(),
            in_sync: false,
        },
        "after a typed edit the file no longer feeds the agent, and the dialog must say so"
    );

    // 2. An inbound replacement: another device's copy of the definition
    //    arriving with different instructions.
    let mut replaced = load_personas(app.handle())
        .expect("load")
        .into_iter()
        .find(|record| record.id == "pm")
        .expect("definition exists");
    replaced.system_prompt = "Replaced from another device.".to_string();
    save_personas(app.handle(), &[replaced]).expect("apply the inbound definition");
    assert!(
        !bound(app.handle().clone())
            .await
            .expect("the entry is still stored")
            .in_sync,
        "an inbound replacement invalidates the binding as surely as a typed edit"
    );

    // 3. Deletion. Nothing in the UI can reach an entry whose definition is
    //    gone, so the delete drops it rather than orphaning it.
    crate::commands::personas::delete_persona("pm".to_string(), app.handle().clone())
        .await
        .expect("the definition deletes");
    assert_eq!(
        bound(app.handle().clone()).await,
        None,
        "deleting the definition must retract its binding, not orphan it"
    );
}

/// The precondition the command passes to the save is what stops a concurrent
/// edit being clobbered, so the test has to run the command with a writer
/// actually inside its read→save window. Driving the helper directly (with the
/// expected timestamp handed to it) proves the helper, not the command: change
/// the command's argument to `None` and that test stays green while this one
/// fails.
#[tokio::test]
async fn a_concurrent_save_inside_the_command_window_is_refused() {
    let home = TempHome::new();
    let app = mock_app();
    save_personas(
        app.handle(),
        &[definition("barrier-pm", "Old instructions.", false)],
    )
    .expect("seed the definition");
    let file = home.prompt_file("pm.md", "Instructions from the file.\n");

    // The concurrent writer: a save that lands after the command has read the
    // definition and before it writes it back.
    let writer_app = app.handle().clone();
    arm_read_save_barrier("barrier-pm", move || {
        let mut personas = load_personas(&writer_app).expect("the writer loads");
        let record = personas
            .iter_mut()
            .find(|record| record.id == "barrier-pm")
            .expect("definition exists");
        record.display_name = "Renamed by someone else".to_string();
        record.updated_at = crate::util::now_iso();
        save_personas(&writer_app, &personas).expect("the concurrent edit lands");
    });

    let error = set_prompt_source_and_reload(
        "barrier-pm".to_string(),
        Some(file.to_string_lossy().into_owned()),
        app.handle().clone(),
    )
    .await
    .expect_err("a definition that moved under the command must not be overwritten");
    assert!(
        error.contains("edited while its prompt file was being read"),
        "the refusal must say what happened, got {error:?}"
    );

    let after = load_personas(app.handle())
        .expect("load")
        .into_iter()
        .find(|record| record.id == "barrier-pm")
        .expect("definition exists");
    assert_eq!(
        after.display_name, "Renamed by someone else",
        "the concurrent edit must survive the refused reload"
    );
    assert_eq!(
        after.system_prompt, "Old instructions.",
        "a refused reload writes nothing at all"
    );
    assert_eq!(
        stored_mapping(app.handle(), "barrier-pm"),
        None,
        "and stores no mapping, so the sidecar cannot claim a prompt that was never applied"
    );
}

/// The second unlockable window: a writer that lands **after** the persona save
/// and before the sidecar write.
///
/// The precondition cannot help here — the reload's own save has already
/// committed — so the only honest answer is the one the sidecar's digest gives:
/// the binding this call reports must say the file no longer matches the
/// agent's instructions, exactly as the next `get_prompt_source` will. Reporting
/// `in_sync: true` here would render "These instructions are loaded from
/// pm.md" over a prompt that came from somewhere else, and the claim would
/// silently flip on the next open with nothing to explain it.
///
/// Mutation proof: restoring the hard-coded `PromptSourceBinding { in_sync:
/// true }` in `submit_reloaded_prompt` fails this test while every other
/// prompt-source test stays green.
#[tokio::test]
async fn a_writer_between_the_save_and_the_mapping_reports_the_binding_out_of_sync() {
    let home = TempHome::new();
    let app = mock_app();
    save_personas(
        app.handle(),
        &[definition("late-writer-pm", "Old instructions.", false)],
    )
    .expect("seed the definition");
    let file = home.prompt_file("pm.md", "Instructions from the file.\n");

    // The concurrent writer: a save that lands after the reload's own persona
    // save and before the sidecar write, so the effective prompt is neither the
    // pre-reload text nor the file's.
    let writer_app = app.handle().clone();
    arm_save_mapping_barrier("late-writer-pm", move || {
        let mut personas = load_personas(&writer_app).expect("the writer loads");
        let record = personas
            .iter_mut()
            .find(|record| record.id == "late-writer-pm")
            .expect("definition exists");
        record.system_prompt = "Typed in another window.".to_string();
        record.updated_at = crate::util::now_iso();
        save_personas(&writer_app, &personas).expect("the concurrent edit lands");
    });

    let result = set_prompt_source_and_reload(
        "late-writer-pm".to_string(),
        Some(file.to_string_lossy().into_owned()),
        app.handle().clone(),
    )
    .await
    .expect("the reload itself succeeded; only the claim it can make is narrower");

    let binding = result
        .binding
        .clone()
        .expect("the mapping was stored, so a binding is reported");
    assert_eq!(Some(binding.path.as_str()), file.to_str());
    assert!(
        !binding.in_sync,
        "the agent's instructions came from the other writer, not from this file"
    );
    assert_eq!(
        result.mapping_error, None,
        "the sidecar write itself worked"
    );

    assert_eq!(
        stored_prompt(app.handle(), "late-writer-pm"),
        "Typed in another window.",
        "the last writer's prompt is what the agent will actually use"
    );

    // The same question asked again on the next open must give the same answer:
    // one response and the durable state cannot disagree.
    let reopened = get_prompt_source("late-writer-pm".to_string(), app.handle().clone())
        .await
        .expect("the sidecar reads")
        .expect("the binding is still stored");
    assert_eq!(
        reopened, binding,
        "the reload's answer must match what the next open reports"
    );
}

/// Deleting a directory-backed team cascades its member definitions away, and
/// each one's prompt-file binding must go with it.
///
/// The same reusable-id hazard as the inbound tombstone: a team re-adopted from
/// the same directory recreates its members under the same ids, and an orphaned
/// entry would rebind one to a file its owner never chose for it. This drives
/// the production `delete_team_with_cascade`, not a seam — removing the
/// `forget_prompt_source` loop from its cascade turns this test RED.
#[test]
fn deleting_a_team_retracts_its_cascaded_members_prompt_bindings() {
    let _home = TempHome::new();
    let app = mock_app();
    let handle = app.handle().clone();

    let mut member = definition("pack-pm", "Instructions from the file.\n", false);
    member.source_team = Some("pack".to_string());
    save_personas(&handle, &[member]).expect("seed the member definition");
    crate::managed_agents::save_teams(
        &handle,
        &[crate::managed_agents::TeamRecord {
            id: "pack-team".to_string(),
            name: "Pack".to_string(),
            description: None,
            instructions: None,
            persona_ids: vec!["pack-pm".to_string()],
            is_builtin: false,
            shared: false,
            catalog_source: None,
            // Never created on disk: the cascade removes the directory only
            // when it exists, and this test is about the sidecar.
            source_dir: Some(
                managed_agents_base_dir(&handle)
                    .expect("base dir")
                    .join("pack"),
            ),
            is_symlink: false,
            symlink_target: None,
            version: None,
            created_at: "2026-09-04T00:00:00Z".to_string(),
            updated_at: "2026-09-04T00:00:00Z".to_string(),
        }],
    )
    .expect("seed the team");

    let sidecar = managed_agents_base_dir(&handle)
        .expect("base dir")
        .join("prompt-sources.json");
    crate::managed_agents::prompt_source::commit_prompt_source_at(
        &sidecar,
        "pack-pm",
        Some((
            std::path::Path::new("/home/me/agent-prompts/pack-pm.md"),
            "Instructions from the file.\n",
        )),
    )
    .expect("seed the binding this device made");

    crate::managed_agents::delete_team_with_cascade(&handle, "pack-team")
        .expect("a non-built-in team with no referencing agents deletes");

    assert!(
        load_personas(&handle)
            .expect("load personas")
            .iter()
            .all(|record| record.id != "pack-pm"),
        "the cascade must remove the member definition"
    );
    assert_eq!(
        stored_mapping(&handle, "pack-pm"),
        None,
        "the binding must not outlive the definition the cascade destroyed"
    );
}

/// The recovery command, driven end to end: a corrupt sidecar refuses every
/// ordinary control, and the reset is what makes the field usable again.
#[tokio::test]
async fn resetting_recovers_from_a_sidecar_no_other_command_can_read() {
    let home = TempHome::new();
    let app = mock_app();
    save_personas(
        app.handle(),
        &[definition("pm", "Old instructions.", false)],
    )
    .expect("seed the definition");
    let file = home.prompt_file("pm.md", "Instructions from the file.\n");
    let sidecar = managed_agents_base_dir(app.handle())
        .expect("base dir")
        .join("prompt-sources.json");
    std::fs::create_dir_all(sidecar.parent().expect("parent")).expect("create the store dir");
    std::fs::write(&sidecar, "{ not json").expect("corrupt the sidecar");

    // Every ordinary way out is closed: reading the binding fails, and so does
    // Clear, which must read the file before it can remove one entry.
    assert!(
        get_prompt_source("pm".to_string(), app.handle().clone())
            .await
            .is_err(),
        "a corrupt sidecar must not read as unbound"
    );
    assert!(
        set_prompt_source_and_reload("pm".to_string(), None, app.handle().clone())
            .await
            .is_err(),
        "Clear cannot recover a corrupt sidecar — this is the state the reset exists for"
    );

    let quarantined = reset_prompt_sources(app.handle().clone())
        .await
        .expect("the reset succeeds");
    assert!(
        std::path::Path::new(&quarantined).exists(),
        "the unreadable file is moved aside, not deleted"
    );

    assert_eq!(
        get_prompt_source("pm".to_string(), app.handle().clone())
            .await
            .expect("the store reads again"),
        None,
        "after the reset every binding is gone, which is what the warning says"
    );
    set_prompt_source_and_reload(
        "pm".to_string(),
        Some(file.to_string_lossy().into_owned()),
        app.handle().clone(),
    )
    .await
    .expect("and the field works again");
    assert_eq!(stored_mapping(app.handle(), "pm").as_deref(), file.to_str());

    assert!(
        reset_prompt_sources(app.handle().clone()).await.is_err(),
        "a readable sidecar must not be resettable, or the recovery becomes a way \
         to drop every working binding at once"
    );
}
