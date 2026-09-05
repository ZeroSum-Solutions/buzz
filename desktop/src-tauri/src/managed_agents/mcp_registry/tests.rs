//! Tests for the registry core. Every one drives the shipped loader, the
//! shipped resolver, the shipped generators or the shipped generation store —
//! deleting the guard each names fails it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use super::generate::{
    generate_server, render_buzz_acp_registry, render_claude_project_config, render_codex_config,
};
use super::generation::{
    Deletion, FlipHooks, FlipStep, GenerationError, GenerationPlan, GenerationStore, JournalPhase,
    NoHooks, NoSecrets, Reconciled, SecretRemover, MAX_POINTER_BYTES,
};
use super::load::{parse_registry, RegistryError};
use super::resolve::{resolve_for_agent, SpawnRefusal};
use super::schema::{
    MAX_DOCUMENT_BYTES, MAX_DOCUMENT_SERVERS, MAX_ENTRY_BYTES, MAX_SERVERS_PER_AGENT,
};
use crate::managed_agents::McpTransport;

const LAUNCHER: &str = "/Applications/Buzz.app/Contents/MacOS/buzz-mcp-launch";

/// The keychain service a generated argv names. In production this is the
/// desktop's own `keyring_service()`; here it is a fixed stand-in.
const SERVICE: &str = "buzz-desktop-dev";

/// Where the fixtures put a server binary.
///
/// The loader refuses a command the host calls relative, and `is_absolute` is
/// host-specific: on Windows a root with no volume — `/usr/local/bin/x` — is
/// relative to the current drive, so a Unix path would have every fixture
/// below rejected for the one rule it is not about. Forward slashes are a
/// Windows separator too, so a single shape serves both hosts.
#[cfg(unix)]
const SERVER_BIN: &str = "/usr/local/bin";
#[cfg(windows)]
const SERVER_BIN: &str = "C:/buzz/bin";

/// Wrap `servers` in a document, with their commands rooted where this host
/// calls absolute. The fixtures are written Unix-first; this is the one place
/// that rewrites them, so each stays a readable JSON literal.
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

// ── Loader ────────────────────────────────────────────────────────────────

#[test]
fn mcp_registry_loads_valid_entries() {
    let registry = parse_registry(
        document(&format!(
            "{},{}",
            stdio("a", "github"),
            stdio("b", "linear")
        ))
        .as_bytes(),
    )
    .expect("a valid document loads");
    assert_eq!(registry.entries.len(), 2);
    assert!(registry.entries.iter().all(|entry| entry.is_usable()));
    assert_eq!(
        registry.by_id("a").map(|e| e.entry.name.as_str()),
        Some("github")
    );
}

#[test]
fn mcp_registry_duplicates_reject_the_whole_document_in_either_order() {
    // Which of two colliding entries survives would otherwise be document
    // order, and both native targets key servers by name, so one command would
    // silently keep the other's credential.
    let first = stdio("a", "github");
    let second = stdio("b", "GitHub");
    let forwards = parse_registry(document(&format!("{first},{second}")).as_bytes())
        .expect_err("a name collision must reject the document");
    let backwards = parse_registry(document(&format!("{second},{first}")).as_bytes())
        .expect_err("a name collision must reject the document");
    assert_eq!(forwards, backwards);
    assert_eq!(
        forwards,
        RegistryError::Duplicate {
            kind: "name",
            value: "github".to_string()
        }
    );

    let same_id = format!("{},{}", stdio("a", "github"), stdio("a", "linear"));
    assert_eq!(
        parse_registry(document(&same_id).as_bytes()).expect_err("an id collision rejects too"),
        RegistryError::Duplicate {
            kind: "id",
            value: "a".to_string()
        }
    );
}

#[test]
fn mcp_registry_document_and_entry_caps_reject_the_whole_document() {
    let oversized_document = vec![b'x'; MAX_DOCUMENT_BYTES + 1];
    assert_eq!(
        parse_registry(&oversized_document).expect_err("over the document cap"),
        RegistryError::DocumentTooLarge
    );

    let padding = "a".repeat(MAX_ENTRY_BYTES);
    let fat = format!(
        "{{\"id\":\"a\",\"name\":\"github\",\"transport\":\"stdio\",\"command\":\"/usr/local/bin/x\",\"args\":[\"{padding}\"]}}"
    );
    assert_eq!(
        parse_registry(document(&fat).as_bytes()).expect_err("over the entry cap"),
        RegistryError::EntryTooLarge("a".to_string())
    );

    let many: Vec<String> = (0..=MAX_DOCUMENT_SERVERS)
        .map(|index| stdio(&format!("s{index}"), &format!("srv{index}")))
        .collect();
    assert_eq!(
        parse_registry(document(&many.join(",")).as_bytes()).expect_err("over the server cap"),
        RegistryError::TooManyServers(MAX_DOCUMENT_SERVERS + 1)
    );
}

