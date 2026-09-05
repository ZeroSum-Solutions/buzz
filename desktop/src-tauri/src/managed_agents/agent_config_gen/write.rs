//! Planning and performing the writes a generated config needs.
//!
//! # Ordering
//!
//! A generated config is more than one file, so the write order is chosen to
//! make every prefix a state the runtime can live in: **every pinned skill
//! first, then Claude's scoped approval list, and the MCP config file last**.
//! A skill directory with no MCP config grants the agent no new tools — it is
//! the pre-generation state plus some unreferenced documentation, and an
//! approval list naming a server no config declares is inert. The reverse order
//! would hand the agent a live server while the skills that describe how to use
//! it are still half-written.
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
//!
//! # Symlinks and permissions
//!
//! The write root is shared: `~/.buzz` is the working directory of every
//! managed agent, and every one of them can write there. So a directory another
//! process planted must never be a way out of the root, and the root itself is
//! not ours to re-permission:
//!
//! * Directories are created **one component at a time**, starting at the
//!   caller's root. A component that already exists must be a real directory —
//!   a symlink is refused, never followed — so a planted
//!   `.claude/skills/<name>` symlink cannot redirect a write outside the root.
//! * Only a directory **this module just created** gets `0o700`. A directory
//!   that already existed — the root included — keeps the permissions its owner
//!   gave it. The previous behaviour chmod-ed the parent of every write, which
//!   for `.mcp.json` is the root itself.
//! * Each file is created with `create_new` (`O_CREAT | O_EXCL`, plus
//!   `O_NOFOLLOW` on Unix), so a pre-planted symlink at the temporary path is an
//!   error rather than a write through it, and the temporary name is drawn from
//!   the OS random source rather than being derived from the pid.
//! * The rename target must be absent or a **regular file**. `rename` replaces a
//!   symlink rather than following it, but a symlink appearing where a config
//!   file belongs is refused loudly instead of being silently replaced.

use std::path::{Component, Path, PathBuf};

use super::{
    render_claude_mcp_json, render_codex_config_toml, AgentRuntimeConfigSpec, ConfigGenError,
};

/// The runtime id whose skill directory holds Claude's pinned skills.
const CLAUDE_RUNTIME_ID: &str = "claude";
/// The runtime id whose skill directory holds Codex's pinned skills.
const CODEX_RUNTIME_ID: &str = "codex";
/// The file name every skill's body is written to.
const SKILL_FILE: &str = "SKILL.md";
/// Claude's per-project local settings file, relative to the project root.
const CLAUDE_LOCAL_SETTINGS: &str = ".claude/settings.local.json";
/// The key in that file that approves named project-scoped MCP servers.
const ENABLED_MCP_KEY: &str = "enabledMcpjsonServers";

/// One file to write, and the root its directory chain is created under.
struct PlannedWrite {
    /// The directory below which this module may create directories. Never
    /// re-permissioned, and every component beneath it is checked for symlinks.
    base: PathBuf,
    /// The file to write.
    path: PathBuf,
    /// Its complete contents.
    contents: String,
}

/// The paths [`write_claude_project_config`] would write, in write order.
///
/// # Errors
///
/// Returns [`ConfigGenError::UnknownRuntime`] if the catalog has no Claude
/// skill directory, or [`ConfigGenError::Io`] if an existing
/// `.claude/settings.local.json` cannot be read or is not a JSON object.
pub fn plan_claude_paths(
    root: &Path,
    spec: &AgentRuntimeConfigSpec,
) -> Result<Vec<PathBuf>, ConfigGenError> {
    Ok(claude_plan(root, spec)?
        .into_iter()
        .map(|w| w.path)
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
        .map(|w| w.path)
        .collect())
}

/// Writes the Claude project configuration: every pinned skill under
/// `<root>/.claude/skills/<name>/SKILL.md`, then
/// `<root>/.claude/settings.local.json` naming exactly this spec's servers in
/// `enabledMcpjsonServers`, then `<root>/.mcp.json`.
///
/// A spec with no servers writes the settings file only if one is already
/// there, and then to *remove* `enabledMcpjsonServers` — a stale approval list
/// left beside a server-less `.mcp.json` would keep approving servers this
/// generation does not declare.
///
/// The approval list is not decoration. Claude does not use a project-scoped
/// MCP server until the project has approved it, so a `.mcp.json` written
/// without it leaves a non-interactive agent with no new tools and no error.
/// The list names the generated servers one by one rather than setting
/// `enableAllProjectMcpServers`, which would approve anything any other writer
/// later adds to the shared root. Other keys in an existing file are preserved.
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
) -> Result<Vec<PlannedWrite>, ConfigGenError> {
    let mut plan = skill_plan(CLAUDE_RUNTIME_ID, root, spec)?;
    let settings = root.join(CLAUDE_LOCAL_SETTINGS);
    // A spec with no servers still has to rewrite an approval list that is
    // already there: leaving a previous `enabledMcpjsonServers` live would keep
    // approving servers this spec does not declare, while `.mcp.json` below is
    // replaced with one that has none. `symlink_metadata` rather than `exists`
    // so a planted symlink is seen here and refused by the write, not skipped.
    // Nothing is created where no settings file exists.
    if !spec.servers().is_empty() || settings.symlink_metadata().is_ok() {
        let contents = render_claude_local_settings(&settings, spec)?;
        plan.push(PlannedWrite {
            base: root.to_path_buf(),
            path: settings,
            contents,
        });
    }
    plan.push(PlannedWrite {
        base: root.to_path_buf(),
        path: root.join(".mcp.json"),
        contents: render_claude_mcp_json(spec)?,
    });
    Ok(plan)
}

