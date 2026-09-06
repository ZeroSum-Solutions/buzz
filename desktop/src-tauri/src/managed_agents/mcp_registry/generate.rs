//! Generating the three configuration artefacts (memo decisions 3, 9 and 10).
//!
//! Every generated stdio entry names the bundled `buzz-mcp-launch` by resolved
//! absolute path, never a bare name — a bare name would resolve through a
//! `PATH` the launcher's own clear removes. Every HTTP entry becomes a stdio
//! entry pointing at the same binary in proxy mode, because no launcher can
//! inject an environment into a remote server.
//!
//! No generated file holds a secret **value**, and no generated entry holds an
//! `env` block at all. A declared variable travels in one channel only: the
//! launcher's own `--set` and `--secret` argv, which the launcher applies to
//! the child it starts after it strips its own capability. Duplicating it into
//! a wire `env` block would put it on the **launcher's** process environment
//! instead — `buzz-agent` applies `McpServer.env` to the launcher's `Command`
//! — where a loader variable (`DYLD_INSERT_LIBRARIES`, `LD_PRELOAD`) would run
//! attacker code inside the launcher before `main`, while the capability that
//! authorizes every one of that agent's secrets is in its environment. The
//! launcher process holds platform essentials and the capability, nothing
//! else. `generated_entries_carry_no_env_block` binds that.

use buzz_secret_store_pkg::looks_like_reference;

use super::schema::{RegistryEntry, RegistryTransport};

/// One generated server: the launcher command line, and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedServer {
    /// The registry name, which becomes the config key.
    pub name: String,
    /// Absolute path of the bundled launcher.
    pub command: String,
    /// Launcher arguments, references included.
    ///
    /// This is the only channel a declared environment variable travels in:
    /// the launcher reads `--set`/`--secret` and applies them to its child.
    pub args: Vec<String>,
}

/// Build the launcher invocation for one registry entry.
///
/// `keychain_service` is the service name the **desktop** stores its secret
/// blob under, and it is written into every generated argv. The launcher's own
/// default is the release name `buzz-desktop` (`buzz-mcp-launch/src/cli.rs`),
/// while the desktop uses `buzz-desktop-dev` in debug builds and a per-slug
/// name on demo builds — so on every build but release-non-demo a generated
/// server that omitted the flag would start and then fail to resolve a single
/// reference.
pub fn generate_server(
    launcher: &str,
    keychain_service: &str,
    entry: &RegistryEntry,
) -> GeneratedServer {
    let mut args = vec!["--service".to_string(), keychain_service.to_string()];

    match &entry.transport {
        RegistryTransport::Stdio {
            command,
            args: tail,
        } => {
            args.push("launch".to_string());
            args.push("--server".to_string());
            args.push(entry.name.clone());
            for (name, value) in &entry.env {
                if looks_like_reference(value) {
                    args.push("--secret".to_string());
                } else {
                    args.push("--set".to_string());
                }
                args.push(format!("{name}={value}"));
            }
            args.push("--".to_string());
            args.push(command.clone());
            args.extend(tail.iter().cloned());
        }
        RegistryTransport::Http { url, auth } => {
            args.push("proxy".to_string());
            args.push("--url".to_string());
            args.push(url.clone());
            if let Some(auth) = auth {
                args.push("--auth-scheme".to_string());
                args.push(auth.scheme.clone());
                args.push("--secret".to_string());
                args.push(auth.secret.clone());
            }
            // An http upstream declares no variables: there is no child
            // process to hand them to.
        }
    }

    GeneratedServer {
        name: entry.name.clone(),
        command: launcher.to_string(),
        args,
    }
}

/// The buzz-acp registry file named by `BUZZ_ACP_MCP_REGISTRY`.
///
/// Carries the launcher command and its argv only. The capability of memo
/// decision 5 never appears in it — it reaches the launcher through the
/// inherited spawn environment — so buzz-acp resolves nothing and no resolved
/// value enters its address space or the ACP wire. No entry carries an `env`
/// block, and `buzz-acp` refuses a document that declares one.
pub const BUZZ_ACP_REGISTRY_ENV_VAR: &str = "BUZZ_ACP_MCP_REGISTRY";

/// Render the buzz-acp registry file.
///
/// # Errors
/// Only if serialization fails, which would mean a non-string key.
pub fn render_buzz_acp_registry(servers: &[GeneratedServer]) -> Result<String, String> {
    let document = serde_json::json!({
        "version": 1,
        "servers": servers
            .iter()
            .map(|server| serde_json::json!({
                "name": server.name,
                "command": server.command,
                "args": server.args,
            }))
            .collect::<Vec<_>>(),
    });
    serde_json::to_string_pretty(&document).map_err(|e| e.to_string())
}

/// Render Claude's project `.mcp.json`.
///
/// No entry carries an `env` block; see the module header.
///
/// # Errors
/// Only if serialization fails.
pub fn render_claude_project_config(servers: &[GeneratedServer]) -> Result<String, String> {
    let mut map = serde_json::Map::new();
    for server in servers {
        map.insert(
            server.name.clone(),
            serde_json::json!({
                "command": server.command,
                "args": server.args,
            }),
        );
    }
    serde_json::to_string_pretty(&serde_json::json!({ "mcpServers": map }))
        .map_err(|e| e.to_string())
}

/// Render Codex's `config.toml` MCP section.
///
/// No entry carries an `env` block; see the module header.
///
/// # Errors
/// Only if serialization fails.
pub fn render_codex_config(servers: &[GeneratedServer]) -> Result<String, String> {
    let mut root = toml::map::Map::new();
    let mut table = toml::map::Map::new();
    for server in servers {
        let mut entry = toml::map::Map::new();
        entry.insert(
            "command".to_string(),
            toml::Value::String(server.command.clone()),
        );
        entry.insert(
            "args".to_string(),
            toml::Value::Array(
                server
                    .args
                    .iter()
                    .map(|arg| toml::Value::String(arg.clone()))
                    .collect(),
            ),
        );
        table.insert(server.name.clone(), toml::Value::Table(entry));
    }
    root.insert("mcp_servers".to_string(), toml::Value::Table(table));
    toml::to_string_pretty(&toml::Value::Table(root)).map_err(|e| e.to_string())
}