#[test]
fn mcp_registry_rejects_entries_one_at_a_time_and_keeps_loading() {
    let entries = [
        stdio("ok", "github"),
        // Reserved prefix.
        stdio("reserved", "buzz-tools"),
        // Built-in collision.
        stdio("builtin", "developer"),
        // Relative command.
        "{\"id\":\"relative\",\"name\":\"relcmd\",\"transport\":\"stdio\",\"command\":\"node\",\"args\":[]}".to_string(),
        // Credential in argv.
        "{\"id\":\"argv\",\"name\":\"argvsecret\",\"transport\":\"stdio\",\"command\":\"/usr/local/bin/x\",\"args\":[\"--token\",\"ghp_0123456789abcdef\"]}".to_string(),
        // Non-loopback http.
        "{\"id\":\"insecure\",\"name\":\"insecure\",\"transport\":\"http\",\"url\":\"http://api.example.com/mcp\"}".to_string(),
        // Credential in the URL query.
        "{\"id\":\"query\",\"name\":\"querysecret\",\"transport\":\"http\",\"url\":\"https://api.example.com/mcp?access_token=abc123\"}".to_string(),
        // Userinfo in the URL.
        "{\"id\":\"userinfo\",\"name\":\"userinfo\",\"transport\":\"http\",\"url\":\"https://u:p@api.example.com/mcp\"}".to_string(),
        // Authenticated proxy value in env.
        "{\"id\":\"proxyenv\",\"name\":\"proxyenv\",\"transport\":\"stdio\",\"command\":\"/usr/local/bin/x\",\"env\":{\"HTTPS_PROXY\":\"http://employee:token@proxy.example\"}}".to_string(),
        // Literal credential in env.
        "{\"id\":\"envsecret\",\"name\":\"envsecret\",\"transport\":\"stdio\",\"command\":\"/usr/local/bin/x\",\"env\":{\"GITHUB_TOKEN\":\"ghp_0123456789abcdef\"}}".to_string(),
    ];
    let registry = parse_registry(document(&entries.join(",")).as_bytes())
        .expect("per-entry failures must not reject the document");
    assert_eq!(registry.entries.len(), entries.len());
    assert!(
        registry.by_id("ok").expect("present").is_usable(),
        "the rest of the registry must still load"
    );
    for id in [
        "reserved",
        "builtin",
        "relative",
        "argv",
        "insecure",
        "query",
        "userinfo",
        "proxyenv",
        "envsecret",
    ] {
        let entry = registry.by_id(id).expect("present");
        assert!(
            !entry.is_usable(),
            "`{id}` must be disabled, not silently accepted"
        );
        assert!(
            !entry.rejection.as_deref().unwrap_or_default().is_empty(),
            "`{id}` must carry a reason the panel can render"
        );
    }
}

#[test]
fn mcp_registry_accepts_references_where_it_refuses_values() {
    let entry = "{\"id\":\"a\",\"name\":\"github\",\"transport\":\"stdio\",\"command\":\"/usr/local/bin/x\",\"env\":{\"GITHUB_TOKEN\":\"mcp:github-token\"}}";
    let registry = parse_registry(document(entry).as_bytes()).expect("loads");
    assert!(
        registry.by_id("a").expect("present").is_usable(),
        "an `mcp:` reference is exactly what an operator is meant to write"
    );

    let bad_reference = "{\"id\":\"a\",\"name\":\"github\",\"transport\":\"stdio\",\"command\":\"/usr/local/bin/x\",\"env\":{\"GITHUB_TOKEN\":\"mcp:identity\"}}";
    let registry = parse_registry(document(bad_reference).as_bytes()).expect("loads");
    assert!(
        !registry.by_id("a").expect("present").is_usable(),
        "`mcp:identity` names the human nsec and must never resolve"
    );
}

#[test]
fn mcp_registry_reads_no_more_than_the_cap_and_follows_no_symlink() {
    let dir = tempfile::tempdir().expect("temp dir");
    let real = dir.path().join("real.json");
    std::fs::write(&real, document(&stdio("a", "github"))).expect("write");
    let missing = dir.path().join("absent.json");
    assert!(super::load::load_registry(&missing)
        .expect("a missing registry is the default state, not an error")
        .entries
        .is_empty());

    // A sparse file far over the cap is refused without being allocated.
    let huge = dir.path().join("huge.json");
    let file = std::fs::File::create(&huge).expect("create");
    file.set_len(4 * 1024 * 1024 * 1024).expect("sparse");
    drop(file);
    assert_eq!(
        super::load::load_registry(&huge).expect_err("over the cap"),
        RegistryError::DocumentTooLarge
    );

    #[cfg(unix)]
    {
        let link = dir.path().join("link.json");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let error = super::load::load_registry(&link).expect_err("a symlink must not be followed");
        assert!(matches!(error, RegistryError::Io { .. }), "{error:?}");
    }
}

// ── Resolution ────────────────────────────────────────────────────────────

