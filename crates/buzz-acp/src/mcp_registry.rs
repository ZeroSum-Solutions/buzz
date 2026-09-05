//! The `BUZZ_ACP_MCP_REGISTRY` handover file (memo decision 10).
//!
//! `BUZZ_ACP_EXTRA_MCP_COMMANDS` can only carry a server's credentials in its
//! argv, where `ps` and any crash dump can read them, so the registry hands
//! servers over as a file instead: a bounded JSON document naming each
//! server's command, args and `env` block. The values in that block are
//! `mcp:` references, not secrets — `buzz-mcp-launch`, which is the command on
//! every generated entry, is the side that resolves them — so no resolved
//! value ever enters this process's address space or the ACP wire.
//!
//! This module only reads and bounds the document. It resolves nothing, and it
//! is deliberately separate from the extras parser: both produce `McpServer`
//! entries and both are appended by [`crate::build_mcp_servers`], so an
//! operator can keep using `BUZZ_ACP_EXTRA_MCP_COMMANDS` while the desktop
//! writes this file.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use serde::Deserialize;

use crate::acp::{EnvVar, McpServer};
use crate::{ConfigError, MAX_MCP_NAME_LEN, MAX_MCP_SERVERS};

/// Variable naming the registry file the desktop generated for this agent.
pub const MCP_REGISTRY_ENV_VAR: &str = "BUZZ_ACP_MCP_REGISTRY";

/// Schema version this build understands. A document declaring anything else
/// is refused rather than read on a guess.
pub const REGISTRY_FILE_VERSION: u32 = 1;

/// Largest accepted registry file, in bytes.
///
/// The desktop caps the operator-editable registry document at 256 KiB and
/// generates from it; the generated form is larger per entry, so this is the
/// generated bound, not a copy of the source one.
pub const MAX_REGISTRY_FILE_BYTES: usize = 1024 * 1024;

/// Largest accepted number of `env` entries on one server.
pub const MAX_REGISTRY_ENV_ENTRIES: usize = 32;

/// Largest accepted `env` variable name, in bytes.
pub const MAX_REGISTRY_ENV_NAME_LEN: usize = 128;

/// Largest accepted `env` value, in bytes.
pub const MAX_REGISTRY_ENV_VALUE_LEN: usize = 4 * 1024;

/// Largest accepted number of arguments on one server.
pub const MAX_REGISTRY_ARGS: usize = 64;

/// Largest accepted single argument, in bytes.
pub const MAX_REGISTRY_ARG_LEN: usize = 1024;

/// Largest accepted command, in bytes.
pub const MAX_REGISTRY_COMMAND_LEN: usize = 4 * 1024;

/// The document as the desktop writes it.
#[derive(Debug, Deserialize)]
struct RegistryFile {
    version: u32,
    #[serde(default)]
    servers: Vec<RegistryFileServer>,
}

/// One generated server.
#[derive(Debug, Deserialize)]
struct RegistryFileServer {
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: Vec<RegistryFileEnv>,
}

/// One declared environment entry. `value` is a reference, not a secret.
#[derive(Debug, Deserialize)]
struct RegistryFileEnv {
    name: String,
    value: String,
}

/// Read the registry file at `path`, bounded at [`MAX_REGISTRY_FILE_BYTES`].
///
/// The read is bounded before any UTF-8 decoding or parsing, so a file grown
/// past the cap — by a bug in the writer or by anything else that can write
/// the app data directory — is refused rather than allocated.
///
/// # Errors
/// A message naming the path when the file cannot be opened or read, or when
/// it is over the cap.
pub fn read_registry_file(path: &Path) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|e| {
        format!(
            "{MCP_REGISTRY_ENV_VAR} names {}, which cannot be opened: {e}",
            path.display()
        )
    })?;
    // One byte past the cap, so a file that exactly fills it is accepted and
    // one byte more is reported: the exhausted limit is what detects the
    // overflow, which widening the bound cannot leave unreported.
    let mut bounded = file.take(MAX_REGISTRY_FILE_BYTES as u64 + 1);
    let mut bytes = Vec::new();
    bounded.read_to_end(&mut bytes).map_err(|e| {
        format!(
            "{MCP_REGISTRY_ENV_VAR} names {}, which cannot be read: {e}",
            path.display()
        )
    })?;
    if bounded.limit() == 0 {
        return Err(format!(
            "{MCP_REGISTRY_ENV_VAR} names {}, which is larger than the {MAX_REGISTRY_FILE_BYTES}-byte cap",
            path.display()
        ));
    }
    Ok(bytes)
}

