//! Tests for the production caller (T7c): the seam where a registry edit or a
//! per-agent toggle becomes an adopted generation, the guards that seam
//! asserts, and the producer/consumer bounds it has to agree with.
//!
//! Every one drives shipped code — `apply::checked_launcher`,
//! `apply::selection_for_record`, `converge`, `GenerationStore::reconcile`,
//! `plan_for_spawn`, the shipped loader — and each names the guard whose
//! removal fails it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use buzz_secret_store_pkg::capability::NONCE_LEN;

use super::apply::{checked_launcher, selection_for_record};
use super::converge::{converge, AgentSelection, GenerationInputs, NonceSource, SecretStoreIo};
use super::generation::{
    Deletion, FlipHooks, FlipStep, GenerationError, GenerationPlan, GenerationStore, JournalPhase,
    NoHooks, Reconciled, SecretRemover, MAX_PLAN_FILES,
};
use super::load::parse_registry;
use super::paths::RegistryPaths;
use super::schema::{
    MAX_ARG_LEN, MAX_ENV_NAME_LEN, MAX_ENV_VALUE_LEN, MAX_GENERATED_ARGS, MAX_NAME_LEN,
};
use super::spawn::plan_for_spawn;
use crate::managed_agents::{McpConfigPlacement, McpTransport};

/// The bundled launcher's stand-in. Absolute on both hosts.
#[cfg(unix)]
const LAUNCHER: &str = "/Applications/Buzz.app/Contents/MacOS/buzz-mcp-launch";
#[cfg(windows)]
const LAUNCHER: &str = "C:/Buzz/buzz-mcp-launch.exe";

const SERVICE: &str = "buzz-desktop-dev";

#[cfg(unix)]
const SERVER_BIN: &str = "/usr/local/bin";
#[cfg(windows)]
const SERVER_BIN: &str = "C:/buzz/bin";

const AGENT: &str = "aaaabbbbccccdddd";

/// A store a test drives instead of the machine keychain.
#[derive(Default)]
struct FakeStore {
    records: Mutex<BTreeMap<String, String>>,
    fail_removes: Mutex<bool>,
}

impl SecretRemover for FakeStore {
    fn remove(&self, key: &str) -> Result<(), String> {
        if *self.fail_removes.lock().unwrap() {
            return Err(format!("injected: cannot remove {key}"));
        }
        self.records.lock().unwrap().remove(key);
        Ok(())
    }

    fn write_all(&self, entries: &BTreeMap<String, String>) -> Result<(), String> {
        let mut guard = self.records.lock().unwrap();
        for (key, value) in entries {
            guard.insert(key.clone(), value.clone());
        }
        Ok(())
    }
}

impl SecretStoreIo for FakeStore {
    fn read_all(&self) -> Result<BTreeMap<String, String>, String> {
        Ok(self.records.lock().unwrap().clone())
    }
}

struct FixedNonce;

impl NonceSource for FixedNonce {
    fn nonce(&self) -> [u8; NONCE_LEN] {
        [7u8; NONCE_LEN]
    }
}

fn document(servers: &str) -> String {
    format!(
        "{{\"version\":1,\"servers\":[{}]}}",
        servers.replace("/usr/local/bin", SERVER_BIN)
    )
}

fn stdio(id: &str, name: &str) -> String {
    format!(
        "{{\"id\":\"{id}\",\"name\":\"{name}\",\"transport\":\"stdio\",\"command\":\"/usr/local/bin/{name}-mcp\",\"args\":[\"--stdio\"]}}"
    )
}

fn paths(root: &Path) -> RegistryPaths {
    RegistryPaths::new(root.join("base"), root.join("nest"))
}

fn selection(enabled: &[&str], transports: &[McpTransport]) -> AgentSelection {
    AgentSelection {
        agent_id: AGENT.to_string(),
        runtime_id: "buzz-agent".to_string(),
        transports: transports.to_vec(),
        placement: McpConfigPlacement::Unsupported,
        enabled: enabled.iter().map(|id| (*id).to_string()).collect(),
    }
}

/// A minimal agent record, built through the same deserializer the store uses
/// so the test cannot drift from what a real `managed-agents.json` produces.
fn record() -> crate::managed_agents::types::ManagedAgentRecord {
    serde_json::from_value(serde_json::json!({
        "pubkey": AGENT,
        "name": "copy",
        "relay_url": "wss://relay.example",
        "acp_command": "buzz-acp",
        "agent_command": "buzz-agent",
        "agent_args": [],
        "mcp_command": "",
        "turn_timeout_seconds": 0,
        "system_prompt": null,
        "created_at": "2026-09-05T00:00:00Z",
        "updated_at": "2026-09-05T00:00:00Z",
        "last_started_at": null,
        "last_stopped_at": null,
        "last_exit_code": null,
        "last_error": null,
    }))
    .expect("the record fixture deserializes")
}

fn binding_reader(store: &FakeStore) -> impl Fn(&str) -> Result<Option<String>, String> + '_ {
    move |key: &str| Ok(store.records.lock().unwrap().get(key).cloned())
}

fn converge_with(
    root: &Path,
    document_body: &str,
    selections: &[AgentSelection],
    store: &FakeStore,
    pending: &BTreeMap<String, String>,
) -> Result<super::converge::Converged, super::converge::ConvergeError> {
    let registry = parse_registry(document_body.as_bytes()).expect("document loads");
    converge(
        &paths(root),
        &registry,
        selections,
        &GenerationInputs {
            launcher: LAUNCHER,
            keychain_service: SERVICE,
            pending,
        },
        store,
        &FixedNonce,
    )
}

// ── The wiring seam's own assertions ──────────────────────────────────────