#[test]
fn mcp_registry_rejected_entry_blocks_spawn() {
    let entries = format!(
        "{},{}",
        stdio("ok", "github"),
        "{\"id\":\"argv\",\"name\":\"argvsecret\",\"transport\":\"stdio\",\"command\":\"/usr/local/bin/x\",\"args\":[\"--token\",\"ghp_0123456789abcdef\"]}"
    );
    let registry = parse_registry(document(&entries).as_bytes()).expect("loads");

    // The rest of the registry is usable...
    assert!(resolve_for_agent(
        &registry,
        "claude",
        &[McpTransport::Stdio],
        &["ok".to_string()]
    )
    .is_ok());

    // ...but an agent that enabled the rejected entry refuses to spawn, with
    // the loader's own message, rather than starting silently short a server.
    let refusal = resolve_for_agent(
        &registry,
        "claude",
        &[McpTransport::Stdio],
        &["argv".to_string()],
    )
    .expect_err("a rejected entry must refuse the spawn");
    let SpawnRefusal::RejectedEntry { name, reason } = refusal else {
        panic!("expected a rejected-entry refusal, got {refusal:?}");
    };
    assert_eq!(name, "argvsecret");
    assert_eq!(
        Some(&reason),
        registry.by_id("argv").expect("present").rejection.as_ref(),
        "the spawn refusal must carry the same message the panel shows"
    );
}

#[test]
fn mcp_registry_refuses_an_http_entry_on_a_stdio_only_runtime() {
    let http = "{\"id\":\"h\",\"name\":\"linear\",\"transport\":\"http\",\"url\":\"https://mcp.linear.app/mcp\"}";
    let registry = parse_registry(document(http).as_bytes()).expect("loads");
    let refusal = resolve_for_agent(
        &registry,
        "buzz-agent",
        &[McpTransport::Stdio],
        &["h".to_string()],
    )
    .expect_err("buzz-agent deserializes only the stdio shape");
    assert!(
        matches!(refusal, SpawnRefusal::TransportUnsupported { .. }),
        "{refusal:?}"
    );
    assert!(resolve_for_agent(
        &registry,
        "claude",
        &[McpTransport::Stdio, McpTransport::Http],
        &["h".to_string()]
    )
    .is_ok());
}

#[test]
fn mcp_registry_caps_servers_per_agent_and_names_a_deleted_server() {
    let entries: Vec<String> = (0..=MAX_SERVERS_PER_AGENT)
        .map(|index| stdio(&format!("s{index}"), &format!("srv{index}")))
        .collect();
    let registry = parse_registry(document(&entries.join(",")).as_bytes()).expect("loads");
    let enabled: Vec<String> = (0..=MAX_SERVERS_PER_AGENT)
        .map(|index| format!("s{index}"))
        .collect();
    assert_eq!(
        resolve_for_agent(&registry, "claude", &[McpTransport::Stdio], &enabled)
            .expect_err("over the per-agent cap"),
        SpawnRefusal::TooManyServers {
            count: MAX_SERVERS_PER_AGENT + 1
        }
    );

    assert_eq!(
        resolve_for_agent(
            &registry,
            "claude",
            &[McpTransport::Stdio],
            &["gone".to_string()]
        )
        .expect_err("an enabled id that no longer exists"),
        SpawnRefusal::UnknownServer("gone".to_string())
    );
}

// ── Generation of the three artefacts ─────────────────────────────────────

#[test]
fn mcp_registry_generated_config_names_the_launcher_and_carries_no_value() {
    let entries = format!(
        "{},{}",
        "{\"id\":\"a\",\"name\":\"github\",\"transport\":\"stdio\",\"command\":\"/usr/local/bin/github-mcp\",\"args\":[\"--stdio\"],\"env\":{\"GITHUB_TOKEN\":\"mcp:github-token\",\"GITHUB_HOST\":\"github.example\"}}",
        "{\"id\":\"b\",\"name\":\"linear\",\"transport\":\"http\",\"url\":\"https://mcp.linear.app/mcp\",\"auth\":{\"scheme\":\"bearer\",\"secret\":\"mcp:linear-token\"}}"
    );
    let registry = parse_registry(document(&entries).as_bytes()).expect("loads");
    let resolved = resolve_for_agent(
        &registry,
        "claude",
        &[McpTransport::Stdio, McpTransport::Http],
        &["a".to_string(), "b".to_string()],
    )
    .expect("resolves");
    let generated: Vec<_> = resolved
        .servers
        .iter()
        .map(|entry| generate_server(LAUNCHER, SERVICE, entry))
        .collect();

    let acp = render_buzz_acp_registry(&generated).expect("renders");
    let claude = render_claude_project_config(&generated).expect("renders");
    let codex = render_codex_config(&generated).expect("renders");

    for (label, rendered) in [("buzz-acp", &acp), ("claude", &claude), ("codex", &codex)] {
        assert!(
            rendered.contains(LAUNCHER),
            "{label} must name the bundled launcher by absolute path"
        );
        assert!(
            !rendered.contains(&format!("{SERVER_BIN}/github-mcp\"\n")),
            "{label} must not name the server binary as the command"
        );
        assert!(
            rendered.contains("mcp:github-token"),
            "{label} must carry the reference"
        );
        assert!(
            rendered.contains("mcp:linear-token"),
            "{label} must carry the http credential reference"
        );
        // The http entry is reached through the proxy, never as a bare URL
        // Claude or Codex would fetch itself.
        assert!(
            rendered.contains("proxy"),
            "{label} must route the http upstream through the proxy"
        );
    }

    // The parsed shapes, not just the text.
    let claude: serde_json::Value = serde_json::from_str(&claude).expect("valid json");
    let github = &claude["mcpServers"]["github"];
    assert_eq!(github["command"], LAUNCHER);
    assert_eq!(github["env"]["GITHUB_TOKEN"], "mcp:github-token");
    assert_eq!(github["env"]["GITHUB_HOST"], "github.example");
    let codex: toml::Value = toml::from_str(&codex).expect("valid toml");
    assert_eq!(
        codex["mcp_servers"]["linear"]["command"].as_str(),
        Some(LAUNCHER)
    );
}