/// Parse `bytes` into MCP servers, refusing anything past a bound.
///
/// `max_servers` is the caller's remaining budget: the whole `session/new`
/// array is capped, and the primary server plus any
/// `BUZZ_ACP_EXTRA_MCP_COMMANDS` entries have already taken their share.
///
/// Every server is marked untrusted. These are third-party processes reached
/// through a launcher, so they receive no Buzz identity variable and never run
/// a hook, exactly like an extras entry.
///
/// # Errors
/// A message naming the failing entry by index — never by content, since an
/// argument or an `env` value may be operator-supplied — when the document
/// does not parse, declares an unsupported version, holds more servers than
/// `max_servers`, repeats a name, or breaches a per-entry bound.
pub fn parse_registry_file(bytes: &[u8], max_servers: usize) -> Result<Vec<McpServer>, String> {
    let document: RegistryFile = serde_json::from_slice(bytes)
        .map_err(|e| format!("the mcp registry file is not a valid registry document: {e}"))?;
    if document.version != REGISTRY_FILE_VERSION {
        return Err(format!(
            "the mcp registry file declares version {}, and this build reads version {REGISTRY_FILE_VERSION}",
            document.version
        ));
    }
    if document.servers.len() > max_servers {
        return Err(format!(
            "the mcp registry file declares {} servers, and only {max_servers} more fit in one \
             session (buzz-agent rejects the whole array past the cap, so every session would fail)",
            document.servers.len()
        ));
    }

    let mut servers: Vec<McpServer> = Vec::with_capacity(document.servers.len());
    for (index, server) in document.servers.iter().enumerate() {
        let entry = index + 1;
        let bound = |what: &str, actual: usize, cap: usize| {
            format!("mcp registry entry {entry} has {actual} {what}, over the cap of {cap}")
        };
        if server.name.is_empty() || server.name.len() > MAX_MCP_NAME_LEN {
            return Err(format!(
                "mcp registry entry {entry} has a name of {} bytes; a name must be 1 to \
                 {MAX_MCP_NAME_LEN} bytes",
                server.name.len()
            ));
        }
        if !server
            .name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(format!(
                "mcp registry entry {entry} has a name outside `[A-Za-z0-9-]`, which the \
                 downstream tool-name validator refuses"
            ));
        }
        if server.command.is_empty() || server.command.len() > MAX_REGISTRY_COMMAND_LEN {
            return Err(format!(
                "mcp registry entry {entry} has a command of {} bytes; a command must be 1 to \
                 {MAX_REGISTRY_COMMAND_LEN} bytes",
                server.command.len()
            ));
        }
        if server.args.len() > MAX_REGISTRY_ARGS {
            return Err(bound("arguments", server.args.len(), MAX_REGISTRY_ARGS));
        }
        if let Some(long) = server
            .args
            .iter()
            .find(|arg| arg.len() > MAX_REGISTRY_ARG_LEN)
        {
            return Err(bound(
                "bytes in one argument",
                long.len(),
                MAX_REGISTRY_ARG_LEN,
            ));
        }
        if server.env.len() > MAX_REGISTRY_ENV_ENTRIES {
            return Err(bound(
                "environment entries",
                server.env.len(),
                MAX_REGISTRY_ENV_ENTRIES,
            ));
        }
        for variable in &server.env {
            if variable.name.is_empty()
                || variable.name.len() > MAX_REGISTRY_ENV_NAME_LEN
                || variable.name.contains('=')
                || variable.name.contains('\0')
            {
                return Err(format!(
                    "mcp registry entry {entry} declares an environment variable whose name is \
                     empty, over {MAX_REGISTRY_ENV_NAME_LEN} bytes, or holds `=` or a NUL"
                ));
            }
            if variable.value.len() > MAX_REGISTRY_ENV_VALUE_LEN {
                return Err(bound(
                    "bytes in one environment value",
                    variable.value.len(),
                    MAX_REGISTRY_ENV_VALUE_LEN,
                ));
            }
        }
        if let Some(first) = servers.iter().position(|s| s.name == server.name) {
            return Err(format!(
                "mcp registry entries {} and {entry} declare the same server name; both native \
                 config formats key servers by name, so one entry would silently keep the other's \
                 command",
                first + 1
            ));
        }
        servers.push(McpServer {
            name: server.name.clone(),
            command: server.command.clone(),
            args: server.args.clone(),
            env: server
                .env
                .iter()
                .map(|variable| EnvVar {
                    name: variable.name.clone(),
                    value: variable.value.clone(),
                })
                .collect(),
            trusted: false,
            // The desktop wrote this file, and every entry in it names the
            // bundled launcher. The marker is what makes `buzz-agent` hand the
            // child `BUZZ_MCP_CAPABILITY`; without it every credential-backed
            // registry server exits 1 before it runs.
            registry_launched: true,
        });
    }
    Ok(servers)
}