/// The `settings.local.json` document that approves exactly this spec's
/// servers, preserving every other key an existing file holds.
fn render_claude_local_settings(
    path: &Path,
    spec: &AgentRuntimeConfigSpec,
) -> Result<String, ConfigGenError> {
    let mut doc = match std::fs::read_to_string(path) {
        Ok(raw) if raw.trim().is_empty() => serde_json::Map::new(),
        Ok(raw) => match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(serde_json::Value::Object(map)) => map,
            Ok(_) => {
                return Err(ConfigGenError::Io {
                    path: path.display().to_string(),
                    source: "existing settings file is not a JSON object; refusing to replace it"
                        .to_string(),
                })
            }
            Err(e) => {
                return Err(ConfigGenError::Io {
                    path: path.display().to_string(),
                    source: format!("existing settings file is not valid JSON: {e}"),
                })
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::Map::new(),
        Err(e) => return Err(io_err(path, &e)),
    };
    let names: Vec<serde_json::Value> = spec
        .servers()
        .iter()
        .map(|s| serde_json::Value::String(s.name().to_string()))
        .collect();
    if names.is_empty() {
        // No servers, so no approval: the key is removed rather than left
        // holding a stale list. Every other key the file carries survives.
        doc.remove(ENABLED_MCP_KEY);
    } else {
        doc.insert(ENABLED_MCP_KEY.to_string(), serde_json::Value::Array(names));
    }
    let mut rendered =
        serde_json::to_string_pretty(&serde_json::Value::Object(doc)).map_err(|e| {
            ConfigGenError::Io {
                path: path.display().to_string(),
                source: e.to_string(),
            }
        })?;
    rendered.push('\n');
    Ok(rendered)
}

fn codex_plan(
    root: &Path,
    codex_home: &Path,
    spec: &AgentRuntimeConfigSpec,
) -> Result<Vec<PlannedWrite>, ConfigGenError> {
    let mut plan = skill_plan(CODEX_RUNTIME_ID, root, spec)?;
    plan.push(PlannedWrite {
        base: codex_home.to_path_buf(),
        path: codex_home.join("config.toml"),
        contents: render_codex_config_toml(spec)?,
    });
    Ok(plan)
}

fn skill_plan(
    runtime_id: &str,
    root: &Path,
    spec: &AgentRuntimeConfigSpec,
) -> Result<Vec<PlannedWrite>, ConfigGenError> {
    let skill_dir = super::runtime_skill_dir(runtime_id)?;
    Ok(spec
        .skills()
        .iter()
        .map(|skill| PlannedWrite {
            base: root.to_path_buf(),
            path: root.join(skill_dir).join(skill.name()).join(SKILL_FILE),
            contents: skill.body().to_string(),
        })
        .collect())
}

fn perform(plan: Vec<PlannedWrite>) -> Result<Vec<PathBuf>, ConfigGenError> {
    let mut written = Vec::with_capacity(plan.len());
    for entry in plan {
        write_file_atomic(&entry.base, &entry.path, &entry.contents)?;
        written.push(entry.path);
    }
    Ok(written)
}

fn write_file_atomic(base: &Path, path: &Path, contents: &str) -> Result<(), ConfigGenError> {
    let parent = path.parent().ok_or_else(|| ConfigGenError::Io {
        path: path.display().to_string(),
        source: "path has no parent directory".to_string(),
    })?;
    ensure_dir_chain(base, parent)?;
    refuse_non_regular_target(path)?;

    let tmp = parent.join(temp_name());
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

/// A temporary file name no other process can predict.
///
/// The previous name was `<pid>.<counter>`, which another process sharing the
/// root could compute and pre-plant. The random half comes from the OS.
///
/// Visible to the module's tests: a staging file exists only between
/// `create_new` and `rename`, so a test that walks the tree after a successful
/// write sees nothing and would stay green if a pid-derived name came back.
/// The name function is the seam that can actually be falsified.
pub(super) fn temp_name() -> String {
    let mut bytes = [0u8; 12];
    if getrandom::getrandom(&mut bytes).is_err() {
        // The OS source is unavailable. Fall back to a clock-derived value
        // rather than to a constant; `create_new` below is what turns a
        // collision into an error instead of a silent overwrite, so this only
        // costs retry-ability.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        bytes[..4].copy_from_slice(&nanos.to_le_bytes());
    }
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(".buzz-config-gen.{hex}.tmp")
}

/// Creates `dir` and every missing component between `base` and it, refusing to
/// follow a symlink and permissioning only what it creates.
fn ensure_dir_chain(base: &Path, dir: &Path) -> Result<(), ConfigGenError> {
    ensure_base_dir(base)?;
    let relative = dir.strip_prefix(base).map_err(|_| ConfigGenError::Io {
        path: dir.display().to_string(),
        source: "path is not under the write root".to_string(),
    })?;
    let mut current = base.to_path_buf();
    for component in relative.components() {
        match component {
            Component::Normal(name) => current.push(name),
            _ => {
                return Err(ConfigGenError::Io {
                    path: dir.display().to_string(),
                    source: "path holds a '.' or '..' component".to_string(),
                })
            }
        }
        match current.symlink_metadata() {
            Ok(meta) if meta.file_type().is_dir() => {}
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(refusal(
                    &current,
                    "is a symlink; refusing to write through it",
                ))
            }
            Ok(_) => return Err(refusal(&current, "exists and is not a directory")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current).map_err(|e| io_err(&current, &e))?;
                restrict_new_dir(&current)?;
            }
            Err(e) => return Err(io_err(&current, &e)),
        }
    }
    Ok(())
}

