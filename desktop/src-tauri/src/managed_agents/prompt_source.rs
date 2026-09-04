//! Machine-local mapping from an agent definition to a prompt file on disk.
//!
//! An operator who keeps an agent's instructions in a repo file wants to edit
//! the file and pull the new text into the definition without retyping it into
//! the dialog. The mapping (`<app-data>/agents/prompt-sources.json`, definition
//! id → absolute path) is **machine-local by design**: it is a convenience for
//! reloading, never part of the published definition. Only the prompt *text*
//! reaches the relay through the normal kind:30175 head, so a path that names a
//! private directory is never broadcast.
//!
//! **Validation posture** mirrors [`crate::managed_agents::custom_harnesses`]:
//! everything a user can type is validated at one boundary, and the loader and
//! the save path share it. A prompt file must be an absolute path that, after
//! symlink resolution, is a regular file inside the user's home directory, is
//! valid UTF-8, and is at most [`MAX_PROMPT_SOURCE_BYTES`] — the same cap
//! `validate_agent_definition_text` enforces on `system_prompt`, so a file that
//! passes here cannot be rejected later by the update path.
//!
//! Reloading is a desktop convenience, not a harness capability, so
//! `KnownAcpRuntime` carries nothing for it.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::managed_agents::{AgentDefinition, UpdatePersonaRequest};

/// Largest prompt file accepted, in bytes.
///
/// Matches `definition_validation::MAX_SYSTEM_PROMPT_BYTES` so the file gate
/// and the definition gate agree: a file this loader accepts always survives
/// `validate_agent_definition_text`.
pub(crate) const MAX_PROMPT_SOURCE_BYTES: usize = 64 * 1024;

/// Definition id → absolute prompt-file path, as stored in the sidecar.
pub(crate) type PromptSourceMap = BTreeMap<String, String>;

/// What [`prepare_prompt_source`] resolved, before anything was written.
///
/// Preparation performs no writes at all: the sidecar is only touched by
/// [`commit_prompt_source_at`], which the command calls **after** the persona
/// save succeeded. That order is what keeps the stored mapping and the
/// effective prompt in agreement when any earlier step fails.
#[derive(Debug)]
pub(crate) enum PromptSourceChange {
    /// The caller asked to unbind the definition; nothing was read.
    Cleared,
    /// The path validated and the file was read.
    Loaded {
        /// The symlink-resolved absolute path to store.
        path: PathBuf,
        /// The file's text, to be submitted as the definition's prompt.
        prompt: String,
    },
}

/// Read the sidecar at `path`. A missing file is an empty map; unreadable or
/// malformed contents are an error, never a silently empty result — a
/// swallowed parse failure would present "no prompt sources" as fact and lose
/// every stored mapping on the next save.
pub(crate) fn load_prompt_sources_at(path: &Path) -> Result<PromptSourceMap, String> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(format!("failed to read prompt-sources.json: {error}")),
    };
    serde_json::from_str(&contents)
        .map_err(|error| format!("failed to parse prompt-sources.json: {error}"))
}

/// Write the sidecar at `path` atomically.
pub(crate) fn save_prompt_sources_at(path: &Path, sources: &PromptSourceMap) -> Result<(), String> {
    let payload = serde_json::to_vec_pretty(sources)
        .map_err(|error| format!("failed to serialize prompt-sources.json: {error}"))?;
    crate::managed_agents::storage::atomic_write_json(path, &payload)
}

/// Validate a user-supplied prompt path and resolve it against `home`.
///
/// `home` is canonicalized here as well, so a caller that passes an
/// unresolved home (`/var/...` on macOS, where the real path is
/// `/private/var/...`) still gets the correct containment answer.
pub(crate) fn resolve_prompt_source_path(raw: &str, home: &Path) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("A prompt file path is required.".to_string());
    }
    let candidate = Path::new(trimmed);
    if !candidate.is_absolute() {
        return Err(format!(
            "Prompt file path must be absolute, got {trimmed:?}."
        ));
    }

    // Canonicalize resolves every symlink in the path, so containment is
    // decided on the real target rather than the name the user typed.
    let resolved = std::fs::canonicalize(candidate).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!("Prompt file not found: {trimmed}")
        } else {
            format!("Cannot read prompt file {trimmed}: {error}")
        }
    })?;

    let home = std::fs::canonicalize(home)
        .map_err(|error| format!("Cannot resolve the home directory: {error}"))?;
    if !resolved.starts_with(&home) {
        return Err(format!(
            "Prompt file must live inside your home directory ({}); {trimmed} resolves outside it.",
            home.display()
        ));
    }

    if !resolved.is_file() {
        return Err(format!("Prompt file must be a regular file: {trimmed}"));
    }

    Ok(resolved)
}

