//! Tests for the spawn wiring: convergence, placement, the capability, and the
//! per-agent working directory.
//!
//! Every one drives the shipped `converge` and the shipped `plan_for_spawn`
//! against a real staging tree and the real `McpSecretLookup`, so deleting the
//! guard each names fails it.

use std::collections::BTreeMap;
use std::sync::Mutex;

use buzz_secret_store_pkg::capability::NONCE_LEN;
use buzz_secret_store_pkg::testing::MemoryBlobSource;
use buzz_secret_store_pkg::{
    binding_key_for, storage_key, AgentCapability, McpSecretLookup, McpSecretRef, SecretBlobSource,
    CAPABILITY_ENV_VAR,
};

use super::converge::{
    converge, AgentSelection, ConvergeError, GenerationInputs, NonceSource, SecretStoreIo,
};
use super::generate::BUZZ_ACP_REGISTRY_ENV_VAR;
use super::generation::SecretRemover;
use super::load::{parse_registry, LoadedRegistry};
use super::paths::{RegistryPaths, AGENTS_SUBDIR, BUZZ_ACP_REGISTRY_FILE, REFUSAL_FILE};
use super::spawn::{managed_env_vars, plan_for_spawn, McpSpawnPlan};
use crate::managed_agents::{McpConfigPlacement, McpTransport};

const LAUNCHER: &str = "/Applications/Buzz.app/Contents/MacOS/buzz-mcp-launch";

/// The keychain service a convergence writes into every generated argv. The
/// production caller passes `crate::app_state::keyring_service()`, which
/// `mcp_registry_generated_argv_names_the_desktops_keychain_service` binds.
const SERVICE: &str = "buzz-desktop-dev.wiring-tests";
const AGENT_A: &str = "a1b2c3";
const AGENT_B: &str = "d4e5f6";

#[cfg(unix)]
const SERVER_BIN: &str = "/usr/local/bin";
#[cfg(windows)]
const SERVER_BIN: &str = "C:/buzz/bin";

/// An in-memory store standing in for the keychain, with a switch that fails
/// one named deletion so the journalled retry can be driven.
#[derive(Default)]
struct FakeStore {
    source: MemoryBlobSource,
    refuse_delete: Mutex<Option<String>>,
    refuse_write: Mutex<bool>,
}

impl FakeStore {
    fn insert(&self, key: &str, value: &str) {
        self.source.insert(key, value);
    }

    fn map(&self) -> BTreeMap<String, String> {
        match self.source.read_blob().expect("readable") {
            None => BTreeMap::new(),
            Some(bytes) => buzz_secret_store_pkg::parse_blob(&bytes)
                .expect("parses")
                .into_iter()
                .collect(),
        }
    }
}

impl SecretRemover for FakeStore {
    fn remove(&self, key: &str) -> Result<(), String> {
        let refuse = self.refuse_delete.lock().expect("lock").clone();
        if refuse.as_deref() == Some(key) {
            return Err(format!("keychain refused to delete {key}"));
        }
        self.source.remove(key);
        Ok(())
    }

    fn write_all(&self, entries: &BTreeMap<String, String>) -> Result<(), String> {
        if *self.refuse_write.lock().expect("lock") {
            return Err("keychain refused the write".to_string());
        }
        for (key, value) in entries {
            self.source.insert(key, value);
        }
        Ok(())
    }
}

impl SecretStoreIo for FakeStore {
    fn read_all(&self) -> Result<BTreeMap<String, String>, String> {
        Ok(self.map())
    }
}

/// A nonce source whose output changes per call, so two generations get two
/// different capabilities without the test having to guess either.
struct CountingNonces(Mutex<u8>);

impl Default for CountingNonces {
    fn default() -> Self {
        Self(Mutex::new(1))
    }
}

impl NonceSource for CountingNonces {
    fn nonce(&self) -> [u8; NONCE_LEN] {
        let mut guard = self.0.lock().expect("lock");
        let value = *guard;
        *guard = guard.wrapping_add(1);
        [value; NONCE_LEN]
    }
}

fn document(servers: &str) -> String {
    format!(
        "{{\"version\":1,\"servers\":[{}]}}",
        servers.replace("/usr/local/bin", SERVER_BIN)
    )
}

/// A stdio entry whose `env` block names one `mcp:` reference.
fn stdio(id: &str, name: &str) -> String {
    format!(
        "{{\"id\":\"{id}\",\"name\":\"{name}\",\"transport\":\"stdio\",\
         \"command\":\"/usr/local/bin/{name}-mcp\",\"args\":[\"--stdio\"],\
         \"env\":{{\"TOKEN\":\"mcp:{id}-token\"}}}}"
    )
}

fn registry(servers: &str) -> LoadedRegistry {
    parse_registry(document(servers).as_bytes()).expect("the fixture document loads")
}

fn selection(agent_id: &str, placement: McpConfigPlacement, enabled: &[&str]) -> AgentSelection {
    AgentSelection {
        agent_id: agent_id.to_string(),
        runtime_id: "buzz-agent".to_string(),
        transports: vec![McpTransport::Stdio],
        placement,
        enabled: enabled.iter().map(|id| id.to_string()).collect(),
    }
}

fn paths(root: &std::path::Path) -> RegistryPaths {
    RegistryPaths::new(root.join("data"), root.join("nest"))
}

/// Read the binding record for a spawn, out of a store the test controls.
fn binding_reader(store: &FakeStore) -> impl Fn(&str) -> Result<Option<String>, String> + '_ {
    move |key: &str| Ok(store.map().get(key).cloned())
}