#[test]
fn mcp_registry_generated_env_block_and_launcher_flags_agree() {
    // The reference appears twice by design — the `env` block is what the
    // runtime hands the launcher process, the `--secret` flag is the channel
    // the launcher actually reads, because it builds its environment from
    // empty. Both come from one source, and this binds them.
    let entry = "{\"id\":\"a\",\"name\":\"github\",\"transport\":\"stdio\",\"command\":\"/usr/local/bin/x\",\"env\":{\"GITHUB_TOKEN\":\"mcp:github-token\",\"GITHUB_HOST\":\"github.example\"}}";
    let registry = parse_registry(document(entry).as_bytes()).expect("loads");
    let generated = generate_server(
        LAUNCHER,
        SERVICE,
        &registry.by_id("a").expect("present").entry,
    );

    let mut from_args = BTreeMap::new();
    let mut arguments = generated.args.iter();
    while let Some(argument) = arguments.next() {
        if argument == "--secret" || argument == "--set" {
            let pair = arguments.next().expect("a flag is followed by its pair");
            let (name, value) = pair.split_once('=').expect("a NAME=VALUE pair");
            from_args.insert(name.to_string(), value.to_string());
        }
    }
    assert_eq!(from_args, generated.env);
    assert!(generated.args.contains(&"--secret".to_string()));
    assert!(generated.args.contains(&"--set".to_string()));
}

// ── The staged generation and its journal ─────────────────────────────────

/// Fails at one named step, once.
struct FailAt {
    step: FlipStep,
    fired: AtomicUsize,
}

impl FailAt {
    fn new(step: FlipStep) -> Self {
        Self {
            step,
            fired: AtomicUsize::new(0),
        }
    }
}

impl FlipHooks for FailAt {
    fn after(&self, step: FlipStep) -> Result<(), String> {
        if step == self.step && self.fired.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err("injected".to_string());
        }
        Ok(())
    }
}

/// A secret store whose removals can be made to fail.
#[derive(Default)]
struct FakeSecrets {
    removed: Mutex<Vec<String>>,
    failing: Mutex<bool>,
    written: Mutex<BTreeMap<String, String>>,
    write_fails: Mutex<bool>,
}

impl SecretRemover for FakeSecrets {
    fn remove(&self, key: &str) -> Result<(), String> {
        if *self.failing.lock().unwrap_or_else(|e| e.into_inner()) {
            return Err("keychain unavailable".to_string());
        }
        self.removed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(key.to_string());
        Ok(())
    }

    fn write_all(&self, entries: &BTreeMap<String, String>) -> Result<(), String> {
        if *self.write_fails.lock().unwrap_or_else(|e| e.into_inner()) {
            return Err("keychain unavailable".to_string());
        }
        let mut written = self.written.lock().unwrap_or_else(|e| e.into_inner());
        for (key, value) in entries {
            written.insert(key.clone(), value.clone());
        }
        Ok(())
    }
}

fn plan(files: &[(&str, &str)], deletions: Vec<Deletion>) -> GenerationPlan {
    GenerationPlan {
        files: files
            .iter()
            .map(|(path, body)| (PathBuf::from(path), (*body).to_string()))
            .collect(),
        deletions,
        secrets: BTreeMap::new(),
    }
}

fn plan_with_secrets(files: &[(&str, &str)], secrets: &[(&str, &str)]) -> GenerationPlan {
    GenerationPlan {
        secrets: secrets
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect(),
        ..plan(files, vec![])
    }
}

