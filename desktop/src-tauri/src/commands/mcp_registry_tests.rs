use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use tauri::Manager;

use super::*;
use crate::app_state::{build_app_state, AppState};
use crate::managed_agents::mcp_registry::load::LoadedEntry;
use crate::managed_agents::mcp_registry::schema::{
    RegistryDocument, RegistryEntry, RegistryTransport, MAX_DOCUMENT_BYTES,
};
use crate::managed_agents::{
    save_managed_agents, AgentMcpServers, BackendKind, ManagedAgentRecord, RespondTo,
    AGENT_MCP_SERVERS_VERSION,
};

struct EnvGuard {
    _path_guard: std::sync::MutexGuard<'static, ()>,
    _temp: tempfile::TempDir,
    old_home: Option<std::ffi::OsString>,
    old_xdg: Option<std::ffi::OsString>,
    old_path: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn new() -> (Self, PathBuf) {
        let path_guard = crate::managed_agents::lock_path_mutex();
        crate::managed_agents::clear_resolve_cache();
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let bin = temp.path().join("bin");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&bin).unwrap();

        let launcher = bin.join("buzz-mcp-launch");
        fs::write(&launcher, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&launcher, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        let old_path = std::env::var_os("PATH");

        #[cfg(target_os = "macos")]
        {
            if let Some(ref real_home) = old_home {
                let real_keychains = PathBuf::from(real_home).join("Library/Keychains");
                if real_keychains.exists() {
                    let temp_lib = home.join("Library");
                    let _ = fs::create_dir_all(&temp_lib);
                    let _ = std::os::unix::fs::symlink(&real_keychains, temp_lib.join("Keychains"));
                }
            }
        }

        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_DATA_HOME", &home);

        let new_path = if let Some(ref p) = old_path {
            format!("{}:{}", bin.display(), p.to_string_lossy())
        } else {
            bin.display().to_string()
        };
        std::env::set_var("PATH", &new_path);

        (
            Self {
                _path_guard: path_guard,
                _temp: temp,
                old_home,
                old_xdg,
                old_path,
            },
            home,
        )
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        crate::managed_agents::clear_resolve_cache();
        if let Some(ref old) = self.old_home {
            std::env::set_var("HOME", old);
        } else {
            std::env::remove_var("HOME");
        }
        if let Some(ref old) = self.old_xdg {
            std::env::set_var("XDG_DATA_HOME", old);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(ref old) = self.old_path {
            std::env::set_var("PATH", old);
        } else {
            std::env::remove_var("PATH");
        }
    }
}

fn mock_app() -> tauri::App<tauri::test::MockRuntime> {
    let state = build_app_state();
    *state.keys.lock().unwrap() = nostr::Keys::generate();
    *state.relay_url_override.lock().unwrap() = Some("ws://127.0.0.1:1".to_string());

    tauri::test::mock_builder()
        .manage(state)
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .expect("mock app builds headless")
}

fn bare_agent_record(pubkey: &str) -> ManagedAgentRecord {
    ManagedAgentRecord {
        mcp_servers: None,
        description: None,
        pubkey: pubkey.to_string(),
        name: "Agent".to_string(),
        persona_id: None,
        private_key_nsec: "nsec1vl029mgpspedva04g90vltkh6fvh240eqtv9xxl2xme3sqnvnabla0uvyu"
            .to_string(),
        auth_tag: None,
        relay_url: "ws://localhost:3000".to_string(),
        avatar_url: None,
        acp_command: "buzz-acp".to_string(),
        agent_command: "buzz-agent".to_string(),
        agent_command_override: None,
        agent_args: vec![],
        mcp_command: "".to_string(),
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
        created_at: "".to_string(),
        updated_at: "".to_string(),
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

#[test]
fn mcp_registry_a_rejected_entrys_credential_shaped_args_are_redacted() {
    let loaded = LoadedEntry {
        entry: RegistryEntry {
            id: "server1".to_string(),
            name: "Server One".to_string(),
            transport: RegistryTransport::Stdio {
                command: "/usr/local/bin/server".to_string(),
                args: vec![
                    "--api-key".to_string(),
                    "sk-live-secrettoken123".to_string(),
                    "--other".to_string(),
                    "plain-arg".to_string(),
                    "ghp_mygithubtoken".to_string(),
                ],
            },
            env: BTreeMap::from([
                ("PLAIN_VAR".to_string(), "normal_val".to_string()),
                ("API_SECRET".to_string(), "sk-live-envsecret".to_string()),
                ("MCP_REF".to_string(), "mcp:my-ref".to_string()),
            ]),
        },
        rejection: Some("argument 0 carries a credential".to_string()),
    };

    let view = view_of(&loaded);
    assert_eq!(
        view.args,
        vec![
            "--api-key".to_string(),
            "<redacted>".to_string(),
            "--other".to_string(),
            "plain-arg".to_string(),
            "<redacted>".to_string(),
        ]
    );

    let plain_env = view.env.iter().find(|e| e.name == "PLAIN_VAR").unwrap();
    assert_eq!(plain_env.literal.as_deref(), Some("<redacted>"));

    let secret_env = view.env.iter().find(|e| e.name == "API_SECRET").unwrap();
    assert_eq!(secret_env.literal.as_deref(), Some("<redacted>"));

    let ref_env = view.env.iter().find(|e| e.name == "MCP_REF").unwrap();
    assert_eq!(ref_env.reference.as_deref(), Some("mcp:my-ref"));
    assert_eq!(ref_env.literal, None);
}

#[test]
fn mcp_registry_save_refuses_a_document_over_the_byte_cap() {
    let (_guard, _home) = EnvGuard::new();
    let app = mock_app();
    let doc_path = document_path(app.handle()).expect("document path");
    fs::create_dir_all(doc_path.parent().unwrap()).unwrap();
    fs::write(&doc_path, vec![b'a'; MAX_DOCUMENT_BYTES + 10]).unwrap();

    let entry = RegistryEntry {
        id: "server1".to_string(),
        name: "Server One".to_string(),
        transport: RegistryTransport::Stdio {
            command: "/usr/local/bin/server".to_string(),
            args: vec![],
        },
        env: BTreeMap::new(),
    };
    let err = save_mcp_registry_server(app.handle().clone(), entry, BTreeMap::new()).unwrap_err();
    assert!(
        err.contains("cap") || err.contains("65536"),
        "expected error mentioning byte cap, got: {err}"
    );
}

#[test]
fn mcp_registry_save_does_not_follow_a_symlinked_document() {
    let (_guard, _home) = EnvGuard::new();
    let app = mock_app();
    let doc_path = document_path(app.handle()).expect("document path");
    fs::create_dir_all(doc_path.parent().unwrap()).unwrap();

    let target = doc_path.parent().unwrap().join("real_document.json");
    fs::write(&target, b"{\"version\":1,\"servers\":[]}").unwrap();

    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &doc_path).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&target, &doc_path).unwrap();

    let entry = RegistryEntry {
        id: "server1".to_string(),
        name: "Server One".to_string(),
        transport: RegistryTransport::Stdio {
            command: "/usr/local/bin/server".to_string(),
            args: vec![],
        },
        env: BTreeMap::new(),
    };
    let err = save_mcp_registry_server(app.handle().clone(), entry, BTreeMap::new()).unwrap_err();
    assert!(
        err.contains("symbolic link") || err.contains("symlink"),
        "expected error mentioning symlink, got: {err}"
    );
}

#[test]
fn mcp_registry_delete_server_reports_partial_state_on_agent_save_failure() {
    let (_guard, _home) = EnvGuard::new();
    let app = mock_app();
    let doc_path = document_path(app.handle()).expect("document path");
    fs::create_dir_all(doc_path.parent().unwrap()).unwrap();

    let doc = RegistryDocument {
        version: 1,
        servers: vec![
            RegistryEntry {
                id: "srv1".to_string(),
                name: "Server 1".to_string(),
                transport: RegistryTransport::Stdio {
                    command: "/usr/local/bin/server1".to_string(),
                    args: vec![],
                },
                env: BTreeMap::new(),
            },
            RegistryEntry {
                id: "srv2".to_string(),
                name: "Server 2".to_string(),
                transport: RegistryTransport::Stdio {
                    command: "/usr/local/bin/server2".to_string(),
                    args: vec![],
                },
                env: BTreeMap::new(),
            },
        ],
    };
    write_document(&doc_path, &doc).unwrap();

    let mut record = bare_agent_record("agent-save-fail");
    record.mcp_servers = Some(AgentMcpServers {
        version: AGENT_MCP_SERVERS_VERSION,
        enabled: vec!["srv1".to_string()],
    });
    save_managed_agents(app.handle(), &[record]).unwrap();

    let err = delete_mcp_registry_server_internal(app.handle(), "srv1", |_| {
        Err("injected agent write failure".to_string())
    })
    .unwrap_err();

    assert_eq!(
        err,
        "mcp registry document updated to remove srv1, but updating agent records failed: injected agent write failure; state is inconsistent"
    );

    // Verify document was written first and srv1 was removed
    let updated_doc = read_document(&doc_path).unwrap();
    assert_eq!(updated_doc.servers.len(), 1);
    assert_eq!(updated_doc.servers[0].id, "srv2");
}

#[test]
fn mcp_registry_concurrent_mutations_do_not_clobber_each_other() {
    let (_guard, _home) = EnvGuard::new();
    let app = mock_app();
    let state = app.state::<AppState>();

    // Hold lock on main thread
    let lock = state.mcp_registry_store_lock.lock().unwrap();

    // In a background thread, attempt to call a mutation (set_agent_mcp_servers)
    let app_handle = app.handle().clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        tx.send("started").unwrap();
        let res = set_agent_mcp_servers(app_handle, "nonexistent".to_string(), vec![]);
        assert!(res.is_err());
        tx.send("finished").unwrap();
    });

    assert_eq!(rx.recv().unwrap(), "started");
    // Background thread must be blocked on mcp_registry_store_lock
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(rx.try_recv().is_err());

    // Release lock
    drop(lock);

    // Now background thread acquires lock and finishes
    assert_eq!(rx.recv().unwrap(), "finished");
    handle.join().unwrap();
}