// ── Deleted-server convergence ────────────────────────────────────────────

/// Memo decision 5's named test. A server removed from an agent's selection is
/// not carried onto the next generation, its old key is deleted as a post-flip
/// journalled step, and the capability that authenticated it is revoked with
/// the generation it belonged to.
///
/// Every read goes through the shipped `McpSecretLookup`, so this is the
/// authorization the launcher and the proxy actually perform. Carrying every
/// key forward unconditionally, or keeping the old generation's binding record,
/// fails it.
#[test]
fn mcp_registry_deleted_server_stops_authenticating() {
    let root = tempfile::tempdir().expect("tempdir");
    let paths = paths(root.path());
    let store = FakeStore::default();
    let nonces = CountingNonces::default();

    // Generation 1: both servers enabled, both credentials in the store under
    // the generation the operator entered them in.
    let both = registry(&format!(
        "{},{}",
        stdio("gh", "github"),
        stdio("sn", "sentry")
    ));
    let first = converge(
        &paths,
        &both,
        &[selection(
            AGENT_A,
            McpConfigPlacement::Unsupported,
            &["gh", "sn"],
        )],
        &GenerationInputs {
            launcher: LAUNCHER,
            keychain_service: SERVICE,
            pending: &BTreeMap::new(),
        },
        &store,
        &nonces,
    )
    .expect("first convergence");
    assert_eq!(first.generation, 1);

    let generation_one = AgentCapability::bind(
        AGENT_A,
        1,
        store
            .map()
            .get(&binding_key_for(AGENT_A, 1))
            .expect("generation 1 minted a binding record"),
    )
    .expect("a stored binding rebuilds");
    store.insert(
        &storage_key(&generation_one, &reference("gh-token")),
        "GITHUB-SECRET",
    );
    store.insert(
        &storage_key(&generation_one, &reference("sn-token")),
        "SENTRY-SECRET",
    );

    // Both authenticate before the deletion — otherwise the assertion below
    // would pass against a store that never worked.
    let lookup = McpSecretLookup::new(&store.source);
    assert_eq!(
        lookup
            .resolve(&generation_one, &reference("gh-token"))
            .expect("github resolves at generation 1")
            .expose(),
        "GITHUB-SECRET"
    );

    // Generation 2: the operator deletes `github` from the registry and drops
    // it from the agent's selection.
    let one = registry(&stdio("sn", "sentry"));
    let second = converge(
        &paths,
        &one,
        &[selection(AGENT_A, McpConfigPlacement::Unsupported, &["sn"])],
        &GenerationInputs {
            launcher: LAUNCHER,
            keychain_service: SERVICE,
            pending: &BTreeMap::new(),
        },
        &store,
        &nonces,
    )
    .expect("second convergence");
    assert_eq!(second.generation, 2);

    let generation_two = AgentCapability::bind(
        AGENT_A,
        2,
        store
            .map()
            .get(&binding_key_for(AGENT_A, 2))
            .expect("generation 2 minted a binding record"),
    )
    .expect("a stored binding rebuilds");

    // The kept server still authenticates, under the new generation.
    assert_eq!(
        lookup
            .resolve(&generation_two, &reference("sn-token"))
            .expect("sentry resolves at generation 2")
            .expose(),
        "SENTRY-SECRET"
    );
    // The deleted one does not, under either capability: it was never carried
    // forward, and generation 1's binding record is gone, so the capability
    // that once authenticated it now resolves nothing at all.
    assert!(lookup
        .resolve(&generation_two, &reference("gh-token"))
        .is_err());
    assert!(lookup
        .resolve(&generation_one, &reference("gh-token"))
        .is_err());
    assert!(lookup
        .resolve(&generation_one, &reference("sn-token"))
        .is_err());

    let records = store.map();
    assert!(
        !records.values().any(|value| value == "GITHUB-SECRET"),
        "the deleted server's credential is still in the store: {records:?}"
    );
    assert!(!records.contains_key(&binding_key_for(AGENT_A, 1)));

    // And the adopted generation's handover file no longer names it.
    let handover = std::fs::read_to_string(
        paths
            .generations_root()
            .join("generations")
            .join("2")
            .join(AGENTS_SUBDIR)
            .join(AGENT_A)
            .join(BUZZ_ACP_REGISTRY_FILE),
    )
    .expect("the adopted generation staged a handover file");
    assert!(!handover.contains("github"), "{handover}");
    assert!(handover.contains("sentry"), "{handover}");
}

