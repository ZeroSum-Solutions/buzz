//! What one spawn takes from the adopted generation (memo decisions 5, 9 and
//! 11).
//!
//! Everything is resolved through the `current` pointer and never through a
//! staged directory, so a spawn always reads one generation whole. The loader
//! does not run here: a generation carries either the artefacts a placement
//! calls for or the refusal that explains why it has none, and a refusal
//! refuses the spawn with the message the panel shows.
//!
//! Nothing this module produces carries a secret **value**. The generated
//! files hold `mcp:` references; the one credential-shaped thing in the plan
//! is the capability, which travels in the spawn environment — never in argv,
//! never in a generated file, and never in a log line.

use std::path::{Path, PathBuf};

use buzz_secret_store_pkg::{binding_key_for, AgentCapability, CAPABILITY_ENV_VAR};

use super::generate::BUZZ_ACP_REGISTRY_ENV_VAR;
use super::generation::{create_no_follow, sync_dir, GenerationStore};
use super::paths::{RegistryPaths, BUZZ_ACP_REGISTRY_FILE, MAX_ARTEFACT_BYTES, REFUSAL_FILE};
use crate::managed_agents::McpConfigPlacement;

/// Every environment variable a plan can set, at **any** placement.
///
/// The spawn strips all of them before it applies the plan, so an ambient
/// value inherited from the desktop's own environment can never stand in for
/// one the plan did not set. Two cases matter and they are different. A stale
/// `BUZZ_MCP_CAPABILITY` would hand a child a token for a generation that no
/// longer exists. An ambient `CODEX_HOME` — the operator's own Codex root, in
/// the desktop's environment because the operator exported it — would become
/// the agent's configuration root whenever its plan is empty, which is the
/// root memo decision 9 is written against.
///
/// The env-rooted names are read from the runtime catalog rather than listed
/// here, so a runtime added with a new [`McpConfigPlacement::EnvRootedDir`]
/// variable cannot be added without its variable being stripped.
pub fn managed_env_vars() -> Vec<&'static str> {
    let mut vars = vec![BUZZ_ACP_REGISTRY_ENV_VAR, CAPABILITY_ENV_VAR];
    for runtime in crate::managed_agents::discovery::KNOWN_ACP_RUNTIMES {
        if let McpConfigPlacement::EnvRootedDir { var, .. } = runtime.mcp_config_placement {
            if !vars.contains(&var) {
                vars.push(var);
            }
        }
    }
    vars
}

/// What a spawn has to apply to its command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpSpawnPlan {
    /// Working directory this agent must run in, when its placement needs one.
    ///
    /// `None` leaves the shared nest as the working directory. Only a
    /// placement that writes a workdir-relative file moves it: every agent has
    /// its own directory, but relocating a spawn that does not need it would
    /// silently move nest-relative skill discovery, `AGENTS.md` and `REPOS`
    /// out from under the harness.
    pub workdir: Option<PathBuf>,
    /// Variables to set, applied after the user env layer so a saved value
    /// cannot shadow them.
    pub set: Vec<(String, String)>,
}

impl McpSpawnPlan {
    /// Whether this plan gives the agent any registry server at all.
    pub fn is_empty(&self) -> bool {
        self.set.is_empty() && self.workdir.is_none()
    }
}