/// A secret write is a durable mutation, so the journal that names it has to
/// be durable first.
///
/// Two injected failures bracket the write. Failing immediately **before** it
/// must leave the store untouched and a `PREPARED` journal already naming the
/// key as rollback debt; failing immediately **after** it must leave the key
/// written and the same journal, so the discard has something to remove. In
/// both cases the pointer never moved and reconcile removes what the journal
/// named. Writing the secrets from inside the plan builder — the order this
/// replaces — fails the first case, because the store already holds the key
/// when the journal is still absent.
#[test]
fn mcp_registry_secret_writes_are_journalled_before_they_happen() {
    for (step, written_before_the_crash) in [
        (FlipStep::JournalPrepared, false),
        (FlipStep::SecretsWritten, true),
    ] {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = GenerationStore::open(dir.path()).expect("opens");
        let secrets = FakeSecrets::default();

        let error = store
            .commit(
                |_, _| {
                    Ok(plan_with_secrets(
                        &[("a.json", "one")],
                        &[("mcp:a:1:tok", "v")],
                    ))
                },
                &secrets,
                &FailAt::new(step),
            )
            .expect_err("the injected failure must abort the commit");
        assert!(
            matches!(error, GenerationError::Injected { .. }),
            "{error:?}"
        );

        assert_eq!(
            secrets
                .written
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key("mcp:a:1:tok"),
            written_before_the_crash,
            "{step:?}: the secret write is on the wrong side of the journal"
        );
        assert_eq!(store.current().expect("pointer readable"), None, "{step:?}");

        let journal = store
            .journal()
            .expect("journal readable")
            .unwrap_or_else(|| panic!("{step:?}: no journal names the write"));
        assert_eq!(journal.phase, JournalPhase::Prepared, "{step:?}");
        assert_eq!(
            journal.rollback,
            vec![Deletion::Secret {
                key: "mcp:a:1:tok".to_string()
            }],
            "{step:?}: the journal does not name the secret it owes"
        );

        // The discard removes exactly what the journal named, and only then
        // clears it.
        let reconciled = store.reconcile(&secrets, &NoHooks).expect("reconciles");
        assert_eq!(reconciled, Reconciled::DiscardedStaging { generation: 1 });
        assert!(secrets
            .removed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&"mcp:a:1:tok".to_string()));
        assert_eq!(store.journal().expect("journal readable"), None);
    }
}

/// A rollback the keychain refuses is owed, not forgotten.
#[test]
fn mcp_registry_a_failed_rollback_keeps_the_prepared_journal() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = GenerationStore::open(dir.path()).expect("opens");
    let secrets = FakeSecrets::default();
    let _ = store.commit(
        |_, _| {
            Ok(plan_with_secrets(
                &[("a.json", "one")],
                &[("mcp:a:1:tok", "v")],
            ))
        },
        &secrets,
        &FailAt::new(FlipStep::SecretsWritten),
    );

    *secrets.failing.lock().unwrap_or_else(|e| e.into_inner()) = true;
    let error = store
        .reconcile(&secrets, &NoHooks)
        .expect_err("a refused rollback is an error");
    assert!(
        matches!(
            error,
            GenerationError::CleanupPending { outstanding: 1, .. }
        ),
        "{error:?}"
    );
    let journal = store
        .journal()
        .expect("journal readable")
        .expect("the debt is still recorded");
    assert_eq!(journal.phase, JournalPhase::Prepared);
    assert_eq!(journal.rollback.len(), 1);
}

/// The journal is replaced by a rename, never rewritten in place.
///
/// `File::create` truncates, so an in-place write puts the only record of an
/// owed deletion through a window in which it is neither entry. This drives
/// that window deterministically: a directory sitting on the staging name
/// makes the write fail, and the previous journal must survive it intact. An
/// in-place implementation writes straight onto `journal.json` and never
/// touches the staging name, so it reports `CleanupPending` instead — which is
/// what makes this test falsifiable.
#[test]
fn mcp_registry_a_failed_journal_write_leaves_the_previous_journal_intact() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = GenerationStore::open(dir.path()).expect("opens");
    let secrets = FakeSecrets::default();
    *secrets.failing.lock().unwrap_or_else(|e| e.into_inner()) = true;

    let owed = vec![Deletion::Secret {
        key: "mcp:a:1:tok".to_string(),
    }];
    let error = store
        .commit(
            |_, _| Ok(plan(&[("a.json", "one")], owed.clone())),
            &secrets,
            &NoHooks,
        )
        .expect_err("the refused deletion is owed");
    assert!(
        matches!(error, GenerationError::CleanupPending { .. }),
        "{error:?}"
    );
    let before = store
        .journal()
        .expect("journal readable")
        .expect("the debt is recorded");

    std::fs::create_dir(dir.path().join("journal.json.next")).expect("occupy the staging name");
    let error = store
        .reconcile(&secrets, &NoHooks)
        .expect_err("the journal write cannot succeed");
    assert!(
        matches!(error, GenerationError::Io { .. }),
        "the journal was written in place: {error:?}"
    );
    assert_eq!(
        store.journal().expect("journal still parses"),
        Some(before),
        "a torn journal write destroyed the deletion debt"
    );
}

