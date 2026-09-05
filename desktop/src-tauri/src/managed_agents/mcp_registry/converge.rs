//! Turning the registry document plus each agent's selection into one adopted
//! configuration generation (memo decisions 5, 8 and 9).
//!
//! This is the production seam behind "a server removed from the registry
//! stops authenticating". A generation carries, per agent, either the
//! generated artefacts or the refusal that explains why it has none; and each
//! agent's secrets are re-keyed from the base generation to the new one before
//! the pointer moves, so a server no longer selected is simply not carried
//! forward. Its old key is then deleted as a post-flip, journalled step, and
//! the capability the previous generation minted stops resolving anything at
//! all: the binding record it verifies against belongs to that generation too.
//!
//! Ordering matters and is deliberate. The carried-forward secrets and the new
//! binding records are written **before** the flip, so at the instant the
//! pointer moves the new generation already resolves. A crash between the two
//! leaves keys nothing points at, which the next convergence's deletions
//! sweep; the reverse order would leave an adopted generation whose servers
//! cannot authenticate.

use std::collections::BTreeMap;
use std::path::PathBuf;

use buzz_secret_store_pkg::capability::NONCE_LEN;
use buzz_secret_store_pkg::namespace::MCP_NAMESPACE_PREFIX;
use buzz_secret_store_pkg::{
    binding_key, looks_like_reference, storage_key, AgentCapability, CapabilityError, McpSecretRef,
};

use super::generate::{
    generate_server, render_buzz_acp_registry, render_claude_project_config, render_codex_config,
    GeneratedServer,
};
use super::generation::{Deletion, GenerationError, GenerationPlan, GenerationStore, NoHooks};
use super::load::LoadedRegistry;
use super::paths::{
    AgentIdError, RegistryPaths, AGENTS_SUBDIR, BUZZ_ACP_REGISTRY_FILE, REFUSAL_FILE,
};
use super::resolve::resolve_for_agent;
use super::schema::{RegistryEntry, RegistryTransport};
use crate::managed_agents::{McpConfigPlacement, McpTransport};

/// Largest number of agents one convergence will stage.
///
/// The staged tree, the secret blob and the deletion journal all grow with this
/// count, so it is bounded directly rather than through the size of any one of
/// them.
pub const MAX_CONVERGED_AGENTS: usize = 256;

/// What one agent brings to a convergence.
#[derive(Debug, Clone)]
pub struct AgentSelection {
    /// The agent's id — its pubkey in production. Validated as a path
    /// component and as a capability field.
    pub agent_id: String,
    /// The runtime id, for the refusal message.
    pub runtime_id: String,
    /// Transports the registry may offer this runtime.
    pub transports: Vec<McpTransport>,
    /// Where this runtime's native MCP config has to be written.
    pub placement: McpConfigPlacement,
    /// Registry entry ids this agent has enabled, in any order.
    pub enabled: Vec<String>,
}

/// Where a resolved secret has to be readable from.
///
/// A trait so a test drives the same convergence code production runs against
/// a store it controls, rather than the machine keychain.
pub trait SecretStoreIo: super::generation::SecretRemover {
    /// Every record in the store.
    ///
    /// # Errors
    /// A message when the store is unavailable. Unavailable must never be
    /// reported as "empty": that would silently drop every carried-forward
    /// secret and adopt a generation whose servers cannot authenticate.
    fn read_all(&self) -> Result<BTreeMap<String, String>, String>;

    /// Insert or overwrite `entries`, leaving every other record alone.
    ///
    /// # Errors
    /// A message when the store is unavailable or the write breaches a blob
    /// bound.
    fn write_all(&self, entries: &BTreeMap<String, String>) -> Result<(), String>;
}

/// Where the unguessable half of a capability comes from.
///
/// A trait so a test can pin the nonce and assert the exact binding record;
/// production passes [`UuidNonces`].
pub trait NonceSource {
    /// Fresh entropy for one capability.
    fn nonce(&self) -> [u8; NONCE_LEN];
}

/// The production nonce source: a fresh v4 UUID per capability.
pub struct UuidNonces;