/// PR 23 follow-up 2. Every generated server names the launcher, and
/// `buzz-acp` refuses a relative command — so a relative or missing launcher
/// would make every registry-enabled agent fail to boot, discovered at spawn
/// instead of at the save that caused it. Deleting either arm of
/// `checked_launcher` makes one half of this pass a bad value through.
#[test]
fn mcp_registry_the_wiring_seam_refuses_a_launcher_that_is_not_an_absolute_file() {
    let temporary = tempfile::tempdir().expect("tempdir");

    let relative = PathBuf::from("buzz-mcp-launch");
    let error = checked_launcher(&relative).expect_err("a relative launcher is refused");
    assert!(
        error.contains("not an absolute path"),
        "the refusal must name the defect, got {error}"
    );

    let missing = temporary.path().join("buzz-mcp-launch");
    let error = checked_launcher(&missing).expect_err("a missing launcher is refused");
    assert!(
        error.contains("cannot be read"),
        "the refusal must name the defect, got {error}"
    );

    let directory = temporary.path().join("not-a-binary");
    std::fs::create_dir(&directory).expect("mkdir");
    let error = checked_launcher(&directory).expect_err("a directory is refused");
    assert!(
        error.contains("not a regular file"),
        "the refusal must name the defect, got {error}"
    );

    let real = temporary.path().join("buzz-mcp-launch-real");
    std::fs::write(&real, b"#!/bin/sh\n").expect("write");
    assert_eq!(
        checked_launcher(&real).expect("a real absolute file is accepted"),
        real.display().to_string()
    );
}

/// Memo decision 9 through the shipped selection builder: what an agent brings
/// to a convergence comes from its record and from the runtime catalog, and a
/// runtime the registry may not configure contributes an empty selection —
/// never a dropped agent, because a convergence is whole-set.
#[test]
fn mcp_registry_a_selection_reads_the_record_and_the_runtime_gate() {
    let mut record = record();
    record.mcp_servers = Some(crate::managed_agents::types::AgentMcpServers {
        version: crate::managed_agents::types::AGENT_MCP_SERVERS_VERSION,
        enabled: vec!["fake".to_string()],
    });

    let available = crate::managed_agents::known_acp_runtime_exact("buzz-agent")
        .expect("buzz-agent is in the catalog");
    assert!(
        available.mcp_registry_available,
        "this test's premise is that buzz-agent is the verified runtime"
    );
    let selection = selection_for_record(&record, Some(available), "buzz-agent");
    assert_eq!(selection.enabled, vec!["fake".to_string()]);
    assert_eq!(selection.transports, vec![McpTransport::Stdio]);

    let withheld =
        crate::managed_agents::known_acp_runtime_exact("claude").expect("claude is in the catalog");
    assert!(
        !withheld.mcp_registry_available,
        "decision 9 keeps claude unverified"
    );
    let selection = selection_for_record(&record, Some(withheld), "claude");
    assert!(
        selection.enabled.is_empty(),
        "an unverified runtime contributes nothing, but is still passed"
    );
    assert_eq!(selection.agent_id, AGENT, "and is never dropped");

    let unknown = selection_for_record(&record, None, "somebody-elses-harness");
    assert!(unknown.enabled.is_empty());
    assert_eq!(unknown.placement, McpConfigPlacement::Unsupported);
}

/// Memo decision 8's absent-versus-empty distinction, at the one place it is
/// durable: the record's own serialization. Absent is a record written before
/// the registry existed; an empty list is an operator who turned everything
/// off. Collapsing the two would make a later default change silently reach
/// records nobody chose it for.
#[test]
fn mcp_registry_an_absent_selection_is_a_different_record_from_an_empty_one() {
    let mut record = record();

    let absent = serde_json::to_value(&record).expect("serialize");
    assert!(
        absent.get("mcp_servers").is_none(),
        "an absent selection writes no key at all"
    );

    record.mcp_servers = Some(crate::managed_agents::types::AgentMcpServers {
        version: crate::managed_agents::types::AGENT_MCP_SERVERS_VERSION,
        enabled: Vec::new(),
    });
    let empty = serde_json::to_value(&record).expect("serialize");
    assert_eq!(
        empty["mcp_servers"]["enabled"],
        serde_json::json!([]),
        "an empty selection writes an empty list"
    );
    assert_eq!(empty["mcp_servers"]["version"], 1, "and its version");

    let round_tripped: crate::managed_agents::types::ManagedAgentRecord =
        serde_json::from_value(empty).expect("deserialize");
    assert_eq!(
        round_tripped.mcp_servers,
        Some(crate::managed_agents::types::AgentMcpServers {
            version: 1,
            enabled: Vec::new()
        }),
        "and reads back as chosen-empty, not as absent"
    );
}

// ── The two user actions ──────────────────────────────────────────────────

/// A registry edit adopts a new generation, and the previous one's artefact
/// stops being what a spawn reads. This is the production behaviour PR 23
/// could not have: `converge` had no caller, so `plan_for_spawn` found no
/// adopted generation and the registry was inert in a shipped build.
#[test]
fn mcp_registry_a_registry_edit_adopts_a_new_generation() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path();
    let store = FakeStore::default();
    let selections = [selection(&["fake"], &[McpTransport::Stdio])];

    let first = converge_with(
        root,
        &document(&stdio("fake", "fake")),
        &selections,
        &store,
        &BTreeMap::new(),
    )
    .expect("first convergence");
    assert_eq!(first.generation, 1);

    let plan = plan_for_spawn(
        &paths(root),
        AGENT,
        McpConfigPlacement::Unsupported,
        binding_reader(&store),
    )
    .expect("a spawn resolves the adopted generation");
    let handover = plan
        .set
        .iter()
        .find(|(name, _)| name == "BUZZ_ACP_MCP_REGISTRY")
        .map(|(_, value)| value.clone())
        .expect("the handover file is named");
    assert!(
        std::fs::read_to_string(&handover)
            .expect("read")
            .contains("fake"),
        "generation 1 carries the server the document declared"
    );

    // The operator deletes the server from the document but leaves the toggle
    // on. The generation still moves, and it carries the refusal — never a
    // silently shorter server list.
    let second = converge_with(root, &document(""), &selections, &store, &BTreeMap::new())
        .expect("the convergence succeeds; the agent carries a refusal");
    assert_eq!(second.generation, 2);
    assert_eq!(second.refused.len(), 1, "{second:?}");
    let error = plan_for_spawn(
        &paths(root),
        AGENT,
        McpConfigPlacement::Unsupported,
        binding_reader(&store),
    )
    .expect_err("and the spawn refuses rather than starting one server short");
    assert!(error.contains("no longer declares it"), "got {error}");

    // The complete edit: the toggle goes too.
    let third = converge_with(
        root,
        &document(""),
        &[selection(&[], &[McpTransport::Stdio])],
        &store,
        &BTreeMap::new(),
    )
    .expect("third convergence");
    assert_eq!(third.generation, 3, "the pointer moved");
    let plan = plan_for_spawn(
        &paths(root),
        AGENT,
        McpConfigPlacement::Unsupported,
        binding_reader(&store),
    )
    .expect("a spawn resolves the new generation");
    assert!(
        plan.is_empty(),
        "generation 2 stages nothing for this agent, so its next spawn gets no server"
    );
}