fn read_current(store: &GenerationStore, relative: &str) -> Option<String> {
    let dir: PathBuf = store.current_dir().expect("pointer readable")?;
    std::fs::read_to_string(dir.join(relative)).ok()
}

#[test]
fn mcp_registry_generation_flip_is_atomic() {
    // Fail after every write and every journal transition, attempting a read
    // through the pointer at each point: the adopted generation is always one
    // whole generation, never a mixture.
    let steps = [
        FlipStep::JournalPrepared,
        FlipStep::FileWritten(0),
        FlipStep::FileWritten(1),
        FlipStep::PointerRenamed,
        FlipStep::JournalFlipped,
    ];
    for step in steps {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = GenerationStore::open(dir.path()).expect("opens");

        // Generation 1 lands cleanly and is what a spawn would read.
        store
            .commit(
                |_, _| Ok(plan(&[("a.json", "one-a"), ("b.json", "one-b")], vec![])),
                &NoSecrets,
                &NoHooks,
            )
            .expect("first generation commits");
        assert_eq!(read_current(&store, "a.json").as_deref(), Some("one-a"));

        let error = store
            .commit(
                |_, _| Ok(plan(&[("a.json", "two-a"), ("b.json", "two-b")], vec![])),
                &NoSecrets,
                &FailAt::new(step),
            )
            .expect_err("the injected failure must abort the commit");
        assert!(
            matches!(error, GenerationError::Injected { .. }),
            "{error:?}"
        );

        let expected = match step {
            // Before the rename: generation 1 is still adopted, whole.
            FlipStep::JournalPrepared | FlipStep::FileWritten(_) => ("one-a", "one-b"),
            // After it: generation 2 is adopted, whole. Never a mixture.
            _ => ("two-a", "two-b"),
        };
        assert_eq!(
            read_current(&store, "a.json").as_deref(),
            Some(expected.0),
            "{step:?}"
        );
        assert_eq!(
            read_current(&store, "b.json").as_deref(),
            Some(expected.1),
            "{step:?}"
        );

        // And whatever the crash left behind, the next start reconciles it.
        store
            .reconcile(&NoSecrets, &NoHooks)
            .expect("reconcile succeeds");
        assert!(
            store.journal().expect("journal readable").is_none(),
            "the journal is cleared only once nothing is owed"
        );
        // Re-read through the pointer against the *same* expectation. Without
        // this, a reconcile that deleted the adopted generation — the state a
        // crash between the pointer rename and the FLIPPED journal write
        // leaves — would still leave this test green.
        assert_eq!(
            read_current(&store, "a.json").as_deref(),
            Some(expected.0),
            "reconcile changed the adopted configuration after {step:?}"
        );
        assert_eq!(
            read_current(&store, "b.json").as_deref(),
            Some(expected.1),
            "reconcile changed the adopted configuration after {step:?}"
        );
    }
}

/// A `current` pointer that does not name a generation must block a commit,
/// not read as "no generation yet".
///
/// Read as `None` the next commit would number itself 1 and stage into the
/// live generation 1 directory, adopting a mixture of two changes over a tree
/// that was never empty. Making `current` return `Ok(None)` on a parse failure
/// fails this test at the first assertion.
#[test]
fn mcp_registry_a_corrupt_pointer_blocks_the_commit_and_changes_nothing() {
    // Each case names the fragment its error must carry, so the read stays
    // bounded: without the `take`, an oversized pointer would be read whole
    // and reported as a parse failure instead.
    for (corruption, expected) in [
        (b"not-a-number".to_vec(), "is not a generation number"),
        (Vec::new(), "is not a generation number"),
        (vec![0xff, 0xfe], "not UTF-8"),
        (vec![b'1'; MAX_POINTER_BYTES + 1], "longer than 64 bytes"),
    ] {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = GenerationStore::open(dir.path()).expect("opens");
        store
            .commit(
                |_, _| Ok(plan(&[("a.json", "one")], vec![])),
                &NoSecrets,
                &NoHooks,
            )
            .expect("generation 1 lands");
        let generation_one = std::fs::read_to_string(store.generation_dir(1).join("a.json"))
            .expect("generation 1 is on disk");

        std::fs::write(dir.path().join("current"), &corruption).expect("corrupt the pointer");

        let error = store
            .commit(
                |_, _| Ok(plan(&[("a.json", "two")], vec![])),
                &NoSecrets,
                &NoHooks,
            )
            .expect_err("a commit must not run from a pointer it cannot read");
        let GenerationError::Pointer(reason) = &error else {
            panic!("{corruption:?}: {error:?}");
        };
        assert!(
            reason.contains(expected),
            "{corruption:?}: expected {expected:?} in {reason:?}"
        );

        assert_eq!(
            std::fs::read_to_string(store.generation_dir(1).join("a.json")).ok(),
            Some(generation_one),
            "{corruption:?}: the refused commit staged over the live generation"
        );
        assert!(
            !store.generation_dir(2).exists(),
            "{corruption:?}: the refused commit staged a new generation"
        );
        assert!(
            store.journal().expect("journal readable").is_none(),
            "{corruption:?}: the refused commit left a journal entry"
        );
        assert!(
            matches!(store.current(), Err(GenerationError::Pointer(_))),
            "{corruption:?}: the pointer was rewritten"
        );
    }
}