/// Resolve the adopted generation for one agent.
///
/// `read_binding` reads one record from the secret store by blob key; it is a
/// parameter so the caller keeps the one keychain handle and a test drives the
/// same code without one. The capability's nonce comes from that record — it
/// is minted when a generation is staged, not per spawn, because one record can
/// have a runtime on more than one relay and rotating per spawn would revoke a
/// live sibling's capability mid-turn.
///
/// # Errors
/// A message when the agent id is unusable, the staging tree cannot be read,
/// the generation carries a refusal for this agent, an artefact is missing or
/// over [`MAX_ARTEFACT_BYTES`], the working directory cannot be created, or the
/// generation staged servers for this agent with no binding record to
/// authenticate them. Every one of them refuses the spawn: an agent that starts
/// short of a server it was told to have, or with servers it cannot
/// authenticate, is a silent behaviour change the operator cannot see.
pub fn plan_for_spawn<F>(
    paths: &RegistryPaths,
    agent_id: &str,
    placement: McpConfigPlacement,
    read_binding: F,
) -> Result<McpSpawnPlan, String>
where
    F: FnOnce(&str) -> Result<Option<String>, String>,
{
    // Validated first, before any state is read: an unusable id must be
    // reported as one whether or not a generation happens to exist, so the
    // guard cannot be satisfied by an early return.
    super::paths::validate_agent_id(agent_id).map_err(|e| e.to_string())?;
    let store = GenerationStore::open(&paths.generations_root()).map_err(|e| e.to_string())?;
    let Some(generation) = store.current().map_err(|e| e.to_string())? else {
        return Ok(McpSpawnPlan::default());
    };
    let staged = RegistryPaths::agent_dir(&store.generation_dir(generation), agent_id)
        .map_err(|e| e.to_string())?;

    let refusal = staged.join(REFUSAL_FILE);
    if staged_file_present(&refusal)? {
        return Err(read_artefact(&refusal)?);
    }

    let Some((file, body)) = artefact(&staged, placement)? else {
        return Ok(McpSpawnPlan::default());
    };

    let nonce = read_binding(&binding_key_for(agent_id, generation))?.ok_or_else(|| {
        format!(
            "this agent's mcp configuration (generation {generation}) has no binding record, so \
             its servers could not authenticate; re-apply the mcp server settings and try again"
        )
    })?;
    let capability = AgentCapability::bind(agent_id, generation, &nonce)
        .map_err(|e| format!("this agent's mcp binding record is unusable: {e}"))?;

    let mut plan = McpSpawnPlan {
        set: vec![(CAPABILITY_ENV_VAR.to_string(), capability.to_env_value())],
        ..McpSpawnPlan::default()
    };

    match placement {
        // No native config file: buzz-acp reads the handover file this names.
        // The path is inside the adopted generation, read-only as far as the
        // agent is concerned, so no copy is needed and the working directory
        // does not move.
        McpConfigPlacement::Unsupported => {
            plan.set.push((
                BUZZ_ACP_REGISTRY_ENV_VAR.to_string(),
                file.display().to_string(),
            ));
        }
        // Claude reads a cwd-relative project file, so the agent has to run in
        // its own directory or two agents would share one file.
        McpConfigPlacement::ProjectFileInWorkdir { file: name } => {
            let workdir = create_workdir(paths, agent_id)?;
            write_atomically(&workdir.join(name), &body)?;
            plan.workdir = Some(workdir);
        }
        // Codex reads a file under a directory an env var names. The directory
        // lives inside the agent's own working directory for the same reason.
        McpConfigPlacement::EnvRootedDir { var, file: name } => {
            let workdir = create_workdir(paths, agent_id)?;
            let root = workdir.join(env_rooted_dir_name(var));
            std::fs::create_dir_all(&root)
                .map_err(|e| format!("failed to create {}: {e}", root.display()))?;
            write_atomically(&root.join(name), &body)?;
            plan.set.push((var.to_string(), root.display().to_string()));
            plan.workdir = Some(workdir);
        }
    }
    Ok(plan)
}

/// Directory name for an [`McpConfigPlacement::EnvRootedDir`] root.
///
/// Derived from the variable rather than added to the placement, so a runtime
/// added to the catalog cannot forget it: `CODEX_HOME` becomes `codex-home`.
fn env_rooted_dir_name(var: &str) -> String {
    var.to_ascii_lowercase().replace('_', "-")
}

/// The staged artefact this placement reads, with its bytes.
///
/// `Ok(None)` means the generation staged nothing for this agent, which is the
/// ordinary "no registry servers" state.
fn artefact(
    staged: &Path,
    placement: McpConfigPlacement,
) -> Result<Option<(PathBuf, String)>, String> {
    let name = match placement {
        McpConfigPlacement::Unsupported => BUZZ_ACP_REGISTRY_FILE,
        McpConfigPlacement::ProjectFileInWorkdir { file } => file,
        McpConfigPlacement::EnvRootedDir { file, .. } => file,
    };
    let path = staged.join(name);
    if !staged_file_present(&path)? {
        return Ok(None);
    }
    let body = read_artefact(&path)?;
    Ok(Some((path, body)))
}