/// A keychain delete that fails after the flip keeps its journal entry, and the
/// deleted server's credential is gone once the retry succeeds — never silently
/// abandoned. Returning `Ok(())` from the failing remove fails this.
#[test]
fn mcp_registry_a_failed_revocation_is_owed_not_abandoned() {
    let root = tempfile::tempdir().expect("tempdir");
    let paths = paths(root.path());
    let store = FakeStore::default();
    let nonces = CountingNonces::default();
    let both = registry(&format!(
        "{},{}",
        stdio("gh", "github"),
        stdio("sn", "sentry")
    ));
    converge(
        &paths,
        &both,
        &[selection(
            AGENT_A,
            McpConfigPlacement::Unsupported,
            &["gh", "sn"],
        )],
        &GenerationInputs {
            launcher: LAUNCHER,
            keychain_service: SERVICE,
            pending: &BTreeMap::new(),
        },
        &store,
        &nonces,
    )
    .expect("first convergence");
    let generation_one = AgentCapability::bind(
        AGENT_A,
        1,
        store
            .map()
            .get(&binding_key_for(AGENT_A, 1))
            .expect("bound"),
    )
    .expect("rebuilds");
    let doomed = storage_key(&generation_one, &reference("gh-token"));
    store.insert(&doomed, "GITHUB-SECRET");
    *store.refuse_delete.lock().expect("lock") = Some(doomed.clone());

    let one = registry(&stdio("sn", "sentry"));
    let error = converge(
        &paths,
        &one,
        &[selection(AGENT_A, McpConfigPlacement::Unsupported, &["sn"])],
        &GenerationInputs {
            launcher: LAUNCHER,
            keychain_service: SERVICE,
            pending: &BTreeMap::new(),
        },
        &store,
        &nonces,
    )
    .expect_err("a failed post-flip deletion is surfaced, not swallowed");
    assert!(format!("{error}").contains("cleanup"), "{error}");
    assert!(store.map().contains_key(&doomed));

    // The debt is durable: the next convergence finishes it before it stages
    // anything of its own.
    *store.refuse_delete.lock().expect("lock") = None;
    converge(
        &paths,
        &one,
        &[selection(AGENT_A, McpConfigPlacement::Unsupported, &["sn"])],
        &GenerationInputs {
            launcher: LAUNCHER,
            keychain_service: SERVICE,
            pending: &BTreeMap::new(),
        },
        &store,
        &nonces,
    )
    .expect("the retry succeeds");
    assert!(!store.map().contains_key(&doomed));
}

// ── Spawn wiring ──────────────────────────────────────────────────────────

/// The buzz-agent path: the handover file is named by absolute path inside the
/// adopted generation, the capability rides in the spawn environment, and the
/// working directory does not move.
#[test]
fn mcp_registry_spawn_hands_over_the_generation_file_and_the_capability() {
    let root = tempfile::tempdir().expect("tempdir");
    let paths = paths(root.path());
    let store = FakeStore::default();
    converge(
        &paths,
        &registry(&stdio("gh", "github")),
        &[selection(AGENT_A, McpConfigPlacement::Unsupported, &["gh"])],
        &GenerationInputs {
            launcher: LAUNCHER,
            keychain_service: SERVICE,
            pending: &BTreeMap::new(),
        },
        &store,
        &CountingNonces::default(),
    )
    .expect("converges");

    let plan = plan_for_spawn(
        &paths,
        AGENT_A,
        McpConfigPlacement::Unsupported,
        binding_reader(&store),
    )
    .expect("the spawn plan resolves");

    let set: BTreeMap<&str, &str> = plan
        .set
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let handover = set
        .get(BUZZ_ACP_REGISTRY_ENV_VAR)
        .expect("the handover file is named");
    assert!(std::path::Path::new(handover).is_absolute(), "{handover}");
    let body = std::fs::read_to_string(handover).expect("readable");
    assert!(body.contains("github"), "{body}");
    assert!(body.contains(LAUNCHER), "{body}");
    // References, never values — this file is readable by the agent.
    assert!(body.contains("mcp:gh-token"), "{body}");

    let capability = set.get(CAPABILITY_ENV_VAR).expect("the capability is set");
    assert!(
        capability.starts_with(&format!("v1.{AGENT_A}.1.")),
        "{capability}"
    );
    assert!(
        AgentCapability::parse(capability).is_ok(),
        "the launcher must be able to parse it: {capability}"
    );
    // The capability is the one credential-shaped thing in the plan, and it is
    // in the environment alone: no generated file carries it.
    assert!(!body.contains(*capability), "{body}");

    // buzz-agent reads a handed-over file, so it keeps the shared nest.
    assert_eq!(plan.workdir, None);
}

/// Memo decision 9's named test. Two agents of one runtime with different
/// selections get their generated config at distinct paths, each holding
/// exactly its own server set, and changing one leaves the other byte-identical.
#[test]
fn mcp_registry_a_toggle_changes_only_the_named_agents_config() {
    let root = tempfile::tempdir().expect("tempdir");
    let paths = paths(root.path());
    let store = FakeStore::default();
    let nonces = CountingNonces::default();
    let placement = McpConfigPlacement::ProjectFileInWorkdir { file: ".mcp.json" };
    let both = registry(&format!(
        "{},{}",
        stdio("gh", "github"),
        stdio("sn", "sentry")
    ));

    converge(
        &paths,
        &both,
        &[
            selection(AGENT_A, placement, &["gh"]),
            selection(AGENT_B, placement, &["sn"]),
        ],
        &GenerationInputs {
            launcher: LAUNCHER,
            keychain_service: SERVICE,
            pending: &BTreeMap::new(),
        },
        &store,
        &nonces,
    )
    .expect("converges");

    let plan_a = plan_for_spawn(&paths, AGENT_A, placement, binding_reader(&store))
        .expect("agent A's plan resolves");
    let plan_b = plan_for_spawn(&paths, AGENT_B, placement, binding_reader(&store))
        .expect("agent B's plan resolves");

    let workdir_a = plan_a.workdir.clone().expect("a project file needs a cwd");
    let workdir_b = plan_b.workdir.clone().expect("a project file needs a cwd");
    assert_ne!(workdir_a, workdir_b);

    let file_a = workdir_a.join(".mcp.json");
    let file_b = workdir_b.join(".mcp.json");
    let before_b = std::fs::read_to_string(&file_b).expect("readable");
    let a = std::fs::read_to_string(&file_a).expect("readable");
    assert!(a.contains("github") && !a.contains("sentry"), "{a}");
    assert!(
        before_b.contains("sentry") && !before_b.contains("github"),
        "{before_b}"
    );

    // Their capabilities are distinct, so neither agent's harness can redeem
    // the other's secrets even though both files sit under one nest.
    assert_ne!(plan_a.set, plan_b.set);

    // Toggling agent A's selection leaves agent B's generated file untouched.
    converge(
        &paths,
        &both,
        &[
            selection(AGENT_A, placement, &["gh", "sn"]),
            selection(AGENT_B, placement, &["sn"]),
        ],
        &GenerationInputs {
            launcher: LAUNCHER,
            keychain_service: SERVICE,
            pending: &BTreeMap::new(),
        },
        &store,
        &nonces,
    )
    .expect("second convergence");
    plan_for_spawn(&paths, AGENT_A, placement, binding_reader(&store)).expect("A replans");
    plan_for_spawn(&paths, AGENT_B, placement, binding_reader(&store)).expect("B replans");
    assert!(std::fs::read_to_string(&file_a)
        .expect("readable")
        .contains("sentry"));
    assert_eq!(
        std::fs::read_to_string(&file_b).expect("readable"),
        before_b,
        "agent B's generated file changed when agent A's toggle did"
    );
}