#[test]
fn mcp_registry_save_surfaces_a_refused_agent_not_just_an_error() {
    let (_guard, _home) = EnvGuard::new();
    let app = mock_app();

    // Agent with runtime buzz-agent which only supports stdio
    let mut record = bare_agent_record("agent-refused");
    record.mcp_servers = Some(AgentMcpServers {
        version: AGENT_MCP_SERVERS_VERSION,
        enabled: vec!["remote-http".to_string()],
    });
    save_managed_agents(app.handle(), &[record]).unwrap();

    let http_entry = RegistryEntry {
        id: "remote-http".to_string(),
        name: "remote-http".to_string(),
        transport: RegistryTransport::Http {
            url: "https://mcp.example.com".to_string(),
            auth: None,
        },
        env: BTreeMap::new(),
    };

    let view = save_mcp_registry_server(app.handle().clone(), http_entry, BTreeMap::new())
        .expect("save succeeds with refused agent in view");

    assert!(
        !view.refused.is_empty(),
        "view.refused must contain the refused agent"
    );
    let (agent_id, reason) = &view.refused[0];
    assert_eq!(agent_id, "agent-refused");
    assert!(
        reason.contains("http") || reason.contains("buzz-agent"),
        "reason should explain transport incompatibility, got: {reason}"
    );
}
