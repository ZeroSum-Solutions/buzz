//! Generating the three configuration artefacts (memo decisions 3, 9 and 10).
//!
//! Every generated stdio entry names the bundled `buzz-mcp-launch` by resolved
//! absolute path, never a bare name — a bare name would resolve through a
//! `PATH` the launcher's own clear removes. Every HTTP entry becomes a stdio
//! entry pointing at the same binary in proxy mode, because no launcher can
//! inject an environment into a remote server.
//!
//! No generated file holds a secret **value**. A credential travels as an
//! `mcp:` reference, in two places written from one source: the entry's `env`
//! block, which is what the runtime hands the launcher process, and the
//! launcher's own `--secret` flags, which is the channel the launcher reads
//! (it builds its child environment from empty and so ignores what it
//! inherits). `generated_env_block_and_launcher_flags_agree` binds the two.

use std::collections::BTreeMap;

use buzz_secret_store_pkg::looks_like_reference;

use super::schema::{RegistryEntry, RegistryTransport};

/// One generated server: the command line and the env block for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedServer {
    /// The registry name, which becomes the config key.
    pub name: String,
    /// Absolute path of the bundled launcher.
    pub command: String,
    /// Launcher arguments, references included.
    pub args: Vec<String>,
    /// Reference-valued environment. Never a secret value.
    pub env: BTreeMap<String, String>,
}

/// Build the launcher invocation for one registry entry.
pub fn generate_server(launcher: &str, entry: &RegistryEntry) -> GeneratedServer {
    let mut args = Vec::new();
    let mut env = BTreeMap::new();

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
                env.insert(name.clone(), value.clone());
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
            // An http upstream gets no child environment: there is no child
            // process to hand variables to.
        }
    }

    GeneratedServer {
        name: entry.name.clone(),
        command: launcher.to_string(),
        args,
        env,
    }
}

/// The buzz-acp registry file named by `BUZZ_ACP_MCP_REGISTRY`.
///
/// Carries references only. The capability of memo decision 5 never appears in
/// it — it reaches the launcher through the inherited spawn environment — so
/// buzz-acp resolves nothing and no resolved value enters its address space or
/// the ACP wire.
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
                "env": server
                    .env
                    .iter()
                    .map(|(name, value)| serde_json::json!({ "name": name, "value": value }))
                    .collect::<Vec<_>>(),
            }))
            .collect::<Vec<_>>(),
    });
    serde_json::to_string_pretty(&document).map_err(|e| e.to_string())
}

/// Render Claude's project `.mcp.json`.
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
                "env": server.env,
            }),
        );
    }
    serde_json::to_string_pretty(&serde_json::json!({ "mcpServers": map }))
        .map_err(|e| e.to_string())
}

/// Render Codex's `config.toml` MCP section.
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
        if !server.env.is_empty() {
            let mut env = toml::map::Map::new();
            for (name, value) in &server.env {
                env.insert(name.clone(), toml::Value::String(value.clone()));
            }
            entry.insert("env".to_string(), toml::Value::Table(env));
        }
        table.insert(server.name.clone(), toml::Value::Table(entry));
    }
    root.insert("mcp_servers".to_string(), toml::Value::Table(table));
    toml::to_string_pretty(&toml::Value::Table(root)).map_err(|e| e.to_string())
}