/// A per-agent toggle change adopts a new generation whose artefact holds
/// exactly the new selection — with the document untouched. Deleting the
/// convergence call from the toggle command leaves the agent spawning from the
/// old generation, which this test catches by reading the file a spawn reads.
#[test]
fn mcp_registry_a_toggle_change_adopts_a_new_generation() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path();
    let store = FakeStore::default();
    let body = document(&format!("{},{}", stdio("one", "one"), stdio("two", "two")));

    converge_with(
        root,
        &body,
        &[selection(&["one"], &[McpTransport::Stdio])],
        &store,
        &BTreeMap::new(),
    )
    .expect("first convergence");

    let read_handover = || {
        let plan = plan_for_spawn(
            &paths(root),
            AGENT,
            McpConfigPlacement::Unsupported,
            binding_reader(&store),
        )
        .expect("spawn plan");
        let path = plan
            .set
            .iter()
            .find(|(name, _)| name == "BUZZ_ACP_MCP_REGISTRY")
            .map(|(_, value)| value.clone())
            .expect("handover file");
        std::fs::read_to_string(path).expect("read")
    };

    let before = read_handover();
    assert!(before.contains("\"one\""), "{before}");
    assert!(!before.contains("\"two\""), "{before}");

    let toggled = converge_with(
        root,
        &body,
        &[selection(&["one", "two"], &[McpTransport::Stdio])],
        &store,
        &BTreeMap::new(),
    )
    .expect("toggle convergence");
    assert_eq!(toggled.generation, 2);

    let after = read_handover();
    assert!(after.contains("\"one\""), "{after}");
    assert!(after.contains("\"two\""), "{after}");
}

/// Memo decision 2 through the whole production seam: an http entry toggled on
/// for a stdio-only runtime is *refused*, and the refusal is staged so the
/// spawn carries it too. Never silently dropped — an agent short a server it
/// was told to have is a behaviour change the operator cannot see.
#[test]
fn mcp_registry_an_http_entry_on_buzz_agent_is_refused_not_dropped() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path();
    let store = FakeStore::default();
    let body = document(
        "{\"id\":\"remote\",\"name\":\"remote\",\"transport\":\"http\",\"url\":\"https://mcp.example/v1\"}",
    );

    let converged = converge_with(
        root,
        &body,
        &[selection(&["remote"], &[McpTransport::Stdio])],
        &store,
        &BTreeMap::new(),
    )
    .expect("the convergence itself succeeds; the agent carries a refusal");
    assert_eq!(converged.refused.len(), 1, "{converged:?}");
    let (agent, message) = &converged.refused[0];
    assert_eq!(agent, AGENT);
    assert!(
        message.contains("http") && message.contains("buzz-agent"),
        "the panel's message must name the transport and the runtime, got {message}"
    );

    let error = plan_for_spawn(
        &paths(root),
        AGENT,
        McpConfigPlacement::Unsupported,
        binding_reader(&store),
    )
    .expect_err("the spawn refuses rather than starting one server short");
    assert!(error.contains("http"), "got {error}");
}

// ── Start-up reconcile ────────────────────────────────────────────────────

/// The app-start reconcile PR 23 deferred. A crash between the pointer rename
/// and the `FLIPPED` journal write leaves a `PREPARED` journal naming the
/// generation the pointer already resolves to — an adopted change one write
/// short of its record. The reconcile must finish it, not discard it:
/// discarding would delete the live configuration and leave `current` naming a
/// directory that no longer exists.
#[test]
fn mcp_registry_reconcile_at_start_finishes_an_adopted_generation() {
    struct FailAfterRename;
    impl FlipHooks for FailAfterRename {
        fn after(&self, step: FlipStep) -> Result<(), String> {
            match step {
                FlipStep::PointerRenamed => Err("crash".to_string()),
                _ => Ok(()),
            }
        }
    }

    let temporary = tempfile::tempdir().expect("tempdir");
    let store = GenerationStore::open(&temporary.path().join("mcp")).expect("open");
    let secrets = FakeStore::default();
    secrets
        .records
        .lock()
        .unwrap()
        .insert("mcp:stale:0:token".to_string(), "value".to_string());

    let error = store
        .commit(
            |_base, _dir| {
                Ok(GenerationPlan {
                    files: vec![(PathBuf::from("agents/x/file.json"), "{}".to_string())],
                    deletions: vec![Deletion::Secret {
                        key: "mcp:stale:0:token".to_string(),
                    }],
                    secrets: BTreeMap::new(),
                })
            },
            &secrets,
            &FailAfterRename,
        )
        .expect_err("the injected crash aborts the commit");
    assert!(
        matches!(error, GenerationError::Injected { .. }),
        "{error:?}"
    );

    assert_eq!(
        store.current().expect("pointer"),
        Some(1),
        "already adopted"
    );
    let journal = store.journal().expect("journal").expect("outstanding");
    assert_eq!(journal.phase, JournalPhase::Prepared);
    assert_eq!(journal.generation, 1);
    assert!(
        secrets
            .records
            .lock()
            .unwrap()
            .contains_key("mcp:stale:0:token"),
        "the deletion is still owed"
    );

    let outcome = store.reconcile(&secrets, &NoHooks).expect("reconcile");
    assert_eq!(
        outcome,
        Reconciled::CompletedCleanup {
            generation: 1,
            deletions: 1
        }
    );
    assert_eq!(
        store.current().expect("pointer"),
        Some(1),
        "the adopted generation survives the reconcile"
    );
    assert!(
        store.generation_dir(1).join("agents/x/file.json").exists(),
        "and so does the configuration it staged"
    );
    assert!(
        !secrets
            .records
            .lock()
            .unwrap()
            .contains_key("mcp:stale:0:token"),
        "and the owed deletion is done"
    );
    assert!(store.journal().expect("journal").is_none(), "and CLEANED");
}