/// The caller's root: created (and restricted) if absent, otherwise accepted as
/// it is. Never re-permissioned — it belongs to whoever made it.
fn ensure_base_dir(base: &Path) -> Result<(), ConfigGenError> {
    match base.symlink_metadata() {
        Ok(meta) if meta.file_type().is_dir() => Ok(()),
        Ok(meta) if meta.file_type().is_symlink() => Err(refusal(
            base,
            "is a symlink; the write root must be a real directory",
        )),
        Ok(_) => Err(refusal(base, "is not a directory")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(base).map_err(|e| io_err(base, &e))?;
            restrict_new_dir(base)
        }
        Err(e) => Err(io_err(base, &e)),
    }
}

/// `0o700` on a directory this module just created. Never called on a directory
/// that already existed.
#[allow(clippy::unnecessary_wraps, unused_variables)]
fn restrict_new_dir(dir: &Path) -> Result<(), ConfigGenError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| io_err(dir, &e))?;
    }
    Ok(())
}

/// A rename target must be absent or a plain file. A symlink or a directory
/// where a config file belongs is someone else's, and is reported rather than
/// replaced.
fn refuse_non_regular_target(path: &Path) -> Result<(), ConfigGenError> {
    match path.symlink_metadata() {
        Ok(meta) if meta.file_type().is_file() => Ok(()),
        Ok(meta) if meta.file_type().is_symlink() => Err(refusal(
            path,
            "is a symlink; refusing to replace it with a generated file",
        )),
        Ok(_) => Err(refusal(path, "exists and is not a regular file")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err(path, &e)),
    }
}

fn write_temp(tmp: &Path, contents: &str) -> Result<(), ConfigGenError> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // create_new is already O_CREAT|O_EXCL, which fails on an existing
        // symlink. O_NOFOLLOW says so at the syscall as well.
        options.custom_flags(libc::O_NOFOLLOW);
        options.mode(0o600);
    }
    let mut file = options.open(tmp).map_err(|e| io_err(tmp, &e))?;
    file.write_all(contents.as_bytes())
        .map_err(|e| io_err(tmp, &e))?;
    file.flush().map_err(|e| io_err(tmp, &e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Redundant with the `mode` above on a fresh file, but it also pins the
        // mode when a umask-independent guarantee is what the test asserts.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| io_err(tmp, &e))?;
    }
    file.sync_all().map_err(|e| io_err(tmp, &e))?;
    Ok(())
}

fn refusal(path: &Path, reason: &str) -> ConfigGenError {
    ConfigGenError::Io {
        path: path.display().to_string(),
        source: reason.to_string(),
    }
}

fn io_err(path: &Path, error: &std::io::Error) -> ConfigGenError {
    ConfigGenError::Io {
        path: path.display().to_string(),
        source: error.to_string(),
    }
}
