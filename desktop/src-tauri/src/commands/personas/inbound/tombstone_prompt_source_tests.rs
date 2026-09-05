//! An inbound kind:5 tombstone must retract the tombstoned definition's
//! machine-local prompt-file binding.
//!
//! Persona ids are reusable: `persona_from_event` sets `id = d_tag`, so a
//! tombstone followed by a re-received kind:30175 head for the same coordinate
//! recreates the definition under the same id. An entry left behind by the
//! tombstone then rebinds that definition to a file its owner never chose for
//! it, and `Reload` reads that file into the agent.
//!
//! This drives the REAL inbound entrypoint
//! `reconcile_inbound_persona_event_blocking` over a `MockRuntime` handle — the
//! same fn the live subscription calls — because the retraction lives inside
//! the store-mutation closure that `commit_inbound_tombstone_with_store` runs,
//! and no helper-level test can reach it. Removing the
//! `forget_prompt_source` call from that closure turns this test RED.

use super::reconcile_inbound_persona_event_blocking;
use crate::app_state::build_app_state;
use crate::managed_agents::persona_events::persona_d_tag;
use crate::managed_agents::prompt_source::{commit_prompt_source_at, load_prompt_sources_at};
use crate::managed_agents::{
    load_personas, managed_agents_base_dir, save_personas, AgentDefinition,
};
use buzz_core_pkg::kind::{KIND_DELETION, KIND_PERSONA};
use nostr::JsonUtil;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

const RELAY: &str = "wss://tombstone-prompt-source.example";

/// A temporary `$HOME` plus the process-env lock that makes it safe, so both
/// the app data dir and the sidecar land inside the tempdir.
struct TempHome {
    _guard: std::sync::MutexGuard<'static, ()>,
    _temp: tempfile::TempDir,
    old_home: Option<OsString>,
    old_xdg: Option<OsString>,
}

impl TempHome {
    fn new() -> Self {
        let guard = crate::managed_agents::lock_path_mutex();
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).expect("create home");
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_DATA_HOME", &home);
        Self {
            _guard: guard,
            _temp: temp,
            old_home,
            old_xdg,
        }
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

fn mock_app(keys: &nostr::Keys) -> tauri::App<tauri::test::MockRuntime> {
    let state = build_app_state();
    *state.keys.lock().unwrap() = keys.clone();
    *state.relay_url_override.lock().unwrap() = Some(RELAY.to_string());
    tauri::test::mock_builder()
        .manage(state)
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .expect("mock app builds headless")
}

fn definition(id: &str) -> AgentDefinition {
    AgentDefinition {
        id: id.to_string(),
        display_name: "PM".to_string(),
        description: None,
        avatar_url: None,
        system_prompt: "Instructions from the file.\n".to_string(),
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

/// A signed NIP-09 deletion naming the persona coordinate, exactly as another
/// device would publish it.
fn signed_persona_tombstone(keys: &nostr::Keys, d_tag: &str) -> nostr::Event {
    let coordinate = format!("{KIND_PERSONA}:{}:{d_tag}", keys.public_key().to_hex());
    nostr::EventBuilder::new(
        nostr::Kind::from(KIND_DELETION as u16),
        "retired from another device",
    )
    .tags([nostr::Tag::parse(["a", coordinate.as_str()]).expect("a-tag parses")])
    .sign_with_keys(keys)
    .expect("tombstone signs")
}

fn sidecar_path(app: &tauri::AppHandle<tauri::test::MockRuntime>) -> PathBuf {
    managed_agents_base_dir(app)
        .expect("base dir")
        .join("prompt-sources.json")
}

#[test]
fn an_inbound_tombstone_retracts_the_deleted_definition_s_prompt_binding() {
    let _home = TempHome::new();
    let keys = nostr::Keys::generate();
    let app = mock_app(&keys);
    let handle = app.handle().clone();

    let record = definition("pm");
    let d_tag = persona_d_tag(&record);
    save_personas(&handle, &[record]).expect("seed the definition");

    let sidecar = sidecar_path(&handle);
    std::fs::create_dir_all(sidecar.parent().expect("parent")).expect("create the store dir");
    commit_prompt_source_at(
        &sidecar,
        "pm",
        Some((
            std::path::Path::new("/home/me/agent-prompts/pm.md"),
            "Instructions from the file.\n",
        )),
    )
    .expect("seed the binding this device made");

    reconcile_inbound_persona_event_blocking(
        signed_persona_tombstone(&keys, &d_tag).as_json(),
        RELAY.to_string(),
        handle.clone(),
    )
    .expect("a signed tombstone for a tracked coordinate reconciles");

    assert!(
        load_personas(&handle)
            .expect("load personas")
            .iter()
            .all(|record| record.id != "pm"),
        "the tombstone must remove the definition"
    );
    assert_eq!(
        load_prompt_sources_at(&sidecar)
            .expect("the sidecar reads")
            .get("pm"),
        None,
        "the binding must not outlive the definition — the id is reusable, and a re-received head would rebind the new definition to a file nobody chose for it"
    );
}

/// The same tombstone against a sidecar nothing can parse.
///
/// A signed deletion from another device is authoritative; optional
/// machine-local convenience metadata does not get to refuse it. Before
/// `retract_prompt_source_at`, the retraction inside the store-mutation closure
/// read the whole map before it could drop one key, so an unparseable file
/// returned `Err` from this whole reconcile: the local definition survived, the
/// corrupt file survived the restart, and the next boot's reconcile failed the
/// same way — an agent the user deleted elsewhere kept coming back on this
/// machine, with nothing in the error naming the reset that would clear it.
///
/// Restoring `commit_prompt_source_at` in `forget_prompt_source` turns this RED.
#[test]
fn an_inbound_tombstone_applies_even_when_the_sidecar_cannot_be_parsed() {
    let _home = TempHome::new();
    let keys = nostr::Keys::generate();
    let app = mock_app(&keys);
    let handle = app.handle().clone();

    let record = definition("pm");
    let d_tag = persona_d_tag(&record);
    save_personas(&handle, &[record]).expect("seed the definition");

    let sidecar = sidecar_path(&handle);
    std::fs::create_dir_all(sidecar.parent().expect("parent")).expect("create the store dir");
    std::fs::write(&sidecar, "{ not json").expect("corrupt the sidecar");

    reconcile_inbound_persona_event_blocking(
        signed_persona_tombstone(&keys, &d_tag).as_json(),
        RELAY.to_string(),
        handle.clone(),
    )
    .expect("an unreadable convenience sidecar must not veto a signed remote deletion");

    assert!(
        load_personas(&handle)
            .expect("load personas")
            .iter()
            .all(|record| record.id != "pm"),
        "the tombstone must still remove the definition"
    );
    assert_eq!(
        std::fs::read_to_string(&sidecar).expect("the sidecar is still there"),
        "{ not json",
        "and must leave the unreadable file for the reset to move aside, never \
         rewrite it from an assumed-empty map"
    );
}