// ── Sol N8, N9, N10 ───────────────────────────────────────────────────────

/// Sol N8. A `Deletion` reaches the store from a journal *file*, so the key is
/// re-read rather than re-derived. `identity` is the human nsec and
/// `agent:<pubkey>` is an agent's; a tampered or corrupted journal that named
/// either would destroy a private key nothing on this machine can rebuild.
/// Removing `Deletion::validate` lets the fake store observe the delete.
#[test]
fn mcp_registry_a_deletion_outside_the_mcp_namespace_is_refused() {
    for key in ["identity", "agent:deadbeef", "some-other-key"] {
        let error = Deletion::Secret {
            key: key.to_string(),
        }
        .validate()
        .expect_err("a key outside the namespace is refused");
        assert!(
            error.contains(key),
            "the refusal must name the key, got {error}"
        );
    }
    Deletion::Secret {
        key: "mcp:agentid:1:token".to_string(),
    }
    .validate()
    .expect("an `mcp:` key is the registry's own");

    // And the refusal reaches the retry loop rather than the store: the
    // journal is on disk, so this is the only place that can catch it.
    let temporary = tempfile::tempdir().expect("tempdir");
    let store = GenerationStore::open(&temporary.path().join("mcp")).expect("open");
    let secrets = FakeStore::default();
    secrets
        .records
        .lock()
        .unwrap()
        .insert("identity".to_string(), "nsec1...".to_string());
    let error = store
        .commit(
            |_base, _dir| {
                Ok(GenerationPlan {
                    files: Vec::new(),
                    deletions: vec![Deletion::Secret {
                        key: "identity".to_string(),
                    }],
                    secrets: BTreeMap::new(),
                })
            },
            &secrets,
            &NoHooks,
        )
        .expect_err("a plan naming the human nsec is refused");
    assert!(
        matches!(error, GenerationError::Plan(_)),
        "refused before anything is staged, got {error:?}"
    );
    assert!(
        secrets.records.lock().unwrap().contains_key("identity"),
        "and the human nsec is still there"
    );
    assert_eq!(
        store.current().expect("pointer"),
        None,
        "nothing was adopted"
    );
}

/// Sol N8, the plan's own shape. A staged path is joined onto the generation
/// directory, so an absolute path or a `..` component would write outside the
/// tree the pointer rename covers. The count and the total bytes are bounded
/// because both are what the write actually costs.
#[test]
fn mcp_registry_a_plan_that_escapes_the_generation_is_refused() {
    let escaping = GenerationPlan {
        files: vec![(PathBuf::from("agents/../../etc/passwd"), "x".to_string())],
        ..GenerationPlan::default()
    };
    let error = escaping.validate().expect_err("`..` is refused");
    assert!(format!("{error}").contains("relative path"), "{error}");

    let too_many = GenerationPlan {
        files: (0..MAX_PLAN_FILES + 1)
            .map(|n| (PathBuf::from(format!("agents/a/{n}.json")), String::new()))
            .collect(),
        ..GenerationPlan::default()
    };
    let error = too_many.validate().expect_err("the file count is bounded");
    assert!(format!("{error}").contains("over the"), "{error}");

    let oversized = GenerationPlan {
        files: vec![(
            PathBuf::from("agents/a/b.json"),
            "x".repeat(super::paths::MAX_ARTEFACT_BYTES + 1),
        )],
        ..GenerationPlan::default()
    };
    let error = oversized
        .validate()
        .expect_err("a file over what a spawn will read is refused");
    assert!(format!("{error}").contains("cap"), "{error}");

    GenerationPlan {
        files: vec![(PathBuf::from("agents/a/b.json"), "{}".to_string())],
        secrets: BTreeMap::from([("mcp:a:1:token".to_string(), "v".to_string())]),
        deletions: vec![Deletion::Generation { number: 1 }],
    }
    .validate()
    .expect("an ordinary plan passes");
}

/// Sol N9. `Path::exists` answers "a `stat` succeeded", which is true of a
/// FIFO — and opening a FIFO blocks until a writer appears, with no timeout
/// anywhere on the spawn path. The type is read from `symlink_metadata` and
/// anything but a regular file is refused, so a planted FIFO is a refusal
/// rather than a spawn that never returns.
#[cfg(unix)]
#[test]
fn mcp_registry_a_fifo_artefact_is_refused_rather_than_opened() {
    use std::os::unix::ffi::OsStrExt as _;

    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path();
    let store = FakeStore::default();
    converge_with(
        root,
        &document(&stdio("fake", "fake")),
        &[selection(&["fake"], &[McpTransport::Stdio])],
        &store,
        &BTreeMap::new(),
    )
    .expect("convergence");

    let generations = GenerationStore::open(&paths(root).generations_root()).expect("open");
    let staged = generations.generation_dir(1).join("agents").join(AGENT);
    let artefact = staged.join(super::paths::BUZZ_ACP_REGISTRY_FILE);
    std::fs::remove_file(&artefact).expect("remove the real artefact");
    let c_path = std::ffi::CString::new(artefact.as_os_str().as_bytes()).expect("cstring");
    // SAFETY-free: a plain libc call with a NUL-terminated path and a mode.
    assert_eq!(
        unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) },
        0,
        "mkfifo failed: {}",
        std::io::Error::last_os_error()
    );

    let error = plan_for_spawn(
        &paths(root),
        AGENT,
        McpConfigPlacement::Unsupported,
        binding_reader(&store),
    )
    .expect_err("a FIFO where an artefact belongs refuses the spawn");
    assert!(
        error.contains("not a regular file"),
        "the refusal must say why, got {error}"
    );
}