/// A staging tree that cannot be removed must fail the reconcile that tries,
/// and must keep its journal entry until the directory is actually gone.
///
/// The removal is the only thing standing between a stale generation directory
/// and the next commit, which recomputes the same generation number: a
/// discarded error there ships obsolete generated files to agents. Deleting the
/// `?` on either `remove_tree` call fails this test.
#[cfg(unix)]
#[test]
fn mcp_registry_a_failed_discard_is_surfaced_and_keeps_the_journal() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("temp dir");
    let store = GenerationStore::open(dir.path()).expect("opens");
    store
        .commit(
            |_, _| Ok(plan(&[("a.json", "one")], vec![])),
            &NoSecrets,
            &NoHooks,
        )
        .expect("generation 1 lands");

    // Generation 2 staged and never adopted: the pointer still names 1, so a
    // reconcile discards the half-staged tree.
    let staged = store.generation_dir(2);
    assert!(store
        .commit(
            |_, _| Ok(plan(&[("a.json", "two")], vec![])),
            &NoSecrets,
            &FailAt::new(FlipStep::FileWritten(0)),
        )
        .is_err());
    assert!(staged.is_dir(), "generation 2 was staged");

    // Take write permission off the parent, so removing the staged directory
    // itself fails after its contents are gone — the half-removed tree.
    let generations = staged.parent().expect("a parent").to_path_buf();
    let original = std::fs::metadata(&generations)
        .expect("readable")
        .permissions();
    std::fs::set_permissions(&generations, std::fs::Permissions::from_mode(0o555))
        .expect("drop write permission");
    // Root ignores the mode, so this machine cannot run the assertion.
    let writable_anyway = std::fs::File::create(generations.join("probe")).is_ok();
    if writable_anyway {
        let _ = std::fs::remove_file(generations.join("probe"));
        std::fs::set_permissions(&generations, original).expect("restore");
        return;
    }

    let error = store
        .reconcile(&NoSecrets, &NoHooks)
        .expect_err("a failed discard must be surfaced, not swallowed");
    assert!(
        matches!(
            error,
            GenerationError::Io {
                operation: "remove",
                ..
            }
        ),
        "{error:?}"
    );
    let journal = store.journal().expect("readable").expect("still recorded");
    assert_eq!(journal.generation, 2);
    assert_eq!(journal.phase, JournalPhase::Prepared);
    // A mutation attempted while the discard is owed is refused too, because
    // commit finishes the outstanding journal before it stages anything.
    assert!(store
        .commit(
            |_, _| Ok(plan(&[("a.json", "three")], vec![])),
            &NoSecrets,
            &NoHooks,
        )
        .is_err());

    // Once the directory can be removed, the discard completes and the journal
    // is cleared — and generation 1 was the adopted one throughout.
    std::fs::set_permissions(&generations, original).expect("restore");
    assert!(matches!(
        store
            .reconcile(&NoSecrets, &NoHooks)
            .expect("the discard completes"),
        Reconciled::DiscardedStaging { generation: 2 }
    ));
    assert!(store.journal().expect("readable").is_none());
    assert_eq!(read_current(&store, "a.json").as_deref(), Some("one"));
}

#[test]
fn mcp_registry_commit_refuses_while_a_deletion_is_owed() {
    // A keychain delete that failed is a credential the operator has already
    // been told is revoked. The next Settings change must not write its own
    // journal over that debt: it retries it first, and refuses if it still
    // cannot be paid.
    let dir = tempfile::tempdir().expect("temp dir");
    let store = GenerationStore::open(dir.path()).expect("opens");
    let secrets = FakeSecrets::default();
    *secrets.failing.lock().expect("lock") = true;

    let owed = Deletion::Secret {
        key: "mcp:agent-a:1:token".to_string(),
    };
    assert!(store
        .commit(
            |_, _| Ok(plan(&[("a.json", "one")], vec![owed.clone()])),
            &secrets,
            &NoHooks,
        )
        .is_err());

    // A second change while the debt stands: refused with the pending reason,
    // and the journal still holds exactly what is owed.
    let error = store
        .commit(
            |_, _| Ok(plan(&[("a.json", "two")], vec![])),
            &secrets,
            &NoHooks,
        )
        .expect_err("a mutation must not be applied over an owed deletion");
    assert!(
        matches!(
            error,
            GenerationError::CleanupPending { outstanding: 1, .. }
        ),
        "{error:?}"
    );
    let journal = store.journal().expect("readable").expect("still owed");
    assert_eq!(journal.deletions, vec![owed.clone()]);
    assert_eq!(
        read_current(&store, "a.json").as_deref(),
        Some("one"),
        "the refused mutation must not have been adopted"
    );

    // Restart with a reachable store: the debt is paid, and only then does a
    // new change go through.
    *secrets.failing.lock().expect("lock") = false;
    store
        .commit(
            |_, _| Ok(plan(&[("a.json", "two")], vec![])),
            &secrets,
            &NoHooks,
        )
        .expect("the change lands once the debt is paid");
    assert_eq!(read_current(&store, "a.json").as_deref(), Some("two"));
    assert!(store.journal().expect("readable").is_none());
    assert_eq!(
        secrets.removed.lock().expect("lock").as_slice(),
        ["mcp:agent-a:1:token"],
        "the owed keychain delete ran exactly once"
    );
}

