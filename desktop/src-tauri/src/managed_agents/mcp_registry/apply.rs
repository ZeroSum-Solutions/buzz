//! The production caller for [`converge`], and the app-start reconcile.
//!
//! T7a built the core and T7b wired the spawn seam, but nothing in a shipped
//! build ever staged a generation: `plan_for_spawn` looked through the
//! `current` pointer and found nothing, so the registry was inert. This module
//! is the missing half — the one place a registry edit or a per-agent toggle
//! turns into an adopted generation, and the one place a start finishes
//! whatever a crash left owed.
//!
//! Three things are asserted here rather than assumed, because this is the
//! seam where a value stops being a suggestion and becomes what a child
//! process runs:
//!
//! 1. **The launcher path is absolute.** A relative launcher would make
//!    `buzz-acp` refuse to boot for every registry-enabled agent, and would be
//!    re-resolved against the session's own working directory if it did not.
//! 2. **The launcher exists and is a regular file.** A generated config naming
//!    a missing binary is a configuration every one of those agents fails on,
//!    discovered at spawn instead of at save.
//! 3. **The convergence is whole-set.** Every managed agent is passed, even
//!    one with no selection: one pointer names one generation for all of them,
//!    so an agent left out would be revoked *and* left without artefacts.

use std::path::Path;

use tauri::AppHandle;

use super::converge::{
    converge, AgentSelection, Converged, GenerationInputs, SecretStoreIo, UuidNonces,
};
use super::generation::{GenerationStore, Reconciled, SecretRemover};
use super::load::load_registry;
use super::paths::RegistryPaths;
use crate::managed_agents::discovery::KnownAcpRuntime;
use crate::managed_agents::types::ManagedAgentRecord;

/// File name of the bundled launcher, as it is bundled beside the app binary.
#[cfg(windows)]
pub const LAUNCHER_COMMAND: &str = "buzz-mcp-launch.exe";

/// File name of the bundled launcher, as it is bundled beside the app binary.
#[cfg(not(windows))]
pub const LAUNCHER_COMMAND: &str = "buzz-mcp-launch";

/// Check `path` before it is written into every generated config.
///
/// # Errors
/// A message when the path is not absolute, does not exist, or is not a
/// regular file. Each is a configuration that would fail at spawn for every
/// registry-enabled agent instead of at the save that caused it (PR 23
/// follow-up 2).
pub fn checked_launcher(path: &Path) -> Result<String, String> {
    if !path.is_absolute() {
        return Err(format!(
            "the bundled mcp launcher resolved to {}, which is not an absolute path; every \
             generated server names it, and the agent harness refuses a relative command",
            path.display()
        ));
    }
    let meta = std::fs::symlink_metadata(path).map_err(|e| {
        format!(
            "the bundled mcp launcher {} cannot be read: {e}",
            path.display()
        )
    })?;
    // A symlink is resolved, not refused: the app bundle legitimately ships
    // one. What is refused is a path that is not, in the end, a regular file.
    let resolved = if meta.file_type().is_symlink() {
        std::fs::canonicalize(path).map_err(|e| {
            format!(
                "the bundled mcp launcher {} is a link that cannot be resolved: {e}",
                path.display()
            )
        })?
    } else {
        path.to_path_buf()
    };
    let target = std::fs::metadata(&resolved).map_err(|e| {
        format!(
            "the bundled mcp launcher {} cannot be read: {e}",
            resolved.display()
        )
    })?;
    if !target.is_file() {
        return Err(format!(
            "the bundled mcp launcher {} is not a regular file",
            resolved.display()
        ));
    }
    path.to_str().map(str::to_string).ok_or_else(|| {
        format!(
            "the bundled mcp launcher's path {} is not valid UTF-8, so it cannot be named in a \
             generated configuration file",
            path.display()
        )
    })
}

/// This agent's contribution to a convergence.
///
/// Every managed agent gets one, including an agent whose runtime the registry
/// cannot configure: the convergence is whole-set, so an agent left out would
/// fail [`super::converge::ConvergeError::MissingAgent`] rather than simply
/// receiving nothing. A runtime the registry may not configure, or an unknown
/// runtime, contributes an empty selection — which stages no artefacts and
/// carries no capability.
///
/// Memo decision 8's absent-versus-empty distinction is read here and nowhere
/// else: `None` and `Some(empty)` both resolve to no servers today, but they
/// are different records and only one of them was chosen by an operator.
pub fn selection_for_record(
    record: &ManagedAgentRecord,
    runtime_meta: Option<&KnownAcpRuntime>,
    runtime_id: &str,
) -> AgentSelection {
    let configurable = runtime_meta.is_some_and(|meta| meta.mcp_registry_available);
    AgentSelection {
        agent_id: record.pubkey.clone(),
        runtime_id: runtime_id.to_string(),
        transports: runtime_meta
            .map(|meta| meta.mcp_transports.to_vec())
            .unwrap_or_default(),
        placement: runtime_meta
            .map(|meta| meta.mcp_config_placement)
            .unwrap_or(crate::managed_agents::McpConfigPlacement::Unsupported),
        enabled: if configurable {
            record
                .mcp_servers
                .as_ref()
                .map(|selection| selection.enabled.clone())
                .unwrap_or_default()
        } else {
            // Not "drop the selection": the operator's list stays on the
            // record, so turning the runtime on later restores it. What is
            // withheld is the generation, because decision 9 has not verified
            // this runtime's isolated configuration root.
            Vec::new()
        },
    }
}

/// The durable secret store, as the generation store and the convergence see
/// it.
///
/// Read and write both go through the desktop's own `SecretStore`, which holds
/// the interprocess blob lock — so a convergence cannot interleave with
/// another Buzz process's write.
pub struct DesktopSecrets {
    service: &'static str,
}