/// Sol N10. `journal.json.next` and `current.next` are fixed names in a
/// directory the whole user account can write. `File::create` follows a
/// symlink, so a link planted at either name would have the staging write
/// truncate the link's target. `O_NOFOLLOW` fails the open instead, and the
/// failure is propagated rather than swallowed.
#[cfg(unix)]
#[test]
fn mcp_registry_a_symlink_at_a_staging_name_fails_loudly() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let victim = temporary.path().join("victim.txt");
    std::fs::write(&victim, b"do not truncate me").expect("write");

    let root = temporary.path().join("mcp");
    let store = GenerationStore::open(&root).expect("open");
    std::os::unix::fs::symlink(&victim, root.join("journal.json.next")).expect("symlink");

    let secrets = FakeStore::default();
    let error = store
        .commit(
            |_base, _dir| {
                Ok(GenerationPlan {
                    files: vec![(PathBuf::from("agents/x/file.json"), "{}".to_string())],
                    ..GenerationPlan::default()
                })
            },
            &secrets,
            &NoHooks,
        )
        .expect_err("a planted symlink is a loud failure");
    assert!(
        matches!(
            error,
            GenerationError::Io {
                operation: "create",
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&victim).expect("read"),
        "do not truncate me",
        "and the link's target is untouched"
    );
    assert_eq!(
        store.current().expect("pointer"),
        None,
        "nothing was adopted"
    );
}

/// PR 23 follow-up 3, the observable half. `write_atomically` installs by
/// rename, leaves no temporary behind, and — because the temporary is created
/// without following a link — refuses a symlink planted at its fixed `.tmp`
/// name rather than truncating what the link points at. The `fsync` calls
/// beside them are not observable from a test; the rename and the no-follow
/// are, and they are the same function's contract.
#[cfg(unix)]
#[test]
fn mcp_registry_a_spawn_write_installs_by_rename_and_follows_no_link() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path();
    let store = FakeStore::default();
    let mut agent = selection(&["fake"], &[McpTransport::Stdio]);
    agent.placement = McpConfigPlacement::ProjectFileInWorkdir { file: ".mcp.json" };
    agent.runtime_id = "claude".to_string();

    converge_with(
        root,
        &document(&stdio("fake", "fake")),
        &[agent],
        &store,
        &BTreeMap::new(),
    )
    .expect("convergence");

    let plan = plan_for_spawn(
        &paths(root),
        AGENT,
        McpConfigPlacement::ProjectFileInWorkdir { file: ".mcp.json" },
        binding_reader(&store),
    )
    .expect("spawn plan");
    let workdir = plan.workdir.clone().expect("a project placement moves cwd");
    assert!(workdir.join(".mcp.json").exists(), "the file is installed");
    assert!(
        !workdir.join(".tmp").exists() && !workdir.join(".mcp.tmp").exists(),
        "and no temporary is left behind"
    );

    // Plant a link at the temporary's fixed name and rewrite.
    let victim = temporary.path().join("victim.txt");
    std::fs::write(&victim, b"do not truncate me").expect("write");
    std::os::unix::fs::symlink(&victim, workdir.join(".mcp.tmp")).expect("symlink");
    let error = plan_for_spawn(
        &paths(root),
        AGENT,
        McpConfigPlacement::ProjectFileInWorkdir { file: ".mcp.json" },
        binding_reader(&store),
    )
    .expect_err("a planted link at the temporary name refuses the spawn");
    assert!(error.contains("failed to create"), "got {error}");
    assert_eq!(
        std::fs::read_to_string(&victim).expect("read"),
        "do not truncate me"
    );
}

// ── Sol W4: producer and consumer bounds ──────────────────────────────────

/// Sol W4, the arithmetic half. Every bound the desktop applies to a string
/// that crosses into `buzz-acp` is pinned to that crate's own constant, so a
/// change on either side fails here instead of at a customer's next launch.
#[test]
fn mcp_registry_argument_bounds_match_the_consumer() {
    assert_eq!(
        MAX_ARG_LEN,
        buzz_acp_pkg::mcp_registry::MAX_REGISTRY_ARG_LEN,
        "an argument the desktop accepts must be one the harness accepts"
    );
    assert_eq!(
        MAX_GENERATED_ARGS,
        buzz_acp_pkg::mcp_registry::MAX_REGISTRY_ARGS,
        "the generated command line is bounded by what the harness reads"
    );
    assert_eq!(
        MAX_ENV_NAME_LEN + 1 + MAX_ENV_VALUE_LEN,
        MAX_ARG_LEN,
        "a declared variable is generated as one `NAME=VALUE` argument, so the worst case has \
         to fit by construction"
    );
}

/// Sol W4, the behavioural half, driven on **both** sides. A name the harness
/// refuses must be refused by the desktop loader first: the harness rejects
/// the whole handover document, so one over-long or underscored name accepted
/// here would stop every registry-enabled agent from starting.
#[test]
fn mcp_registry_name_bounds_match_the_consumer_on_both_sides() {
    let launcher = std::path::Path::new(LAUNCHER);
    let handover = |name: &str| {
        serde_json::json!({
            "version": 1,
            "servers": [{ "name": name, "command": LAUNCHER, "args": [] }]
        })
        .to_string()
    };
    let consumer_refuses = |name: &str| {
        buzz_acp_pkg::mcp_registry::parse_registry_file(handover(name).as_bytes(), 16, launcher)
            .err()
    };

    // The consumer's cap is *discovered*, not copied: `MAX_MCP_NAME_LEN` is
    // crate-private, and a second copy of a number is exactly the defect W4
    // named. Widening the desktop's own cap therefore fails here rather than
    // silently agreeing with itself.
    let consumer_cap = (1..=64usize)
        .take_while(|len| consumer_refuses(&"a".repeat(*len)).is_none())
        .last()
        .expect("the harness accepts some name length");
    assert!(
        MAX_NAME_LEN <= consumer_cap,
        "the desktop accepts a {MAX_NAME_LEN}-byte name while the harness refuses anything over \
         {consumer_cap}, so every registry-enabled agent would fail to start"
    );

    for (name, why) in [
        (
            "a".repeat(consumer_cap + 1),
            "a name one byte over the harness's cap",
        ),
        ("has_underscore".to_string(), "an underscored name"),
    ] {
        let consumer = consumer_refuses(&name)
            .unwrap_or_else(|| panic!("the harness must refuse {why}: {name}"));
        assert!(
            consumer.contains("name"),
            "the harness's refusal must be about the name, got {consumer}"
        );

        let entry = format!(
            "{{\"id\":\"x\",\"name\":\"{name}\",\"transport\":\"stdio\",\"command\":\"/usr/local/bin/x\",\"args\":[]}}"
        );
        let loaded = parse_registry(document(&entry).as_bytes()).expect("the document loads");
        let rejection = loaded.entries[0]
            .rejection
            .clone()
            .unwrap_or_else(|| panic!("the desktop loader must refuse {why}: {name}"));
        assert!(
            rejection.contains("name"),
            "the desktop's refusal must be about the name, got {rejection}"
        );
    }

    // And the shape both sides do accept still loads.
    let ok = parse_registry(document(&stdio("fine", "fine-server")).as_bytes()).expect("loads");
    assert!(ok.entries[0].rejection.is_none(), "{:?}", ok.entries[0]);
}