/// Append the desktop's generated registry servers to `servers`.
///
/// Nothing happens when [`MCP_REGISTRY_ENV_VAR`] is unset or empty, so an
/// operator running only `BUZZ_ACP_EXTRA_MCP_COMMANDS` is unaffected.
///
/// # Errors
/// [`ConfigError::ConfigFile`] when the file cannot be read or does not
/// satisfy a bound (see [`read_registry_file`] and [`parse_registry_file`]),
/// when the agent command is not the one adapter that honours the `trusted`
/// marker, or when a generated name collides with a server already built. Every
/// one fails startup rather than the first session: a harness that started here
/// would announce itself online and then fail every `session/new`.
pub fn append_registry_mcp_servers(
    config: &crate::config::Config,
    servers: &mut Vec<McpServer>,
) -> Result<(), ConfigError> {
    let Some(path) = config
        .mcp_registry
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    else {
        return Ok(());
    };

    // Same boundary the extras path draws, for the same reason: these servers
    // are declared untrusted, and only `buzz-agent` acts on that marker.
    // Every other adapter spawns them itself out of a process holding
    // BUZZ_PRIVATE_KEY, so refuse at startup rather than leak on the first
    // session.
    let adapter = crate::config::normalize_agent_command_identity(&config.agent_command);
    if adapter != crate::EXTRA_MCP_ADAPTER {
        return Err(ConfigError::ConfigFile(format!(
            "{MCP_REGISTRY_ENV_VAR} is set, but the agent command is `{adapter}`. Only \
             `{}` honours the `trusted` marker that withholds BUZZ_PRIVATE_KEY, \
             NOSTR_PRIVATE_KEY, BUZZ_RELAY_URL and BUZZ_AUTH_TAG from a registry MCP server; \
             every other ACP adapter spawns the declared servers itself, from a process that \
             inherited this one's environment.",
            crate::EXTRA_MCP_ADAPTER
        )));
    }

    let budget = MAX_MCP_SERVERS.saturating_sub(servers.len());
    let bytes = read_registry_file(Path::new(path)).map_err(ConfigError::ConfigFile)?;
    let generated = parse_registry_file(&bytes, budget).map_err(ConfigError::ConfigFile)?;

    for server in generated {
        if servers.iter().any(|existing| existing.name == server.name) {
            return Err(ConfigError::ConfigFile(format!(
                "mcp registry server `{}` has the same name as a server this harness already \
                 declares; rename it in the registry and restart",
                server.name
            )));
        }
        servers.push(server);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::{wire_log_line, REDACTED_ENV_VALUE};

    /// A document holding one server with the `env` block a registry entry
    /// carries: reference values, never secrets.
    fn document(servers: &str) -> String {
        format!("{{\"version\":1,\"servers\":[{servers}]}}")
    }

    fn stdio(name: &str) -> String {
        format!(
            "{{\"name\":\"{name}\",\"command\":\"/opt/buzz/bin/buzz-mcp-launch\",\
             \"args\":[\"launch\",\"--server\",\"{name}\",\"--\",\"/usr/local/bin/{name}\"],\
             \"env\":[{{\"name\":\"TOKEN\",\"value\":\"mcp:{name}-token\"}}]}}"
        )
    }

    #[test]
    fn registry_file_becomes_untrusted_servers_with_their_env_block() {
        let servers = parse_registry_file(document(&stdio("github")).as_bytes(), MAX_MCP_SERVERS)
            .expect("a well-formed document loads");
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "github");
        assert_eq!(servers[0].command, "/opt/buzz/bin/buzz-mcp-launch");
        // The `env` block is the whole point of the file: the extras path
        // hardcodes an empty one, so a credential could only travel in argv.
        assert_eq!(servers[0].env.len(), 1);
        assert_eq!(servers[0].env[0].name, "TOKEN");
        assert_eq!(servers[0].env[0].value, "mcp:github-token");
        // Untrusted: no Buzz identity variable, no hooks.
        assert!(!servers[0].trusted);
        // Registry-launched: this entry runs the bundled launcher, so
        // buzz-agent forwards BUZZ_MCP_CAPABILITY to it.
        assert!(servers[0].registry_launched);
    }

    /// The marker crosses the wire under the name `buzz-agent` deserializes,
    /// and only a registry entry carries it.
    ///
    /// The launcher reads the capability from its own inherited environment
    /// and exits 1 without it, so an entry that reached `session/new` without
    /// this field would start no server at all; and a marker on an extra
    /// command would hand an operator's own process a bearer token for every
    /// reference the agent can resolve. Both halves are asserted on the
    /// serialized JSON, which is what the two crates actually agree on.
    #[test]
    fn registry_file_marks_only_its_own_servers_registry_launched() {
        let mut servers = vec![McpServer {
            name: "extra".to_string(),
            command: "/opt/extra".to_string(),
            args: Vec::new(),
            env: Vec::new(),
            trusted: false,
            registry_launched: false,
        }];
        let generated = parse_registry_file(document(&stdio("github")).as_bytes(), MAX_MCP_SERVERS)
            .expect("a well-formed document loads");
        servers.extend(generated);

        let wire = serde_json::to_value(&servers).expect("serializes");
        assert_eq!(
            wire[0].get("registry_launched"),
            None,
            "an operator's own server must not be marked: {wire}"
        );
        assert_eq!(
            wire[1]["registry_launched"],
            serde_json::json!(true),
            "the registry entry lost its marker: {wire}"
        );
    }

    #[test]
    fn registry_file_bounds_are_enforced_per_entry() {
        let over_version = "{\"version\":2,\"servers\":[]}";
        assert!(
            parse_registry_file(over_version.as_bytes(), MAX_MCP_SERVERS)
                .expect_err("an unknown version is refused")
                .contains("version 2")
        );

        let long_name = "a".repeat(MAX_MCP_NAME_LEN + 1);
        let doc = document(&format!(
            "{{\"name\":\"{long_name}\",\"command\":\"/x\",\"args\":[],\"env\":[]}}"
        ));
        assert!(parse_registry_file(doc.as_bytes(), MAX_MCP_SERVERS)
            .expect_err("an over-long name is refused")
            .contains("bytes"));

        let bad_name = document("{\"name\":\"a/b\",\"command\":\"/x\",\"args\":[],\"env\":[]}");
        assert!(parse_registry_file(bad_name.as_bytes(), MAX_MCP_SERVERS).is_err());

        let args: Vec<String> = (0..=MAX_REGISTRY_ARGS)
            .map(|i| format!("\"{i}\""))
            .collect();
        let many_args = document(&format!(
            "{{\"name\":\"a\",\"command\":\"/x\",\"args\":[{}],\"env\":[]}}",
            args.join(",")
        ));
        assert!(parse_registry_file(many_args.as_bytes(), MAX_MCP_SERVERS)
            .expect_err("too many arguments is refused")
            .contains("over the cap"));

        let long_value = "v".repeat(MAX_REGISTRY_ENV_VALUE_LEN + 1);
        let big_env = document(&format!(
            "{{\"name\":\"a\",\"command\":\"/x\",\"args\":[],\
             \"env\":[{{\"name\":\"T\",\"value\":\"{long_value}\"}}]}}"
        ));
        assert!(parse_registry_file(big_env.as_bytes(), MAX_MCP_SERVERS)
            .expect_err("an over-long env value is refused")
            .contains("over the cap"));

        // The budget is the caller's remaining share of one session's array,
        // not the file's own size: the primary server and any extras have
        // already taken theirs.
        let two = document(&format!("{},{}", stdio("a"), stdio("b")));
        assert!(parse_registry_file(two.as_bytes(), 1)
            .expect_err("more servers than the remaining budget is refused")
            .contains("only 1 more"));
        assert!(parse_registry_file(two.as_bytes(), 2).is_ok());
    }

    #[test]
    fn registry_file_refuses_two_entries_with_one_name() {
        let doc = document(&format!("{},{}", stdio("github"), stdio("github")));
        assert!(parse_registry_file(doc.as_bytes(), MAX_MCP_SERVERS)
            .expect_err("a repeated name is refused")
            .contains("same server name"));
    }

    #[test]
    fn registry_file_read_is_bounded_and_missing_files_are_surfaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("registry.json");

        let error = read_registry_file(&path).expect_err("a missing file is an error");
        assert!(error.contains(MCP_REGISTRY_ENV_VAR), "{error}");

        std::fs::write(&path, vec![b'x'; MAX_REGISTRY_FILE_BYTES + 1]).expect("write");
        let error = read_registry_file(&path).expect_err("an oversized file is refused");
        assert!(error.contains("cap"), "{error}");

        std::fs::write(&path, document(&stdio("github"))).expect("write");
        let bytes = read_registry_file(&path).expect("a file under the cap reads");
        assert_eq!(
            parse_registry_file(&bytes, MAX_MCP_SERVERS)
                .expect("parses")
                .len(),
            1
        );
    }

    /// The redaction is only worth anything if the log site uses it.
    ///
    /// `wire_log_redacts_env_values` drives the serializer, which a revert of
    /// the log site back to `serde_json::to_string(&msg)` would leave green —
    /// a guard whose removal fails no test. This reads `acp.rs` and asserts
    /// every outbound `acp::wire` line goes through `wire_log_line`, so that
    /// revert fails here.
    #[test]
    fn every_outbound_wire_log_serializes_through_the_redacting_writer() {
        let source = include_str!("acp.rs");
        let outbound: Vec<&str> = source
            .lines()
            .map(str::trim)
            .filter(|line| line.contains("target: \"acp::wire\"") && line.contains("\"\u{2192}"))
            .collect();
        assert!(
            outbound.len() >= 3,
            "expected every outbound wire log site; found {}",
            outbound.len()
        );
        for line in outbound {
            assert!(
                line.contains("wire_log_line(&msg)"),
                "an outbound acp::wire log does not redact declared env values: {line}"
            );
        }
    }

    /// Memo decision 10's logging clause, at the seam `acp::wire` uses.
    ///
    /// `send_request` logs the whole serialized `session/new`, and `McpServer.env`
    /// is not skipped by its `Serialize`. The redaction is structural, so the
    /// guarantee does not depend on the log level staying below `debug`.
    /// Restoring `serde_json::to_string(&msg)` at the log site fails this.
    #[test]
    fn wire_log_redacts_env_values() {
        let servers = parse_registry_file(document(&stdio("github")).as_bytes(), MAX_MCP_SERVERS)
            .expect("loads");
        // The exact `params` shape `session_new_full` builds.
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/new",
            "params": { "cwd": "/tmp", "mcpServers": servers },
        });

        let logged = wire_log_line(&message);
        assert!(
            !logged.contains("mcp:github-token"),
            "the wire log carried a declared env value: {logged}"
        );
        assert!(logged.contains(REDACTED_ENV_VALUE), "{logged}");
        // Redaction, not blanking: the log still names the server, the
        // launcher and the variable, which is what makes it worth keeping.
        assert!(logged.contains("github"), "{logged}");
        assert!(logged.contains("buzz-mcp-launch"), "{logged}");
        assert!(logged.contains("TOKEN"), "{logged}");
        // A message with no mcpServers is passed through unchanged.
        let plain = serde_json::json!({"jsonrpc": "2.0", "method": "x", "params": {"a": 1}});
        assert_eq!(
            wire_log_line(&plain),
            serde_json::to_string(&plain).expect("serializes")
        );
    }
}