#[test]
fn mcp_registry_post_flip_cleanup_resumes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = GenerationStore::open(dir.path()).expect("opens");
    let secrets = FakeSecrets::default();
    *secrets.failing.lock().expect("lock") = true;

    let error = store
        .commit(
            |_, _| {
                Ok(plan(
                    &[("a.json", "one")],
                    vec![Deletion::Secret {
                        key: "mcp:agent-a:1:token".to_string(),
                    }],
                ))
            },
            &secrets,
            &NoHooks,
        )
        .expect_err("a failed keychain delete must be reported, not swallowed");
    assert!(
        matches!(
            error,
            GenerationError::CleanupPending { outstanding: 1, .. }
        ),
        "{error:?}"
    );

    // The flip stands, and the journal keeps exactly what is still owed.
    assert_eq!(read_current(&store, "a.json").as_deref(), Some("one"));
    let journal = store.journal().expect("readable").expect("outstanding");
    assert_eq!(journal.phase, JournalPhase::Flipped);
    assert_eq!(journal.deletions.len(), 1);

    // Still failing on the next start: still owed, still recorded.
    assert!(store.reconcile(&secrets, &NoHooks).is_err());
    assert!(store.journal().expect("readable").is_some());

    *secrets.failing.lock().expect("lock") = false;
    let outcome = store
        .reconcile(&secrets, &NoHooks)
        .expect("the retry succeeds once the store is reachable");
    assert_eq!(
        outcome,
        Reconciled::CompletedCleanup {
            generation: 1,
            deletions: 1
        }
    );
    assert!(
        store.journal().expect("readable").is_none(),
        "the journal is removed only at CLEANED"
    );
    assert_eq!(
        secrets.removed.lock().expect("lock").as_slice(),
        ["mcp:agent-a:1:token"]
    );
}

#[test]
fn mcp_registry_two_writers_serialize() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().to_path_buf();
    let store = GenerationStore::open(&root).expect("opens");
    store
        .commit(
            |_, _| Ok(plan(&[("log.txt", "base")], vec![])),
            &NoSecrets,
            &NoHooks,
        )
        .expect("base generation");

    // Both writers append to whatever the base generation holds. If either one
    // staged from a stale base, one append would be lost.
    let append = |root: PathBuf, mark: &'static str| {
        std::thread::spawn(move || {
            let store = GenerationStore::open(&root).expect("opens");
            store.commit(
                move |_, base_dir: Option<&Path>| {
                    let previous = base_dir
                        .and_then(|dir| std::fs::read_to_string(dir.join("log.txt")).ok())
                        .unwrap_or_default();
                    Ok(plan(&[("log.txt", &format!("{previous}+{mark}"))], vec![]))
                },
                &NoSecrets,
                &NoHooks,
            )
        })
    };
    let first = append(root.clone(), "one");
    let second = append(root.clone(), "two");
    let generations = [
        first.join().expect("thread").expect("commits"),
        second.join().expect("thread").expect("commits"),
    ];
    assert_eq!(
        {
            let mut sorted = generations;
            sorted.sort_unstable();
            sorted
        },
        [2, 3],
        "the loser must restage against the new base, not reuse generation 2"
    );

    let final_log = read_current(&store, "log.txt").expect("adopted");
    assert!(
        final_log.contains("+one") && final_log.contains("+two"),
        "neither action may be silently discarded: {final_log}"
    );
}

#[test]
fn mcp_registry_staging_tree_keeps_at_most_two_generations() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = GenerationStore::open(dir.path()).expect("opens");
    for index in 0..5 {
        store
            .commit(
                move |_, _| Ok(plan(&[("a.json", &format!("gen{index}"))], vec![])),
                &NoSecrets,
                &NoHooks,
            )
            .expect("commits");
    }
    let kept = std::fs::read_dir(dir.path().join("generations"))
        .expect("readable")
        .count();
    assert_eq!(
        kept, 2,
        "retention is the current generation plus one rollback"
    );
    assert_eq!(read_current(&store, "a.json").as_deref(), Some("gen4"));
}
