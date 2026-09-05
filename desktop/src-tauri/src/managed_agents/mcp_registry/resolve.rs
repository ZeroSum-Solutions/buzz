//! Turning a loaded registry plus an agent's toggles into the servers that
//! agent will actually get — or into a refusal to spawn (memo decision 7).
//!
//! A rejected entry does not silently disappear. An agent that has one toggled
//! on refuses to spawn, with the same message the panel shows beside the entry,
//! rather than starting silently short a server the operator asked for.

use super::load::{LoadedEntry, LoadedRegistry};
use super::schema::{RegistryEntry, MAX_SERVERS_PER_AGENT};
use crate::managed_agents::McpTransport;

/// Why an agent cannot be spawned with its current registry selection.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpawnRefusal {
    /// An enabled id names no entry in the registry.
    #[error("this agent has mcp server `{0}` enabled, but the registry no longer declares it")]
    UnknownServer(String),
    /// An enabled entry is disabled by the loader.
    #[error("mcp server `{name}` is disabled: {reason}")]
    RejectedEntry {
        /// The entry's name.
        name: String,
        /// The loader's reason, verbatim.
        reason: String,
    },
    /// An enabled entry needs a transport this runtime cannot be offered.
    #[error("mcp server `{name}` is an http server, which the `{runtime}` runtime cannot use")]
    TransportUnsupported {
        /// The entry's name.
        name: String,
        /// The runtime id.
        runtime: String,
    },
    /// More than [`MAX_SERVERS_PER_AGENT`] enabled.
    #[error("this agent enables {count} mcp servers, over the {MAX_SERVERS_PER_AGENT} cap")]
    TooManyServers {
        /// How many were enabled.
        count: usize,
    },
}

/// The servers one agent will be configured with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedServers {
    /// Entries in the order the registry declares them.
    pub servers: Vec<RegistryEntry>,
}

/// Resolve `enabled` against `registry` for a runtime offering `transports`.
///
/// # Errors
/// The first [`SpawnRefusal`] found. Every one of them is a refusal to start
/// the agent, not a warning: an agent short a server it was told to have is a
/// silent behaviour change the operator cannot see.
pub fn resolve_for_agent(
    registry: &LoadedRegistry,
    runtime_id: &str,
    transports: &[McpTransport],
    enabled: &[String],
) -> Result<ResolvedServers, SpawnRefusal> {
    if enabled.len() > MAX_SERVERS_PER_AGENT {
        return Err(SpawnRefusal::TooManyServers {
            count: enabled.len(),
        });
    }

    let mut servers = Vec::new();
    for id in enabled {
        let Some(LoadedEntry { entry, rejection }) = registry.by_id(id) else {
            return Err(SpawnRefusal::UnknownServer(id.clone()));
        };
        if let Some(reason) = rejection {
            return Err(SpawnRefusal::RejectedEntry {
                name: entry.name.clone(),
                reason: reason.clone(),
            });
        }
        let needed = if entry.is_http() {
            McpTransport::Http
        } else {
            McpTransport::Stdio
        };
        if !transports.contains(&needed) {
            return Err(SpawnRefusal::TransportUnsupported {
                name: entry.name.clone(),
                runtime: runtime_id.to_string(),
            });
        }
        servers.push(entry.clone());
    }

    // Document order, not toggle order, so two agents with the same selection
    // generate byte-identical config regardless of how it was clicked.
    servers.sort_by(|a, b| {
        let position = |entry: &RegistryEntry| {
            registry
                .entries
                .iter()
                .position(|loaded| loaded.entry.id == entry.id)
                .unwrap_or(usize::MAX)
        };
        position(a).cmp(&position(b))
    });
    Ok(ResolvedServers { servers })
}