/// The bound that actually costs is the generated command line, not the
/// entry's own `args`: the generator prepends flags and adds two arguments per
/// declared variable, and the harness bounds the sum. Counting it beside the
/// generator is what keeps the count and the generation in step — a flag added
/// to one without the other fails here.
#[test]
fn mcp_registry_generated_arg_count_matches_the_generator() {
    let body = document(
        "{\"id\":\"one\",\"name\":\"one\",\"transport\":\"stdio\",\"command\":\"/usr/local/bin/one\",\
          \"args\":[\"--a\",\"--b\"],\"env\":{\"A\":\"literal\",\"B\":\"mcp:token\"}},\
         {\"id\":\"two\",\"name\":\"two\",\"transport\":\"http\",\"url\":\"https://mcp.example/v1\",\
          \"auth\":{\"scheme\":\"bearer\",\"secret\":\"mcp:token\"}}",
    );
    let loaded = parse_registry(body.as_bytes()).expect("loads");
    for entry in &loaded.entries {
        let generated = super::generate::generate_server(LAUNCHER, SERVICE, &entry.entry);
        assert_eq!(
            super::generate::generated_arg_count(&entry.entry),
            generated.args.len(),
            "the count and the generator disagree for `{}`",
            entry.entry.id
        );
    }

    // And an entry that would generate more than the harness reads is refused
    // per entry, with the rest of the registry still loading.
    let args: Vec<String> = (0..MAX_GENERATED_ARGS)
        .map(|n| format!("\"--a{n}\""))
        .collect();
    let fat = format!(
        "{{\"id\":\"fat\",\"name\":\"fat\",\"transport\":\"stdio\",\"command\":\"/usr/local/bin/fat\",\"args\":[{}]}}",
        args.join(",")
    );
    let loaded = parse_registry(document(&format!("{fat},{}", stdio("ok", "ok"))).as_bytes())
        .expect("the document still loads");
    let rejection = loaded.entries[0]
        .rejection
        .clone()
        .expect("the fat entry is refused");
    assert!(rejection.contains("launcher command line"), "{rejection}");
    assert!(
        loaded.entries[1].rejection.is_none(),
        "the rest still loads"
    );
}

/// Follow-up 8's shape guard, decided and bound: a NUL cannot cross `execve`,
/// so a command or an argument carrying one is refused at the loader where the
/// operator can see why, rather than truncating at the OS boundary.
#[test]
fn mcp_registry_a_nul_in_a_command_line_is_refused() {
    let with_nul = "{\"id\":\"n\",\"name\":\"n\",\"transport\":\"stdio\",\
        \"command\":\"/usr/local/bin/n\",\"args\":[\"--flag\\u0000hidden\"]}";
    let loaded = parse_registry(document(with_nul).as_bytes()).expect("the document loads");
    let rejection = loaded.entries[0]
        .rejection
        .clone()
        .expect("an argument with a NUL is refused");
    assert!(rejection.contains("NUL"), "{rejection}");

    let long_command = format!(
        "{{\"id\":\"l\",\"name\":\"l\",\"transport\":\"stdio\",\"command\":\"/usr/local/bin/{}\",\"args\":[]}}",
        "x".repeat(MAX_ARG_LEN)
    );
    let loaded = parse_registry(document(&long_command).as_bytes()).expect("loads");
    let rejection = loaded.entries[0]
        .rejection
        .clone()
        .expect("an over-long command is refused");
    assert!(rejection.contains("over the"), "{rejection}");
}

/// A secret the operator types is written into the adopted generation's
/// keyspace under the reserved `mcp:` prefix, bound to the agent and the
/// generation — and appears in no generated file. This is the whole of "stored
/// once, never echoed back".
#[test]
fn mcp_registry_a_typed_secret_reaches_the_store_and_no_generated_file() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path();
    let store = FakeStore::default();
    let body = document(
        "{\"id\":\"auth\",\"name\":\"auth\",\"transport\":\"stdio\",\
          \"command\":\"/usr/local/bin/auth\",\"args\":[],\"env\":{\"API_KEY\":\"mcp:api-key\"}}",
    );
    let pending = BTreeMap::from([("api-key".to_string(), "sk-live-do-not-log".to_string())]);

    converge_with(
        root,
        &body,
        &[selection(&["auth"], &[McpTransport::Stdio])],
        &store,
        &pending,
    )
    .expect("convergence");

    let records = store.records.lock().unwrap().clone();
    let key = format!("mcp:{AGENT}:1:api-key");
    assert_eq!(
        records.get(&key).map(String::as_str),
        Some("sk-live-do-not-log"),
        "the value is bound to (agent, generation) under the reserved prefix; got {:?}",
        records.keys().collect::<Vec<_>>()
    );

    let generations = GenerationStore::open(&paths(root).generations_root()).expect("open");
    let staged = generations.generation_dir(1);
    let mut checked = 0;
    for entry in walk(&staged) {
        let body = std::fs::read_to_string(&entry).expect("read");
        assert!(
            !body.contains("sk-live-do-not-log"),
            "{} carries the value",
            entry.display()
        );
        assert!(
            body.contains("mcp:api-key"),
            "{} lost the reference",
            entry.display()
        );
        checked += 1;
    }
    assert!(checked > 0, "no staged file was checked");
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(walk(&path));
        } else {
            found.push(path);
        }
    }
    found
}

