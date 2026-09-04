//! Planning and performing the writes a generated config needs.
//!
//! # Ordering
//!
//! A generated config is more than one file, so the write order is chosen to
//! make every prefix a state the runtime can live in: **every pinned skill
//! first, the MCP config file last**. A skill directory with no MCP config
//! grants the agent no new tools — it is the pre-generation state plus some
//! unreferenced documentation. The reverse order would hand the agent a live
//! server while the skills that describe how to use it are still half-written.
//!
//! Each individual file is written to a temporary file in its own directory,
//! flushed, and renamed into place, so no reader ever sees a partial file and a
//! failure part-way through leaves earlier files complete.
//!
//! # Roots
//!
//! Every path is derived from a root the caller passes in. Nothing here reads
//! `$HOME`, `CLAUDE_CONFIG_DIR` or `CODEX_HOME`, so the operator's own
//! `~/.claude.json` and `~/.codex` cannot be reached from this module.
//! [`plan_claude_paths`] and [`plan_codex_paths`] return exactly the set the
//! matching write would touch, in write order.

use std::path::{Path, PathBuf};

use super::{
    render_claude_mcp_json, render_codex_config_toml, AgentRuntimeConfigSpec, ConfigGenError,
};

/// The runtime id whose skill directory holds Claude's pinned skills.
const CLAUDE_RUNTIME_ID: &str = "claude";
/// The runtime id whose skill directory holds Codex's pinned skills.
const CODEX_RUNTIME_ID: &str = "codex";
/// The file name every skill's body is written to.
const SKILL_FILE: &str = "SKILL.md";

/// The paths [`write_claude_project_config`] would write, in write order.
///
/// # Errors
///
/// Returns [`ConfigGenError::UnknownRuntime`] if the catalog has no Claude
/// skill directory.
pub fn plan_claude_paths(
    root: &Path,
    spec: &AgentRuntimeConfigSpec,
) -> Result<Vec<PathBuf>, ConfigGenError> {
    Ok(claude_plan(root, spec)?
        .into_iter()
        .map(|(p, _)| p)
        .collect())
}

/// The paths [`write_codex_config`] would write, in write order.
///
/// # Errors
///
/// Returns [`ConfigGenError::UnknownRuntime`] if the catalog has no Codex skill
/// directory.
pub fn plan_codex_paths(
    root: &Path,
    codex_home: &Path,
    spec: &AgentRuntimeConfigSpec,
) -> Result<Vec<PathBuf>, ConfigGenError> {
    Ok(codex_plan(root, codex_home, spec)?
        .into_iter()
        .map(|(p, _)| p)
        .collect())
}

/// Writes the Claude project configuration: every pinned skill under
/// `<root>/.claude/skills/<name>/SKILL.md`, then `<root>/.mcp.json`.
///
/// `root` is the directory the agent is spawned in — see
/// [`super::claude_project_config_root`].
///
/// Returns the paths written, in write order.
///
/// # Errors
///
/// Returns [`ConfigGenError::Io`] naming the path that failed, leaving every
/// file written before it complete and the MCP config absent.
pub fn write_claude_project_config(
    root: &Path,
    spec: &AgentRuntimeConfigSpec,
) -> Result<Vec<PathBuf>, ConfigGenError> {
    perform(claude_plan(root, spec)?)
}

/// Writes the Codex configuration: every pinned skill under
/// `<root>/.codex/skills/<name>/SKILL.md`, then `<codex_home>/config.toml`.
///
/// `codex_home` is the directory a spawned agent would be given as
/// `CODEX_HOME`. This generator does not set that variable on any spawn: a
/// per-agent `CODEX_HOME` starts a fresh keychain namespace and can leave the
/// Codex CLI logged out, and that behavior has not been proven for a
/// Dock-launched agent yet.
///
/// Returns the paths written, in write order.
///
/// # Errors
///
/// Returns [`ConfigGenError::Io`] naming the path that failed, leaving every
/// file written before it complete and the config file absent.
pub fn write_codex_config(
    root: &Path,
    codex_home: &Path,
    spec: &AgentRuntimeConfigSpec,
) -> Result<Vec<PathBuf>, ConfigGenError> {
    perform(codex_plan(root, codex_home, spec)?)
}

fn claude_plan(
    root: &Path,
    spec: &AgentRuntimeConfigSpec,
) -> Result<Vec<(PathBuf, String)>, ConfigGenError> {
    let mut plan = skill_plan(CLAUDE_RUNTIME_ID, root, spec)?;
    plan.push((root.join(".mcp.json"), render_claude_mcp_json(spec)?));
    Ok(plan)
}

fn codex_plan(
    root: &Path,
    codex_home: &Path,
    spec: &AgentRuntimeConfigSpec,
) -> Result<Vec<(PathBuf, String)>, ConfigGenError> {
    let mut plan = skill_plan(CODEX_RUNTIME_ID, root, spec)?;
    plan.push((
        codex_home.join("config.toml"),
        render_codex_config_toml(spec)?,
    ));
    Ok(plan)
}

fn skill_plan(
    runtime_id: &str,
    root: &Path,
    spec: &AgentRuntimeConfigSpec,
) -> Result<Vec<(PathBuf, String)>, ConfigGenError> {
    let skill_dir = super::runtime_skill_dir(runtime_id)?;
    Ok(spec
        .skills()
        .iter()
        .map(|skill| {
            (
                root.join(skill_dir).join(skill.name()).join(SKILL_FILE),
                skill.body().to_string(),
            )
        })
        .collect())
}

fn perform(plan: Vec<(PathBuf, String)>) -> Result<Vec<PathBuf>, ConfigGenError> {
    let mut written = Vec::with_capacity(plan.len());
    for (path, contents) in plan {
        write_file_atomic(&path, &contents)?;
        written.push(path);
    }
    Ok(written)
}

/// A per-process counter so two concurrent writers in the same directory never
/// pick the same temporary name.
static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn write_file_atomic(path: &Path, contents: &str) -> Result<(), ConfigGenError> {
    let parent = path.parent().ok_or_else(|| ConfigGenError::Io {
        path: path.display().to_string(),
        source: "path has no parent directory".to_string(),
    })?;
    std::fs::create_dir_all(parent).map_err(|e| io_err(parent, &e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| io_err(parent, &e))?;
    }

    let seq = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = parent.join(format!(".buzz-config-gen.{}.{seq}.tmp", std::process::id()));
    if let Err(e) = write_temp(&tmp, contents) {
        // Best-effort cleanup of the partial file; the real failure is returned.
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(io_err(path, &e));
    }
    Ok(())
}

fn write_temp(tmp: &Path, contents: &str) -> Result<(), ConfigGenError> {
    use std::io::Write;
    let mut file = std::fs::File::create(tmp).map_err(|e| io_err(tmp, &e))?;
    file.write_all(contents.as_bytes())
        .map_err(|e| io_err(tmp, &e))?;
    file.flush().map_err(|e| io_err(tmp, &e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| io_err(tmp, &e))?;
    }
    file.sync_all().map_err(|e| io_err(tmp, &e))?;
    Ok(())
}

fn io_err(path: &Path, error: &std::io::Error) -> ConfigGenError {
    ConfigGenError::Io {
        path: path.display().to_string(),
        source: error.to_string(),
    }
}
