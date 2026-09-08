use super::*;
use crate::managed_agents::mcp_registry::{
    generation::GenerationStore,
    paths::{RegistryPaths, BUZZ_ACP_REGISTRY_FILE},
};

#[test]
fn mcp_runtime_change_reprojects_stored_selection_in_both_directions() {
    let (_guard, _home) = EnvGuard::new();
    let app = mock_app();
    let path = document_path(app.handle()).unwrap();
    write_document(
        &path,
        &RegistryDocument {
            version: 1,
            servers: vec![RegistryEntry {
                id: "server".into(),
                name: "server".into(),
                transport: RegistryTransport::Stdio {
                    command: std::env::current_exe().unwrap().to_str().unwrap().into(),
                    args: vec![],
                },
                env: BTreeMap::new(),
            }],
        },
    )
    .unwrap();
    let mut record = bare_agent_record("runtime-change");
    record.mcp_servers = Some(AgentMcpServers {
        version: 1,
        enabled: vec!["server".into()],
    });
    let secrets = FakeStore::default();
    let paths = apply::registry_paths(app.handle()).unwrap().unwrap();
    let store = GenerationStore::open(&paths.generations_root()).unwrap();
    for command in ["buzz-agent", "claude", "buzz-agent"] {
        record.agent_command = command.into();
        record.agent_command_override = Some(command.into());
        let result = apply::reconverge_after_runtime_change_with_secrets(
            app.handle(),
            &[record.clone()],
            &secrets,
        )
        .unwrap()
        .unwrap();
        assert!(result.refused.is_empty(), "refused: {:?}", result.refused);
        let current = store.current_dir().unwrap().unwrap();
        let artifact = RegistryPaths::agent_dir(&current, &record.pubkey)
            .unwrap()
            .join(BUZZ_ACP_REGISTRY_FILE);
        assert_eq!(
            artifact.exists(),
            command == "buzz-agent",
            "projection must reflect the new runtime immediately"
        );
        assert_eq!(record.mcp_servers.as_ref().unwrap().enabled, vec!["server"]);
    }
}

#[test]
fn mcp_runtime_change_refuses_corrupt_registry_without_replacing_generation() {
    let (_guard, _home) = EnvGuard::new();
    let app = mock_app();
    let path = document_path(app.handle()).unwrap();
    write_document(
        &path,
        &RegistryDocument {
            version: 1,
            servers: vec![],
        },
    )
    .unwrap();
    let records = [bare_agent_record("runtime-change")];
    let secrets = FakeStore::default();
    let first =
        apply::reconverge_after_runtime_change_with_secrets(app.handle(), &records, &secrets)
            .unwrap()
            .unwrap();
    fs::write(&path, b"invalid registry").unwrap();
    let error =
        apply::reconverge_after_runtime_change_with_secrets(app.handle(), &records, &secrets)
            .unwrap_err();
    assert!(error.contains("were saved"));
    let paths = apply::registry_paths(app.handle()).unwrap().unwrap();
    assert_eq!(
        GenerationStore::open(&paths.generations_root())
            .unwrap()
            .current()
            .unwrap(),
        Some(first.generation)
    );
}

#[test]
fn mcp_runtime_change_without_registry_does_not_require_a_launcher() {
    let (_guard, _home) = EnvGuard::new();
    let app = mock_app();
    assert!(apply::reconverge_after_runtime_change_with_secrets(
        app.handle(),
        &[bare_agent_record("plain")],
        &FakeStore::default()
    )
    .unwrap()
    .is_none());
}