#[test]
fn mcp_registry_a_pending_secret_for_an_unselected_server_is_not_silently_dropped() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path();
    let store = FakeStore::default();
    let body = document(
        "{\"id\":\"auth\",\"name\":\"auth\",\"transport\":\"stdio\",\
          \"command\":\"/usr/local/bin/auth\",\"args\":[],\"env\":{\"API_KEY\":\"mcp:api-key\"}}",
    );
    let pending = BTreeMap::from([("api-key".to_string(), "sk-live-do-not-log".to_string())]);

    // Converge with no agent enabling "auth".
    converge_with(
        root,
        &body,
        &[selection(&[], &[McpTransport::Stdio])],
        &store,
        &pending,
    )
    .expect("first convergence without agent selection");

    // Later, agent enables "auth" with no new pending secret.
    converge_with(
        root,
        &body,
        &[selection(&["auth"], &[McpTransport::Stdio])],
        &store,
        &BTreeMap::new(),
    )
    .expect("second convergence with agent selection");

    let records = store.records.lock().unwrap().clone();
    let key = format!("mcp:{AGENT}:2:api-key");
    assert_eq!(
        records.get(&key).map(String::as_str),
        Some("sk-live-do-not-log"),
        "the secret value must be durably retrievable by the agent's later toggle-on convergence; got {:?}",
        records.keys().collect::<Vec<_>>()
    );
}

/// A holding key stores a pending secret only until some selected agent
/// carries it forward under its own (agent, generation) key. Once that has
/// happened, the holding key must be consumed: a later convergence with no
/// new pending value must read the agent's own carried-forward value, not
/// fall back to a stale holding key that a subsequent rotation left behind.
#[test]
fn mcp_registry_a_consumed_holding_secret_does_not_roll_a_later_rotation_back() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path();
    let store = FakeStore::default();
    let body = document(
        "{\"id\":\"auth\",\"name\":\"auth\",\"transport\":\"stdio\",\
          \"command\":\"/usr/local/bin/auth\",\"args\":[],\"env\":{\"API_KEY\":\"mcp:api-key\"}}",
    );

    // 1. Save OLD while "auth" is unselected by any agent: OLD lands only in
    //    the holding key.
    let old_pending = BTreeMap::from([("api-key".to_string(), "sk-old".to_string())]);
    converge_with(
        root,
        &body,
        &[selection(&[], &[McpTransport::Stdio])],
        &store,
        &old_pending,
    )
    .expect("first convergence without agent selection");

    // 2. Enable "auth" with no new pending secret: the agent's generation-2
    //    key is seeded from the holding key.
    converge_with(
        root,
        &body,
        &[selection(&["auth"], &[McpTransport::Stdio])],
        &store,
        &BTreeMap::new(),
    )
    .expect("second convergence with agent selection");

    // 3. Rotate to NEW while still selected: the operator's pending value
    //    supersedes whatever the holding key still carries.
    let new_pending = BTreeMap::from([("api-key".to_string(), "sk-new".to_string())]);
    converge_with(
        root,
        &body,
        &[selection(&["auth"], &[McpTransport::Stdio])],
        &store,
        &new_pending,
    )
    .expect("third convergence with rotated pending secret");

    // 4. Converge again with no pending secret at all.
    converge_with(
        root,
        &body,
        &[selection(&["auth"], &[McpTransport::Stdio])],
        &store,
        &BTreeMap::new(),
    )
    .expect("fourth convergence with no pending secret");

    let records = store.records.lock().unwrap().clone();
    let key = format!("mcp:{AGENT}:4:api-key");
    assert_eq!(
        records.get(&key).map(String::as_str),
        Some("sk-new"),
        "the fourth convergence must carry forward the rotated NEW value, \
         not roll back to the stale holding value; got {:?}",
        records.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        records.get(&super::converge::holding_key("api-key")),
        None,
        "the holding key must be consumed once an agent carries the \
         reference, not linger and later win over a fresher rotation; got {:?}",
        records.keys().collect::<Vec<_>>()
    );
}

/// A credential-shaped command path is rejected at load (`load.rs`), so an
/// agent that has it enabled refuses through the same production seam a
/// rejected argument uses — and the raw path never reaches a staged
/// artefact, because `resolve_for_agent` returns the refusal before
/// `generate_server` ever runs on the entry.
#[test]
fn mcp_registry_a_credential_shaped_command_is_refused_and_never_reaches_generated_argv() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path();
    let store = FakeStore::default();
    let body = document(
        "{\"id\":\"cred\",\"name\":\"cred\",\"transport\":\"stdio\",\
          \"command\":\"/tmp/sk-live-secret123/server\",\"args\":[]}",
    );

    let converged = converge_with(
        root,
        &body,
        &[selection(&["cred"], &[McpTransport::Stdio])],
        &store,
        &BTreeMap::new(),
    )
    .expect("convergence still adopts a generation; the refusal is per-agent, not fatal");

    assert_eq!(
        converged.refused.len(),
        1,
        "the agent must be refused for enabling a rejected entry; got {:?}",
        converged.refused
    );
    assert_eq!(converged.refused[0].0, AGENT);

    let generations = GenerationStore::open(&paths(root).generations_root()).expect("open");
    let staged = generations.generation_dir(1);
    let mut checked = 0;
    for entry in walk(&staged) {
        let content = std::fs::read_to_string(&entry).expect("read");
        assert!(
            !content.contains("sk-live-secret123"),
            "{} carries the credential-shaped command path",
            entry.display()
        );
        checked += 1;
    }
    assert!(checked > 0, "no staged file was checked");
}

/// An http entry's `env` block is accepted as valid syntax (an `mcp:`
/// reference) but has no effect: the http generator hands nothing to a child
/// process. Saving such an entry with a pending secret for that reference
/// must be rejected, and the operator's credential must never be retained
/// anywhere in the secret store — not under an agent's carried key and not as
/// an unassigned holding secret either.
#[test]
fn mcp_registry_an_http_entrys_env_is_rejected_and_its_secret_is_never_retained() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path();
    let store = FakeStore::default();
    let body = document(
        "{\"id\":\"authed\",\"name\":\"authed\",\"transport\":\"http\",\
          \"url\":\"https://mcp.example/v1\",\"env\":{\"API_TOKEN\":\"mcp:token\"}}",
    );

    let loaded = parse_registry(body.as_bytes()).expect("document loads");
    let entry = loaded.by_id("authed").expect("present");
    assert!(
        !entry.is_usable(),
        "an http entry with a non-empty env must be rejected at validation"
    );

    let pending = BTreeMap::from([("token".to_string(), "sk-do-not-retain".to_string())]);
    let converged = converge_with(
        root,
        &body,
        &[selection(&["authed"], &[McpTransport::Http])],
        &store,
        &pending,
    )
    .expect("convergence still adopts a generation; the rejection is per-agent, not fatal");

    assert_eq!(
        converged.refused.len(),
        1,
        "the agent must be refused for enabling a rejected entry; got {:?}",
        converged.refused
    );

    let records = store.records.lock().unwrap().clone();
    assert!(
        records.values().all(|value| value != "sk-do-not-retain"),
        "the credential must never be retained anywhere in the store; got {:?}",
        records.keys().collect::<Vec<_>>()
    );
    assert!(
        !records
            .keys()
            .any(|key| super::converge::is_holding_key(key)),
        "an http env reference must not be held as an unassigned pending \
         secret either, since nothing will ever consume it; got {:?}",
        records.keys().collect::<Vec<_>>()
    );
}

