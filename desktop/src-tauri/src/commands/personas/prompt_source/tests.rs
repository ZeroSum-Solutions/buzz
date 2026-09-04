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

/// The ticket's acceptance check, end to end: edit `agent-prompts/pm.md`, click
/// Reload, restart the agent, and the prompt the adapter receives carries the
/// file's bytes unchanged.
///
/// Every hop is production code, and each hop's output is the next hop's input,
/// so the seam is bound rather than sampled at two ends:
///
/// 1. the file on disk and the real `set_prompt_source_and_reload` command;
/// 2. the definition read back off disk and
///    [`resolve_effective_config`] — the same resolve
///    `runtime.rs` performs on a restart, with the record's own stale prompt
///    bytes losing to the definition;
/// 3. that value exported under [`SYSTEM_PROMPT_ENV`], the name the spawn
///    writes, and read back by the harness's own
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

    // 3. The spawn exports it; the harness process parses its own environment.
    let _prompt_env = EnvVarGuard::set(SYSTEM_PROMPT_ENV, &spawned);
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