/// The regenerated native config is **installed**, never rewritten in place.
///
/// `plan_for_spawn` writes this file on every spawn, and a harness may already
/// be reading the previous generation's copy. `write_atomically` writes a
/// temporary and renames it, so the old inode stays whole for whoever holds it
/// and the path flips to the new one in a single step; a plain
/// `std::fs::write` would truncate the live file and let a concurrent reader
/// observe a prefix of the new configuration. Replacing `write_atomically`
/// with a direct write keeps the inode, which is what this asserts.
#[test]
#[cfg(unix)]
fn mcp_registry_a_regenerated_config_is_installed_by_rename() {
    use std::io::Read as _;
    use std::os::unix::fs::MetadataExt as _;

    let root = tempfile::tempdir().expect("tempdir");
    let paths = paths(root.path());
    let store = FakeStore::default();
    let nonces = CountingNonces::default();
    let placement = McpConfigPlacement::ProjectFileInWorkdir { file: ".mcp.json" };
    let both = registry(&format!(
        "{},{}",
        stdio("gh", "github"),
        stdio("sn", "sentry")
    ));

    converge(
        &paths,
        &both,
        &[selection(AGENT_A, placement, &["gh"])],
        &GenerationInputs {
            launcher: LAUNCHER,
            keychain_service: SERVICE,
            pending: &BTreeMap::new(),
        },
        &store,
        &nonces,
    )
    .expect("the first convergence");
    let plan = plan_for_spawn(&paths, AGENT_A, placement, binding_reader(&store))
        .expect("the first plan resolves");
    let workdir = plan.workdir.clone().expect("a project file needs a cwd");
    let file = workdir.join(".mcp.json");
    let first = std::fs::metadata(&file).expect("readable").ino();
    // A harness that already opened the file, as one running under the
    // previous generation would have.
    let mut held = std::fs::File::open(&file).expect("the previous generation opens");

    converge(
        &paths,
        &both,
        &[selection(AGENT_A, placement, &["gh", "sn"])],
        &GenerationInputs {
            launcher: LAUNCHER,
            keychain_service: SERVICE,
            pending: &BTreeMap::new(),
        },
        &store,
        &nonces,
    )
    .expect("the second convergence");
    plan_for_spawn(&paths, AGENT_A, placement, binding_reader(&store))
        .expect("the second plan resolves");

    assert_ne!(
        first,
        std::fs::metadata(&file).expect("readable").ino(),
        "the regenerated config replaced the live file in place, so a harness reading it could \
         observe a prefix of the new generation"
    );

    let mut previous = String::new();
    held.read_to_string(&mut previous)
        .expect("the held handle still reads");
    assert!(
        previous.contains("github") && !previous.contains("sentry"),
        "the held handle saw a torn file: {previous}"
    );

    let installed = std::fs::read_to_string(&file).expect("readable");
    assert!(
        installed.contains("github") && installed.contains("sentry"),
        "{installed}"
    );

    // The temporary the install renamed from leaves nothing behind for a
    // reader to pick up.
    let leftovers: Vec<String> = std::fs::read_dir(&workdir)
        .expect("the working directory lists")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

/// The env-rooted placement writes under the agent's own working directory and
/// names it with the variable the runtime catalog gives.
#[test]
fn mcp_registry_an_env_rooted_placement_gets_its_own_root() {
    let root = tempfile::tempdir().expect("tempdir");
    let paths = paths(root.path());
    let store = FakeStore::default();
    let placement = McpConfigPlacement::EnvRootedDir {
        var: "CODEX_HOME",
        file: "config.toml",
    };
    converge(
        &paths,
        &registry(&stdio("gh", "github")),
        &[selection(AGENT_A, placement, &["gh"])],
        &GenerationInputs {
            launcher: LAUNCHER,
            keychain_service: SERVICE,
            pending: &BTreeMap::new(),
        },
        &store,
        &CountingNonces::default(),
    )
    .expect("converges");

    let plan = plan_for_spawn(&paths, AGENT_A, placement, binding_reader(&store))
        .expect("the plan resolves");
    let workdir = plan.workdir.clone().expect("an env-rooted dir needs a cwd");
    let home = plan
        .set
        .iter()
        .find(|(key, _)| key == "CODEX_HOME")
        .map(|(_, value)| value.clone())
        .expect("the root variable is set");
    assert!(
        std::path::Path::new(&home).starts_with(&workdir),
        "{home} is not inside {}",
        workdir.display()
    );
    let body =
        std::fs::read_to_string(std::path::Path::new(&home).join("config.toml")).expect("readable");
    assert!(body.contains("mcp_servers"), "{body}");
    assert!(body.contains(LAUNCHER), "{body}");
}

/// A refusal staged by the convergence refuses the spawn, with the loader's own
/// message. The loader does not run at spawn, so this is the only thing
/// standing between a rejected entry and an agent that starts silently short of
/// a server it was told to have.
#[test]
fn mcp_registry_a_staged_refusal_refuses_the_spawn() {
    let root = tempfile::tempdir().expect("tempdir");
    let paths = paths(root.path());
    let store = FakeStore::default();
    let outcome = converge(
        &paths,
        &registry(&stdio("gh", "github")),
        // `sn` is not in the registry at all.
        &[selection(AGENT_A, McpConfigPlacement::Unsupported, &["sn"])],
        &GenerationInputs {
            launcher: LAUNCHER,
            keychain_service: SERVICE,
            pending: &BTreeMap::new(),
        },
        &store,
        &CountingNonces::default(),
    )
    .expect("the rest of the registry still loads");
    assert_eq!(outcome.refused.len(), 1);
    assert_eq!(outcome.refused[0].0, AGENT_A);

    let error = plan_for_spawn(
        &paths,
        AGENT_A,
        McpConfigPlacement::Unsupported,
        binding_reader(&store),
    )
    .expect_err("an agent with a rejected entry must not start");
    assert_eq!(error, outcome.refused[0].1);
    assert!(error.contains("sn"), "{error}");
}

/// Generated servers with no binding record cannot authenticate, so the spawn
/// refuses rather than starting an agent whose every secret read fails.
#[test]
fn mcp_registry_generated_servers_without_a_binding_refuse_the_spawn() {
    let root = tempfile::tempdir().expect("tempdir");
    let paths = paths(root.path());
    let store = FakeStore::default();
    converge(
        &paths,
        &registry(&stdio("gh", "github")),
        &[selection(AGENT_A, McpConfigPlacement::Unsupported, &["gh"])],
        &GenerationInputs {
            launcher: LAUNCHER,
            keychain_service: SERVICE,
            pending: &BTreeMap::new(),
        },
        &store,
        &CountingNonces::default(),
    )
    .expect("converges");

    let error = plan_for_spawn(&paths, AGENT_A, McpConfigPlacement::Unsupported, |_| {
        Ok(None)
    })
    .expect_err("a missing binding record must refuse the spawn");
    assert!(error.contains("binding record"), "{error}");

    // An unavailable store is surfaced as itself, never as "no record".
    let error = plan_for_spawn(&paths, AGENT_A, McpConfigPlacement::Unsupported, |_| {
        Err("keychain unavailable".to_string())
    })
    .expect_err("an unavailable store must refuse the spawn");
    assert!(error.contains("unavailable"), "{error}");
}

/// With no adopted generation, and with a generation that staged nothing for
/// this agent, the spawn gets an empty plan — no variables, no relocation.
#[test]
fn mcp_registry_an_agent_with_no_servers_gets_an_empty_plan() {
    let root = tempfile::tempdir().expect("tempdir");
    let paths = paths(root.path());
    let store = FakeStore::default();

    assert_eq!(
        plan_for_spawn(
            &paths,
            AGENT_A,
            McpConfigPlacement::Unsupported,
            binding_reader(&store)
        )
        .expect("no generation is not an error"),
        McpSpawnPlan::default()
    );

    converge(
        &paths,
        &registry(&stdio("gh", "github")),
        &[selection(AGENT_A, McpConfigPlacement::Unsupported, &["gh"])],
        &GenerationInputs {
            launcher: LAUNCHER,
            keychain_service: SERVICE,
            pending: &BTreeMap::new(),
        },
        &store,
        &CountingNonces::default(),
    )
    .expect("converges");
    let plan = plan_for_spawn(
        &paths,
        AGENT_B,
        McpConfigPlacement::Unsupported,
        binding_reader(&store),
    )
    .expect("an agent the generation says nothing about is not an error");
    assert!(plan.is_empty());
}

/// Every variable a plan sets, at **every** placement, is one the spawn strips
/// first — so an ambient value inherited from the desktop cannot stand in for
/// one the plan did not set.
///
/// Driving only the handed-over placement is what let `CODEX_HOME` sit outside
/// the stripped set: an `EnvRootedDir` plan sets it, and a Codex agent whose
/// plan is empty would otherwise inherit the operator's own Codex root.
/// Dropping any name from `managed_env_vars()` fails this.
#[test]
fn mcp_registry_managed_variables_cover_what_a_plan_sets() {
    let placements = [
        McpConfigPlacement::Unsupported,
        McpConfigPlacement::ProjectFileInWorkdir { file: ".mcp.json" },
        McpConfigPlacement::EnvRootedDir {
            var: "CODEX_HOME",
            file: "config.toml",
        },
    ];
    for placement in placements {
        let root = tempfile::tempdir().expect("tempdir");
        let paths = paths(root.path());
        let store = FakeStore::default();
        converge(
            &paths,
            &registry(&stdio("gh", "github")),
            &[selection(AGENT_A, placement, &["gh"])],
            &GenerationInputs {
                launcher: LAUNCHER,
                keychain_service: SERVICE,
                pending: &BTreeMap::new(),
            },
            &store,
            &CountingNonces::default(),
        )
        .expect("converges");

        let plan =
            plan_for_spawn(&paths, AGENT_A, placement, binding_reader(&store)).expect("resolves");
        assert!(!plan.set.is_empty(), "{placement:?} set nothing");
        for (key, _) in &plan.set {
            assert!(
                managed_env_vars().contains(&key.as_str()),
                "{key} is set by a {placement:?} plan but never stripped before the user env layer"
            );
        }
    }
}

/// The registry configures only a runtime whose isolated configuration root
/// has been verified on a real launch, which memo decision 9 says is
/// buzz-agent alone in v1.
///
/// The gate is the shipped predicate `mcp_registry_spawn_plan` calls. Widening
/// it — back to `!mcp_transports.is_empty()`, or by flipping one runtime's
/// availability — fails here, which is the point: Claude and Codex both
/// declare `[Stdio, Http]`, so a transports-based gate turns both on and moves
/// their working directories off the nest.
#[test]
fn mcp_registry_only_verified_runtimes_are_configured() {
    use crate::managed_agents::discovery::KNOWN_ACP_RUNTIMES;
    use crate::managed_agents::runtime::registry_configures;

    let configured: Vec<&str> = KNOWN_ACP_RUNTIMES
        .iter()
        .filter(|runtime| registry_configures(Some(runtime)))
        .map(|runtime| runtime.id)
        .collect();
    assert_eq!(
        configured,
        vec!["buzz-agent"],
        "memo decision 9: no other runtime's isolated root has been verified"
    );

    for id in ["claude", "codex"] {
        let runtime = KNOWN_ACP_RUNTIMES
            .iter()
            .find(|runtime| runtime.id == id)
            .expect("catalog entry");
        assert!(
            !runtime.mcp_transports.is_empty(),
            "{id} declares transports, so a transports-based gate would configure it"
        );
        assert!(!registry_configures(Some(runtime)), "{id} was configured");
    }
    assert!(
        !registry_configures(None),
        "an unknown harness must never be configured"
    );
}

/// One pointer names one generation for every agent, so a convergence is
/// whole-set by construction.
///
/// Converging a subset would delete every other agent's `mcp:` records — their
/// credentials and their binding record — because the deletion sweep is by
/// generation, and would leave them with no artefacts under the adopted
/// generation either. Refuse instead, and leave the store untouched.
#[test]
fn mcp_registry_converge_refuses_a_partial_agent_set() {
    let root = tempfile::tempdir().expect("tempdir");
    let paths = paths(root.path());
    let store = FakeStore::default();
    let registry = registry(&format!(
        "{},{}",
        stdio("gh", "github"),
        stdio("ln", "linear")
    ));
    converge(
        &paths,
        &registry,
        &[
            selection(AGENT_A, McpConfigPlacement::Unsupported, &["gh"]),
            selection(AGENT_B, McpConfigPlacement::Unsupported, &["ln"]),
        ],
        &GenerationInputs {
            launcher: LAUNCHER,
            keychain_service: SERVICE,
            pending: &BTreeMap::new(),
        },
        &store,
        &CountingNonces::default(),
    )
    .expect("converges");
    let before = store.map();
    assert!(before.contains_key(&binding_key_for(AGENT_B, 1)));

    let error = converge(
        &paths,
        &registry,
        &[selection(AGENT_A, McpConfigPlacement::Unsupported, &["gh"])],
        &GenerationInputs {
            launcher: LAUNCHER,
            keychain_service: SERVICE,
            pending: &BTreeMap::new(),
        },
        &store,
        &CountingNonces::default(),
    )
    .expect_err("a partial convergence must be refused");
    assert!(
        matches!(&error, ConvergeError::MissingAgent(agent) if agent == AGENT_B),
        "{error:?}"
    );
    assert_eq!(store.map(), before, "the refused convergence changed state");

    // The other agent is still spawnable, which is what the refusal protects.
    assert!(plan_for_spawn(
        &paths,
        AGENT_B,
        McpConfigPlacement::Unsupported,
        binding_reader(&store)
    )
    .expect("resolves")
    .set
    .iter()
    .any(|(key, _)| key == CAPABILITY_ENV_VAR));
}

/// Every generated argv names the keychain service the **desktop** stores its
/// blob under.
///
/// The launcher's own default is the release name `buzz-desktop`, while the
/// desktop uses `buzz-desktop-dev` in debug builds and a per-slug name on demo
/// builds. Omitting the flag would let every spawn succeed and every reference
/// then fail to resolve, on every build but release-non-demo.
#[test]
fn mcp_registry_generated_argv_names_the_desktops_keychain_service() {
    let service = crate::app_state::keyring_service();
    let root = tempfile::tempdir().expect("tempdir");
    let paths = paths(root.path());
    let store = FakeStore::default();
    converge(
        &paths,
        &registry(&stdio("gh", "github")),
        &[selection(AGENT_A, McpConfigPlacement::Unsupported, &["gh"])],
        &GenerationInputs {
            launcher: LAUNCHER,
            keychain_service: service,
            pending: &BTreeMap::new(),
        },
        &store,
        &CountingNonces::default(),
    )
    .expect("converges");

    let body = std::fs::read_to_string(
        paths
            .generations_root()
            .join("generations")
            .join("1")
            .join(AGENTS_SUBDIR)
            .join(AGENT_A)
            .join(BUZZ_ACP_REGISTRY_FILE),
    )
    .expect("readable");
    let document: serde_json::Value = serde_json::from_str(&body).expect("valid json");
    let args = document["servers"][0]["args"]
        .as_array()
        .expect("generated args");
    let flag = args
        .iter()
        .position(|arg| arg == "--service")
        .expect("no generated argv names a keychain service");
    assert_eq!(
        args[flag + 1],
        serde_json::Value::String(service.to_string()),
        "the generated argv names a service the desktop does not write to"
    );
    // The default the flag overrides, so the two names cannot silently agree.
    assert_eq!(
        buzz_mcp_launch_pkg::cli::DEFAULT_KEYCHAIN_SERVICE,
        "buzz-desktop"
    );
}

/// An agent id is the only caller-supplied component of these paths, so it is
/// validated rather than trusted: a separator or `..` would put one agent's
/// generated configuration in another agent's directory.
#[test]
fn mcp_registry_a_hostile_agent_id_reaches_no_path() {
    let root = tempfile::tempdir().expect("tempdir");
    let paths = paths(root.path());
    let store = FakeStore::default();

    for hostile in ["../escape", "a/b", "", "UPPER", &"x".repeat(200)] {
        assert!(
            plan_for_spawn(
                &paths,
                hostile,
                McpConfigPlacement::Unsupported,
                binding_reader(&store)
            )
            .is_err(),
            "spawn accepted the agent id {hostile:?}"
        );
        assert!(matches!(
            converge(
                &paths,
                &registry(&stdio("gh", "github")),
                &[selection(hostile, McpConfigPlacement::Unsupported, &["gh"])],
                &GenerationInputs {
                    launcher: LAUNCHER,
                    keychain_service: SERVICE,
                    pending: &BTreeMap::new(),
                },
                &store,
                &CountingNonces::default(),
            ),
            Err(ConvergeError::AgentId(_))
        ));
    }
}

/// Two selections for one agent would make which one wins depend on list
/// order, and more agents than the cap would grow the staged tree, the blob and
/// the journal without bound.
#[test]
fn mcp_registry_converge_bounds_agents_and_refuses_duplicates() {
    let root = tempfile::tempdir().expect("tempdir");
    let paths = paths(root.path());
    let store = FakeStore::default();
    let registry = registry(&stdio("gh", "github"));

    assert!(matches!(
        converge(
            &paths,
            &registry,
            &[
                selection(AGENT_A, McpConfigPlacement::Unsupported, &["gh"]),
                selection(AGENT_A, McpConfigPlacement::Unsupported, &[]),
            ],
            &GenerationInputs {
                launcher: LAUNCHER,
                keychain_service: SERVICE,
                pending: &BTreeMap::new(),
            },
            &store,
            &CountingNonces::default(),
        ),
        Err(ConvergeError::DuplicateAgent(_))
    ));

    let many: Vec<AgentSelection> = (0..=super::converge::MAX_CONVERGED_AGENTS)
        .map(|index| {
            selection(
                &format!("agent-{index}"),
                McpConfigPlacement::Unsupported,
                &[],
            )
        })
        .collect();
    assert!(matches!(
        converge(
            &paths,
            &registry,
            &many,
            &GenerationInputs {
                launcher: LAUNCHER,
                keychain_service: SERVICE,
                pending: &BTreeMap::new(),
            },
            &store,
            &CountingNonces::default(),
        ),
        Err(ConvergeError::TooManyAgents { .. })
    ));
}

/// No generated file, at any placement, carries a resolved secret value or the
/// capability nonce. The agent can read every one of them.
#[test]
fn mcp_registry_no_generated_file_carries_a_secret() {
    let root = tempfile::tempdir().expect("tempdir");
    let paths = paths(root.path());
    let store = FakeStore::default();
    let placement = McpConfigPlacement::ProjectFileInWorkdir { file: ".mcp.json" };
    converge(
        &paths,
        &registry(&stdio("gh", "github")),
        &[selection(AGENT_A, placement, &["gh"])],
        &GenerationInputs {
            launcher: LAUNCHER,
            keychain_service: SERVICE,
            pending: &BTreeMap::new(),
        },
        &store,
        &CountingNonces::default(),
    )
    .expect("converges");

    let capability = AgentCapability::bind(
        AGENT_A,
        1,
        store
            .map()
            .get(&binding_key_for(AGENT_A, 1))
            .expect("bound"),
    )
    .expect("rebuilds");
    store.insert(
        &storage_key(&capability, &reference("gh-token")),
        "GITHUB-SECRET",
    );

    let plan =
        plan_for_spawn(&paths, AGENT_A, placement, binding_reader(&store)).expect("resolves");
    let workdir = plan.workdir.clone().expect("cwd");
    for file in [
        workdir.join(".mcp.json"),
        paths
            .generations_root()
            .join("generations")
            .join("1")
            .join(AGENTS_SUBDIR)
            .join(AGENT_A)
            .join(".mcp.json"),
    ] {
        let body = std::fs::read_to_string(&file).expect("readable");
        assert!(!body.contains("GITHUB-SECRET"), "{body}");
        assert!(!body.contains(capability.binding_value()), "{body}");
        assert!(body.contains("mcp:gh-token"), "{body}");
    }
    assert!(!std::fs::read_to_string(workdir.join(".mcp.json"))
        .expect("readable")
        .contains(REFUSAL_FILE));
}

fn reference(id: &str) -> McpSecretRef {
    McpSecretRef::parse(&format!("mcp:{id}")).expect("valid reference")
}

// ── The spawn seam in `runtime.rs` ────────────────────────────────────────

/// The strip and the apply, driven through the production functions
/// `spawn_agent_child` calls.
///
/// `std::process::Command` records explicit overrides, so a removal is
/// observable as `(key, None)` and a set as `(key, Some(value))` without
/// spawning anything. Emptying `managed_env_vars()`, or dropping either loop
/// body, fails this — and deleting either *call* from `spawn_agent_child` is a
/// compile error, because the strip's token is consumed by the apply and the
/// apply's by `spawn_with_effort_proof`.
#[test]
fn mcp_registry_the_spawn_seam_strips_before_it_sets() {
    use std::ffi::OsStr;

    let root = tempfile::tempdir().expect("tempdir");
    let paths = paths(root.path());
    let store = FakeStore::default();
    converge(
        &paths,
        &registry(&stdio("gh", "github")),
        &[selection(AGENT_A, McpConfigPlacement::Unsupported, &["gh"])],
        &GenerationInputs {
            launcher: LAUNCHER,
            keychain_service: SERVICE,
            pending: &BTreeMap::new(),
        },
        &store,
        &CountingNonces::default(),
    )
    .expect("converges");
    let plan = plan_for_spawn(
        &paths,
        AGENT_A,
        McpConfigPlacement::Unsupported,
        binding_reader(&store),
    )
    .expect("resolves");

    // An ambient value for every managed variable, as a harness environment
    // carrying a stale generation's capability would have.
    let mut command = std::process::Command::new("true");
    for key in managed_env_vars() {
        command.env(key, "AMBIENT");
    }
    let stripped = crate::managed_agents::strip_mcp_registry_env(&mut command);
    let overrides: Vec<(String, Option<String>)> = command
        .get_envs()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.map(OsStr::to_string_lossy).map(|v| v.into_owned()),
            )
        })
        .collect();
    for key in managed_env_vars() {
        assert_eq!(
            overrides
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone()),
            Some(None),
            "{key} was not stripped before the user env layer"
        );
    }

    // The user env layer runs between the two, so a saved value would land
    // here — and the apply below has to win.
    command.env(CAPABILITY_ENV_VAR, "SAVED-BY-THE-USER");
    // The token is what `spawn_with_effort_proof` consumes in production;
    // this test's job ends at the command it produced.
    let _applied = crate::managed_agents::apply_mcp_registry_env(&mut command, &plan, stripped);

    let applied: BTreeMap<String, Option<String>> = command
        .get_envs()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.map(OsStr::to_string_lossy).map(|v| v.into_owned()),
            )
        })
        .collect();
    for (key, value) in &plan.set {
        assert_eq!(
            applied.get(key),
            Some(&Some(value.clone())),
            "{key} did not reach the spawn command"
        );
    }
    assert_ne!(
        applied.get(CAPABILITY_ENV_VAR),
        Some(&Some("SAVED-BY-THE-USER".to_string())),
        "a saved user value shadowed this spawn's capability"
    );
    // buzz-agent's placement leaves the shared nest as the working directory.
    assert_eq!(command.get_current_dir(), None);
}