/// Whether a staged artefact is present **and** is a plain file.
///
/// `Path::exists` answers "a `stat` succeeded", which is true of a FIFO, a
/// device and a symlink to either. Opening a FIFO blocks until a writer
/// appears, so a spawn that probed with `exists` and then opened would hang
/// forever rather than refuse — with no timeout anywhere on the path. The type
/// is read from `symlink_metadata`, which does not traverse the final
/// component, so a link planted over the artefact is refused rather than
/// followed (Sol N9). The open itself repeats the regular-file check on the
/// **opened handle**, so a swap between this call and the open cannot slip
/// through.
///
/// # Errors
/// A message when the path exists but is not a regular file, or when its type
/// cannot be read at all. Neither is reported as absent: read as "no
/// artefact", a planted FIFO would silently turn a configured agent into an
/// unconfigured one.
fn staged_file_present(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => Ok(true),
        Ok(meta) => Err(format!(
            "{} is not a regular file ({:?}); this agent's mcp configuration was not written by \
             this desktop",
            path.display(),
            meta.file_type()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!(
            "failed to read the type of {}: {e}",
            path.display()
        )),
    }
}

/// Read one staged file, bounded at [`MAX_ARTEFACT_BYTES`].
///
/// The open does not follow a symlink at the final component and the opened
/// handle must be a regular file, so neither a link nor a FIFO swapped in
/// after [`staged_file_present`] can redirect or block the read.
fn read_artefact(path: &Path) -> Result<String, String> {
    use std::io::Read as _;
    let file = open_artefact_no_follow(path)
        .map_err(|e| format!("failed to open {}: {e}", path.display()))?;
    let kind = file
        .metadata()
        .map_err(|e| format!("failed to read the type of {}: {e}", path.display()))?
        .file_type();
    if !kind.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    // One byte past the cap, so the exhausted limit is what reports an
    // oversized file rather than a truncated read passing as a whole one.
    let mut bounded = file.take(MAX_ARTEFACT_BYTES as u64 + 1);
    let mut bytes = Vec::new();
    bounded
        .read_to_end(&mut bytes)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    if bounded.limit() == 0 {
        return Err(format!(
            "{} is larger than the {MAX_ARTEFACT_BYTES}-byte cap",
            path.display()
        ));
    }
    String::from_utf8(bytes).map_err(|e| format!("{} is not UTF-8: {e}", path.display()))
}

/// Create this agent's own working directory.
fn create_workdir(paths: &RegistryPaths, agent_id: &str) -> Result<PathBuf, String> {
    let workdir = paths.agent_workdir(agent_id).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&workdir)
        .map_err(|e| format!("failed to create {}: {e}", workdir.display()))?;
    Ok(workdir)
}

/// Open a staged artefact for reading without following a symlink at its final
/// component.
fn open_artefact_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the path is a symbolic link",
            ));
        }
        std::fs::File::open(path)
    }
}

/// Write `body` to `path` through a temporary file and a rename.
///
/// A harness reading the file while it is rewritten must see one whole
/// generation's configuration or the previous one, never a prefix of the new.
///
/// The temporary file's bytes and the parent directory's new entry are both
/// fsynced (PR 23 follow-up 3). Without the first, a crash after the rename
/// can leave the harness's config file present, named and empty; without the
/// second, the rename itself can be lost while the temporary file survives, so
/// the agent starts from the previous generation while the desktop believes it
/// installed this one. `write_file_synced` in `generation.rs` already had this
/// shape; this is the same one, and it is also what refuses a symlink planted
/// at the fixed `.tmp` name.
fn write_atomically(path: &Path, body: &str) -> Result<(), String> {
    use std::io::Write as _;
    let temporary = path.with_extension("tmp");
    let install = |result: std::io::Result<()>, what: &str| {
        result.map_err(|e| format!("failed to {what} {}: {e}", temporary.display()))
    };
    let mut file = create_no_follow(&temporary)
        .map_err(|e| format!("failed to create {}: {e}", temporary.display()))?;
    install(file.write_all(body.as_bytes()), "write")?;
    install(file.sync_all(), "sync")?;
    drop(file);
    std::fs::rename(&temporary, path).map_err(|e| {
        // A failed rename leaves the temporary file behind; remove it so a
        // retry does not read a stale one, and report the rename failure
        // rather than the cleanup.
        let _ = std::fs::remove_file(&temporary);
        format!("failed to install {}: {e}", path.display())
    })?;
    // The rename is durable only once the directory entry is. Reported, never
    // discarded: a lost rename is the torn state this function exists to
    // prevent.
    if let Some(parent) = path.parent() {
        sync_dir(parent).map_err(|e| e.to_string())?;
    }
    Ok(())
}