/// Read an already-resolved prompt file.
///
/// Reads through a `take`-bounded reader rather than trusting a prior
/// `metadata()` call: the size is enforced on the bytes actually read, so a
/// file that grows between the check and the read cannot slip past the cap.
pub(crate) fn read_prompt_source(path: &Path) -> Result<String, String> {
    let file = std::fs::File::open(path)
        .map_err(|error| format!("Cannot open prompt file {}: {error}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_PROMPT_SOURCE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Cannot read prompt file {}: {error}", path.display()))?;
    if bytes.len() > MAX_PROMPT_SOURCE_BYTES {
        return Err(format!(
            "Prompt file is too large (over {MAX_PROMPT_SOURCE_BYTES} bytes): {}",
            path.display()
        ));
    }
    String::from_utf8(bytes)
        .map_err(|_| format!("Prompt file is not valid UTF-8: {}", path.display()))
}

/// Validate `raw_path` and read the file it names, writing nothing.
///
/// `raw_path` `None` resolves to [`PromptSourceChange::Cleared`]. `Some`
/// validates the path and reads the file. No sidecar write happens here, so a
/// caller that fails later — definition-text validation, the persona save —
/// leaves no mapping behind that points at a prompt the agent never received.
pub(crate) fn prepare_prompt_source(
    raw_path: Option<&str>,
    home: &Path,
) -> Result<PromptSourceChange, String> {
    let Some(raw_path) = raw_path else {
        return Ok(PromptSourceChange::Cleared);
    };

    let resolved = resolve_prompt_source_path(raw_path, home)?;
    let prompt = read_prompt_source(&resolved)?;
    Ok(PromptSourceChange::Loaded {
        path: resolved,
        prompt,
    })
}

/// Store (`Some`) or remove (`None`) the mapping for `definition_id` in the
/// sidecar at `store_path`.
///
/// The last step of a reload, run only once the prompt is durably the
/// definition's own: a mapping is a claim that the file's text is what the
/// agent uses, and this is the first moment that claim is true.
pub(crate) fn commit_prompt_source_at(
    store_path: &Path,
    definition_id: &str,
    path: Option<&Path>,
) -> Result<(), String> {
    let mut sources = load_prompt_sources_at(store_path)?;

    let Some(path) = path else {
        if sources.remove(definition_id).is_some() {
            save_prompt_sources_at(store_path, &sources)?;
        }
        return Ok(());
    };

    let stored = path
        .to_str()
        .ok_or_else(|| format!("Prompt file path is not valid UTF-8: {}", path.display()))?
        .to_string();
    sources.insert(definition_id.to_string(), stored);
    save_prompt_sources_at(store_path, &sources)
}

/// The definition a reloaded prompt produces.
///
/// The single projection point for a reload: the command builds the next
/// definition here, then hands it to [`update_request_from_definition`], so the
/// published kind:30175 content and the drift hash are computed from exactly
/// the record the update path will store.
pub(crate) fn definition_with_prompt(
    definition: &AgentDefinition,
    prompt: &str,
) -> AgentDefinition {
    AgentDefinition {
        system_prompt: prompt.to_string(),
        ..definition.clone()
    }
}

/// Project a definition onto the update request the persona edit path takes.
///
/// `env_vars` and `behavior` are deliberately `None`: on
/// [`UpdatePersonaRequest`] absent means "leave the stored value alone", and a
/// prompt reload has no business rewriting either. Every other field is the
/// definition's current value, so the reload changes the prompt and nothing
/// else.
pub(crate) fn update_request_from_definition(definition: &AgentDefinition) -> UpdatePersonaRequest {
    UpdatePersonaRequest {
        id: definition.id.clone(),
        display_name: definition.display_name.clone(),
        avatar_url: definition.avatar_url.clone(),
        description: definition.description.clone(),
        system_prompt: definition.system_prompt.clone(),
        runtime: definition.runtime.clone(),
        model: definition.model.clone(),
        provider: definition.provider.clone(),
        name_pool: definition.name_pool.clone(),
        env_vars: None,
        behavior: None,
    }
}

#[cfg(test)]
mod tests;