/// A saved user env layer cannot supply either registry variable.
///
/// The strip runs before the `descriptor.env` loop and the apply runs after it,
/// but the apply writes only what this spawn's plan holds — and an empty plan
/// is every spawn with no registry servers, which is every spawn today. So a
/// saved `BUZZ_ACP_MCP_REGISTRY` (a code-execution surface: `buzz-acp` spawns
/// every command the file names) or `BUZZ_MCP_CAPABILITY` (a bearer token for
/// every `mcp:` record bound to the agent) would otherwise survive the seam.
/// Both names are in `RESERVED_ENV_KEYS`, so `merged_user_env` — the producer
/// of every user-settable layer of `descriptor.env` — drops them first.
///
/// Removing either entry from `RESERVED_ENV_KEYS` fails this test at the merge,
/// and emptying the plan's apply cannot rescue it.
#[test]
fn mcp_registry_a_user_env_layer_cannot_reach_the_spawned_command() {
    use std::ffi::OsStr;

    let managed = [BUZZ_ACP_REGISTRY_ENV_VAR, CAPABILITY_ENV_VAR];
    let mut saved = BTreeMap::new();
    for key in managed {
        saved.insert(key.to_string(), "SAVED-BY-THE-USER".to_string());
    }

    // The `descriptor.env` the spawn loop iterates: persona env under the
    // record's own overrides, through the production merge.
    let descriptor_env = crate::managed_agents::env_vars::merged_user_env(&saved, &saved);
    for key in managed {
        assert!(
            !descriptor_env.contains_key(key),
            "{key} survived the user env layer"
        );
    }

    // The seam itself, with the empty plan every non-registry spawn has.
    let mut command = std::process::Command::new("true");
    let stripped = crate::managed_agents::strip_mcp_registry_env(&mut command);
    for (key, value) in &descriptor_env {
        command.env(key, value);
    }
    let _applied = crate::managed_agents::apply_mcp_registry_env(
        &mut command,
        &McpSpawnPlan::default(),
        stripped,
    );

    let overrides: BTreeMap<String, Option<String>> = command
        .get_envs()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.map(OsStr::to_string_lossy).map(|v| v.into_owned()),
            )
        })
        .collect();
    for key in managed {
        assert_eq!(
            overrides.get(key),
            Some(&None),
            "{key} reached the spawned command"
        );
    }
}