impl NonceSource for UuidNonces {
    fn nonce(&self) -> [u8; NONCE_LEN] {
        *uuid::Uuid::new_v4().as_bytes()
    }
}

/// Why a convergence could not be applied.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConvergeError {
    /// An agent id is not usable as a path component or a capability field.
    #[error("{0}")]
    AgentId(#[from] AgentIdError),
    /// More agents than [`MAX_CONVERGED_AGENTS`].
    #[error("{count} agents were given to one convergence, over the {MAX_CONVERGED_AGENTS} cap")]
    TooManyAgents {
        /// How many were given.
        count: usize,
    },
    /// Two selections name the same agent, so which one wins would be
    /// list order.
    #[error("agent `{0}` appears twice in one convergence")]
    DuplicateAgent(String),
    /// A capability could not be minted for an otherwise valid id.
    #[error("could not mint a capability: {0}")]
    Capability(#[from] CapabilityError),
    /// The staging tree refused the change.
    #[error(transparent)]
    Generation(#[from] GenerationError),
}

/// What a convergence adopted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Converged {
    /// The generation now named by the pointer.
    pub generation: u64,
    /// Agents whose selection could not be resolved, with the message the
    /// panel shows and the spawn refuses with. Not an error: the rest of the
    /// registry still loads, per memo decision 7.
    pub refused: Vec<(String, String)>,
}

/// Stage and adopt one generation covering every agent in `agents`.
///
/// # Errors
/// [`ConvergeError`]. Nothing is adopted unless the pointer moved; a failure
/// after it leaves the new generation adopted with its deletions still owed in
/// the journal, which the next start retries.
pub fn converge<S: SecretStoreIo>(
    paths: &RegistryPaths,
    registry: &LoadedRegistry,
    agents: &[AgentSelection],
    launcher: &str,
    secrets: &S,
    nonces: &dyn NonceSource,
) -> Result<Converged, ConvergeError> {
    if agents.len() > MAX_CONVERGED_AGENTS {
        return Err(ConvergeError::TooManyAgents {
            count: agents.len(),
        });
    }
    let mut seen = std::collections::BTreeSet::new();
    for agent in agents {
        super::paths::validate_agent_id(&agent.agent_id)?;
        if !seen.insert(agent.agent_id.as_str()) {
            return Err(ConvergeError::DuplicateAgent(agent.agent_id.clone()));
        }
    }

    let store = GenerationStore::open(&paths.generations_root())?;
    let mut refused: Vec<(String, String)> = Vec::new();

    let generation = store.commit(
        |base, _base_dir| {
            refused.clear();
            let next = base.map(|number| number + 1).unwrap_or(1);
            let existing = secrets.read_all().map_err(GenerationError::Plan)?;

            let mut files: Vec<(PathBuf, String)> = Vec::new();
            let mut carried: BTreeMap<String, String> = BTreeMap::new();

            for agent in agents {
                let dir = PathBuf::from(AGENTS_SUBDIR).join(&agent.agent_id);
                let resolved = match resolve_for_agent(
                    registry,
                    &agent.runtime_id,
                    &agent.transports,
                    &agent.enabled,
                ) {
                    Ok(resolved) => resolved,
                    Err(refusal) => {
                        // The loader does not run at spawn — the generation is
                        // what a spawn reads — so the refusal is staged beside
                        // the artefacts it replaces. An agent told to have a
                        // server it cannot have refuses to start rather than
                        // starting silently short of it.
                        let message = refusal.to_string();
                        refused.push((agent.agent_id.clone(), message.clone()));
                        files.push((dir.join(REFUSAL_FILE), message));
                        continue;
                    }
                };
                if resolved.servers.is_empty() {
                    continue;
                }

                // The capability is minted per (agent, generation), not per
                // spawn: one record can have a runtime on more than one relay,
                // and rotating on every spawn would revoke a live sibling's
                // capability mid-turn. The generation is what rotates it.
                let capability = AgentCapability::mint(&agent.agent_id, next, nonces.nonce())
                    .map_err(|e| GenerationError::Plan(e.to_string()))?;
                carried.insert(
                    binding_key(&capability),
                    capability.binding_value().to_string(),
                );
                carry_secrets(
                    &capability,
                    base,
                    &resolved.servers,
                    &existing,
                    &mut carried,
                );

                let generated: Vec<GeneratedServer> = resolved
                    .servers
                    .iter()
                    .map(|entry| generate_server(launcher, entry))
                    .collect();
                for (name, body) in render_artefacts(agent.placement, &generated)? {
                    files.push((dir.join(name), body));
                }
            }

            // Written before the flip: at the instant the pointer moves, the
            // adopted generation's secrets already resolve.
            secrets.write_all(&carried).map_err(GenerationError::Plan)?;

            Ok(GenerationPlan {
                files,
                deletions: stale_secret_deletions(&existing, next),
            })
        },
        secrets,
        &NoHooks,
    )?;

    Ok(Converged {
        generation,
        refused,
    })
}

/// Re-key every reference one agent's selected servers name, from the base
/// generation to the capability's own.
///
/// A server that is no longer selected is simply never visited, so its key is
/// not carried forward and [`stale_secret_deletions`] removes it. That is the
/// whole of "a deleted server stops authenticating": there is nothing to
/// revoke, because the new generation never had it.
fn carry_secrets(
    capability: &AgentCapability,
    base: Option<u64>,
    servers: &[RegistryEntry],
    existing: &BTreeMap<String, String>,
    carried: &mut BTreeMap<String, String>,
) {
    let Some(base) = base else {
        return;
    };
    for entry in servers {
        for reference in entry_references(entry) {
            let from = format!(
                "{MCP_NAMESPACE_PREFIX}{}:{base}:{}",
                capability.agent_id(),
                reference.id()
            );
            if let Some(value) = existing.get(&from) {
                carried.insert(storage_key(capability, &reference), value.clone());
            }
        }
    }
}

/// Every `mcp:` reference one entry names, in its `env` block and its HTTP
/// auth. A literal value is not a reference and is skipped.
fn entry_references(entry: &RegistryEntry) -> Vec<McpSecretRef> {
    let mut references: Vec<McpSecretRef> = entry
        .env
        .values()
        .filter(|value| looks_like_reference(value))
        .filter_map(|value| McpSecretRef::parse(value).ok())
        .collect();
    if let RegistryTransport::Http {
        auth: Some(auth), ..
    } = &entry.transport
    {
        if let Ok(reference) = McpSecretRef::parse(&auth.secret) {
            references.push(reference);
        }
    }
    references
}

/// Every `mcp:` record that does not belong to the adopted generation.
///
/// These are the post-flip deletions the journal retries until they succeed.
/// Retention keeps one rollback *generation directory*, but not its secrets: a
/// rollback that could still authenticate a deleted server is the thing this
/// convergence exists to prevent.
fn stale_secret_deletions(existing: &BTreeMap<String, String>, adopted: u64) -> Vec<Deletion> {
    let keep = format!(":{adopted}:");
    existing
        .keys()
        .filter(|key| key.starts_with(MCP_NAMESPACE_PREFIX) && !key.contains(&keep))
        .map(|key| Deletion::Secret { key: key.clone() })
        .collect()
}

/// The files one agent's placement calls for, as `(name, body)` pairs.
fn render_artefacts(
    placement: McpConfigPlacement,
    servers: &[GeneratedServer],
) -> Result<Vec<(String, String)>, GenerationError> {
    let render = |result: Result<String, String>| {
        result.map_err(|e| GenerationError::Plan(format!("render: {e}")))
    };
    Ok(match placement {
        // buzz-agent has no native MCP config; the registry reaches it through
        // the handover file `BUZZ_ACP_MCP_REGISTRY` names.
        McpConfigPlacement::Unsupported => vec![(
            BUZZ_ACP_REGISTRY_FILE.to_string(),
            render(render_buzz_acp_registry(servers))?,
        )],
        McpConfigPlacement::ProjectFileInWorkdir { file } => {
            vec![(
                file.to_string(),
                render(render_claude_project_config(servers))?,
            )]
        }
        McpConfigPlacement::EnvRootedDir { file, .. } => {
            vec![(file.to_string(), render(render_codex_config(servers))?)]
        }
    })
}