#[test]
fn mcp_registry_deleted_agent_retires_mcp_state_and_allows_subsequent_convergence() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path();
    let store = FakeStore::default();
    let body = document(
        "{\"id\":\"auth\",\"name\":\"auth\",\"transport\":\"stdio\",\
          \"command\":\"/usr/local/bin/auth\",\"args\":[],\"env\":{\"API_KEY\":\"mcp:api-key\"}}",
    );
    let agent_a = "aaaaaaaaaaaaaaaa";
    let agent_b = "bbbbbbbbbbbbbbbb";
    let sel_a = AgentSelection {
        agent_id: agent_a.to_string(),
        runtime_id: "buzz-agent".to_string(),
        transports: vec![McpTransport::Stdio],
        placement: McpConfigPlacement::Unsupported,
        enabled: vec!["auth".to_string()],
    };
    let sel_b = AgentSelection {
        agent_id: agent_b.to_string(),
        runtime_id: "buzz-agent".to_string(),
        transports: vec![McpTransport::Stdio],
        placement: McpConfigPlacement::Unsupported,
        enabled: vec![],
    };
    let pending = BTreeMap::from([("api-key".to_string(), "sk-secret".to_string())]);

    // 1. Initial convergence: both A and B exist, A has auth enabled.
    converge_with(root, &body, &[sel_a, sel_b.clone()], &store, &pending)
        .expect("initial convergence");
    assert!(store
        .records
        .lock()
        .unwrap()
        .contains_key(&format!("mcp:{agent_a}:1:api-key")));

    // 2. Delete A: simulate post-deletion convergence with records = [B]
    let mut rec_b = record();
    rec_b.pubkey = agent_b.to_string();
    let records = vec![rec_b];

    let selections =
        super::apply::selections_for_records(&records, &[], &Default::default(), &store);
    converge_with(root, &body, &selections, &store, &BTreeMap::new())
        .expect("post-deletion convergence must retire deleted agent");

    // 3. Subsequent convergence for remaining agent B must succeed without MissingAgent
    let subsequent = converge_with(root, &body, &[sel_b], &store, &BTreeMap::new());
    assert!(
        subsequent.is_ok(),
        "must succeed without MissingAgent, got: {:?}",
        subsequent
    );

    // 4. Assert A's keys in store are gone
    let current_keys = store.records.lock().unwrap().clone();
    let a_keys: Vec<_> = current_keys
        .keys()
        .filter(|k| k.contains(agent_a))
        .collect();
    assert!(
        a_keys.is_empty(),
        "agent A keys should be gone, found: {:?}",
        a_keys
    );
}

// ── `converge_now` — the real production caller ────────────────────────────

/// A sandboxed `HOME`/`PATH` for the one test below that has to exercise the
/// real `converge_now_with_records` (`apply.rs`), which resolves the app data
/// directory and the agent nest from the real environment.
///
/// Every other test in this file drives `converge()` directly against a
/// caller-chosen root, which is deliberately independent of `HOME`; this is
/// the only one that needs the production entry point itself, so it is the
/// only one paying for this fixture. Same technique
/// `commands::mcp_registry_tests::EnvGuard` uses for the same reason, and the
/// same shared `lock_path_mutex`, so the two cannot race each other over the
/// process-global environment.
struct SandboxedHome {
    _guard: std::sync::MutexGuard<'static, ()>,
    temp: tempfile::TempDir,
    old_home: Option<std::ffi::OsString>,
    old_xdg: Option<std::ffi::OsString>,
    old_path: Option<std::ffi::OsString>,
}

impl SandboxedHome {
    fn new() -> Self {
        let guard = crate::managed_agents::lock_path_mutex();
        crate::managed_agents::clear_resolve_cache();
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        let bin = temp.path().join("bin");
        std::fs::create_dir_all(&home).expect("mkdir home");
        std::fs::create_dir_all(&bin).expect("mkdir bin");

        // `converge_now_with_records` must get past `checked_launcher` before
        // it ever reaches the personas/global-config read this test targets.
        let launcher = bin.join(super::apply::LAUNCHER_COMMAND);
        std::fs::write(&launcher, b"#!/bin/sh\nexit 0\n").expect("write launcher");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }

        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        let old_path = std::env::var_os("PATH");
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_DATA_HOME", &home);
        let new_path = old_path
            .as_ref()
            .map(|p| format!("{}:{}", bin.display(), p.to_string_lossy()))
            .unwrap_or_else(|| bin.display().to_string());
        std::env::set_var("PATH", &new_path);

        Self {
            _guard: guard,
            temp,
            old_home,
            old_xdg,
            old_path,
        }
    }

    fn home(&self) -> PathBuf {
        self.temp.path().join("home")
    }
}

impl Drop for SandboxedHome {
    fn drop(&mut self) {
        crate::managed_agents::clear_resolve_cache();
        match &self.old_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match &self.old_xdg {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        match &self.old_path {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }
    }
}

fn mock_app() -> tauri::App<tauri::test::MockRuntime> {
    tauri::test::mock_builder()
        .manage(crate::app_state::build_app_state())
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .expect("mock app builds headless")
}

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

    let generations_root = RegistryPaths::new(base, sandbox.home()).generations_root();
    let before = GenerationStore::open(&generations_root)
        .expect("open")
        .current()
        .expect("readable");
    assert_eq!(before, None, "nothing has converged yet");

    let error = super::apply::converge_now_with_records(app.handle(), &[], &BTreeMap::new())
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
