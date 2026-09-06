//! Where the registry keeps its document, its staging tree and each agent's
//! own working directory (memo decisions 8 and 9).
//!
//! Every path an agent id reaches goes through [`RegistryPaths::agent_dir`],
//! which validates the id against the same charset the capability accepts. An
//! agent id is a relay-sourced hex pubkey today, but it is the only
//! caller-supplied component of these paths, so it is checked here rather than
//! trusted: `..` or a separator in an id would put one agent's generated
//! configuration in another agent's directory.

use std::path::{Path, PathBuf};

use buzz_secret_store_pkg::capability::MAX_AGENT_ID_LEN;

use super::REGISTRY_FILE_NAME;

/// Directory inside a generation, and inside the workdir root, that holds one
/// agent's own files.
pub const AGENTS_SUBDIR: &str = "agents";

/// Staging-tree directory name inside the base directory.
pub const GENERATIONS_SUBDIR: &str = "mcp";

/// Name of the buzz-acp handover file inside an agent's generation directory.
pub const BUZZ_ACP_REGISTRY_FILE: &str = "buzz-acp-registry.json";

/// Name of the file recording why an agent's selection could not be resolved.
///
/// Its presence is what makes a rejected entry refuse a spawn after a restart:
/// the loader does not run at spawn, so the generation has to carry the
/// refusal rather than the artefacts.
pub const REFUSAL_FILE: &str = "refusal.txt";

/// Largest accepted generated artefact, in bytes.
///
/// The source document is capped at 256 KiB with at most 256 entries; the
/// generated form is larger per entry, so this is its own bound rather than a
/// copy of that one. It bounds what a spawn reads and copies.
pub const MAX_ARTEFACT_BYTES: usize = 1024 * 1024;

/// Why an agent id cannot be used as a path component.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("agent id is not usable as a directory name: it must be 1 to {MAX_AGENT_ID_LEN} characters of `[a-z0-9_-]`")]
pub struct AgentIdError;

/// Whether `id` may become a directory name and a capability agent id.
///
/// The charset is [`buzz_secret_store_pkg::AgentCapability`]'s, so an id that
/// passes here is one the capability can also carry: the two must agree or a
/// staged generation would mint a capability the launcher cannot parse.
pub fn validate_agent_id(id: &str) -> Result<(), AgentIdError> {
    if id.is_empty()
        || id.len() > MAX_AGENT_ID_LEN
        || !id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        return Err(AgentIdError);
    }
    Ok(())
}

/// The filesystem roots the registry works in.
#[derive(Debug, Clone)]
pub struct RegistryPaths {
    /// Directory holding the registry document and the staging tree. The app
    /// data directory's `agents/` folder in production.
    base: PathBuf,
    /// Root under which each agent gets its own working directory. The nest in
    /// production.
    workdir_root: PathBuf,
}

impl RegistryPaths {
    /// Build the roots.
    pub fn new(base: impl Into<PathBuf>, workdir_root: impl Into<PathBuf>) -> Self {
        Self {
            base: base.into(),
            workdir_root: workdir_root.into(),
        }
    }

    /// The operator-editable registry document.
    pub fn document(&self) -> PathBuf {
        self.base.join(REGISTRY_FILE_NAME)
    }

    /// The staging tree root, which [`super::generation::GenerationStore`] owns.
    pub fn generations_root(&self) -> PathBuf {
        self.base.join(GENERATIONS_SUBDIR)
    }

    /// One agent's directory inside `parent`, with the id validated.
    ///
    /// # Errors
    /// [`AgentIdError`] when `agent_id` is empty, over the cap, or holds a
    /// character outside `[a-z0-9_-]` — which includes every separator and
    /// every spelling of `..`.
    pub fn agent_dir(parent: &Path, agent_id: &str) -> Result<PathBuf, AgentIdError> {
        validate_agent_id(agent_id)?;
        Ok(parent.join(AGENTS_SUBDIR).join(agent_id))
    }

    /// One agent's own working directory.
    ///
    /// # Errors
    /// [`AgentIdError`], as [`RegistryPaths::agent_dir`].
    pub fn agent_workdir(&self, agent_id: &str) -> Result<PathBuf, AgentIdError> {
        Self::agent_dir(&self.workdir_root, agent_id)
    }
}