impl DesktopSecrets {
    /// Bind to the keychain service this build stores its blob under.
    pub fn new(service: &'static str) -> Self {
        Self { service }
    }

    fn store(&self) -> &'static crate::secret_store::SecretStore {
        crate::secret_store::SecretStore::shared(self.service)
    }
}

impl SecretRemover for DesktopSecrets {
    fn remove(&self, key: &str) -> Result<(), String> {
        self.store().mutate_checked(|map| {
            map.remove(key);
            Ok(())
        })
    }

    fn write_all(
        &self,
        entries: &std::collections::BTreeMap<String, String>,
    ) -> Result<(), String> {
        if entries.is_empty() {
            return Ok(());
        }
        self.store().mutate_checked(|map| {
            for (key, value) in entries {
                map.insert(key.clone(), value.clone());
            }
            Ok(())
        })
    }
}

impl SecretStoreIo for DesktopSecrets {
    fn read_all(&self) -> Result<std::collections::BTreeMap<String, String>, String> {
        // `Ok(None)` is "no blob has ever been written", which is genuinely
        // empty. An unavailable backend is an `Err` from the store and stays
        // one: reported as empty it would drop every carried-forward secret
        // and adopt a generation whose servers cannot authenticate.
        Ok(self
            .store()
            .load_all_readonly()?
            .map(|records| records.into_iter().collect())
            .unwrap_or_default())
    }
}

/// Stage and adopt one generation from the registry document and every agent's
/// current selection.
///
/// Called after a registry edit and after a per-agent toggle change — the two
/// user actions that change what an agent's next spawn will read.
///
/// `pending` holds secret values the operator entered in this action, keyed by
/// reference id. They travel one way: into the store, under the reserved
/// `mcp:` prefix, bound to the generation this call adopts.
///
/// # Errors
/// A message when the nest is unavailable, the launcher fails
/// [`checked_launcher`], the registry document cannot be read, or the
/// convergence itself fails. Every one of them leaves the previous generation
/// adopted: nothing is half-applied.
pub fn converge_now<R: tauri::Runtime>(
    app: &AppHandle<R>,
    pending: &std::collections::BTreeMap<String, String>,
) -> Result<Converged, String> {
    let Some(paths) = crate::managed_agents::runtime::mcp_registry_paths(app)? else {
        return Err(
            "this build cannot resolve the agent working directory, so mcp server settings \
             cannot be applied"
                .to_string(),
        );
    };
    let launcher_path = crate::managed_agents::discovery::resolve_command(LAUNCHER_COMMAND)
        .ok_or_else(|| {
            format!(
                "the bundled mcp launcher `{LAUNCHER_COMMAND}` was not found beside this app, so \
                 no mcp server could be started"
            )
        })?;
    let launcher = checked_launcher(&launcher_path)?;

    let registry = load_registry(&paths.document()).map_err(|e| e.to_string())?;
    let records = crate::managed_agents::load_managed_agents(app)?;
    let personas = crate::managed_agents::load_personas(app).unwrap_or_default();
    let global = crate::managed_agents::load_global_agent_config(app).unwrap_or_default();

    let selections: Vec<AgentSelection> = records
        .iter()
        .map(|record| {
            // A dangling harness id degrades to the record's own snapshot
            // rather than failing the whole convergence: the agent is already
            // unspawnable for a reason the spawn path reports, and dropping it
            // here would breach the whole-set rule and revoke every other
            // agent's configuration too.
            let runtime_id = crate::managed_agents::resolve_effective_harness_descriptor(
                record, &personas, &global,
            )
            .map(|descriptor| descriptor.command)
            .unwrap_or_else(|_| record.agent_command.clone());
            let meta = crate::managed_agents::known_acp_runtime(&runtime_id);
            selection_for_record(record, meta, &runtime_id)
        })
        .collect();

    let secrets = DesktopSecrets::new(crate::app_state::keyring_service());
    converge(
        &paths,
        &registry,
        &selections,
        &GenerationInputs {
            launcher: &launcher,
            keychain_service: crate::app_state::keyring_service(),
            pending,
        },
        &secrets,
        &UuidNonces,
    )
    .map_err(|e| e.to_string())
}

/// Finish whatever the last configuration change left owed.
///
/// Runs once at app start, before any agent is restored: a `PREPARED` journal
/// with no flip is discarded, and a `FLIPPED` one's keychain deletions are
/// retried until they succeed. Without it a crash mid-change leaves a
/// revoked credential in the keychain with nothing left to retry it, and the
/// next Settings action is refused because a deletion is still owed.
///
/// # Errors
/// A message when the staging tree cannot be opened or a deletion is still
/// owed after the retry. The caller logs it: a start must not be blocked by
/// configuration debt, and the next Settings action retries the same work.
pub fn reconcile_at_start<R: tauri::Runtime>(app: &AppHandle<R>) -> Result<Reconciled, String> {
    let Some(paths) = crate::managed_agents::runtime::mcp_registry_paths(app)? else {
        return Ok(Reconciled::Nothing);
    };
    let store = GenerationStore::open(&paths.generations_root()).map_err(|e| e.to_string())?;
    let secrets = DesktopSecrets::new(crate::app_state::keyring_service());
    store
        .reconcile(&secrets, &super::generation::NoHooks)
        .map_err(|e| e.to_string())
}

/// Roots for this app. Re-exported so a caller outside `runtime` can build the
/// same pair the spawn path uses.
pub fn registry_paths<R: tauri::Runtime>(
    app: &AppHandle<R>,
) -> Result<Option<RegistryPaths>, String> {
    crate::managed_agents::runtime::mcp_registry_paths(app)
}
