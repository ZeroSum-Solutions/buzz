//! Configuration read failures must preserve the adopted generation.
use super::*;
use crate::managed_agents::mcp_registry::apply;

/// [PRIOR F7] `converge_now_with_records` used to read personas and the
/// global agent config with `.unwrap_or_default()`, treating "corrupt" the
/// same as "absent" — silently computing every agent's effective runtime,
/// and therefore its convergence, from defaults instead of surfacing that the
/// store could not be read. A corrupt `managed-agents.json` must now be
/// propagated as an error, and the current generation must be left exactly as
/// it was: nothing is adopted from defaults.
#[test]
fn mcp_registry_a_corrupt_personas_store_is_propagated_and_leaves_the_generation_unchanged() {
    let sandbox = SandboxedHome::new();
    let app = mock_app();

    let base =
        crate::managed_agents::managed_agents_base_dir(app.handle()).expect("base dir resolves");
    std::fs::write(base.join("managed-agents.json"), b"{ not json")
        .expect("write a corrupt agent store");

    // The mock app owns an isolated data directory on every platform. A
    // refused read must leave its adopted generation exactly unchanged.
    let generations_root = RegistryPaths::new(base, sandbox.home()).generations_root();
    let before = GenerationStore::open(&generations_root)
        .expect("open")
        .current()
        .expect("readable");

    let error = apply::converge_now_with_records(app.handle(), &[], &BTreeMap::new())
        .expect_err("a corrupt personas store must be propagated, not defaulted away");
    assert!(
        error.contains("personas"),
        "the error must name what failed to read, got {error}"
    );

    let after = GenerationStore::open(&generations_root)
        .expect("open")
        .current()
        .expect("readable");
    assert_eq!(
        before, after,
        "a propagated read failure must leave the current generation unchanged"
    );
}

#[test]
fn mcp_registry_corrupt_global_config_preserves_generation() {
    let sandbox = SandboxedHome::new();
    let app = mock_app();
    let base = crate::managed_agents::managed_agents_base_dir(app.handle()).unwrap();
    std::fs::write(base.join("global-agent-config.json"), b"not json").unwrap();
    let root = RegistryPaths::new(base, sandbox.home()).generations_root();
    let store = GenerationStore::open(&root).unwrap();
    let before = store.current().unwrap();
    let error = apply::converge_now_with_records_and_secrets(
        app.handle(),
        &[],
        &BTreeMap::new(),
        &FakeStore::default(),
    )
    .unwrap_err();
    assert!(error.contains("global agent config"), "{error}");
    assert_eq!(store.current().unwrap(), before);
}
