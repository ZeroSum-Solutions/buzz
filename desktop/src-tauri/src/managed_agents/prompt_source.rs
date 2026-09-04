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
//! The mapping is read back as well as written:
//! [`prompt_source_binding_at`] answers "which file feeds this agent?" so the
//! dialog can show the stored path when it re-opens instead of making the
//! operator retype it.
//!
//! **The claim is verified on read, not maintained on write.** A binding is a
//! claim that the file's text is the agent's instructions, and any number of
//! other paths write that prompt — a hand-typed edit in the dialog, an inbound
//! kind:30175 replacement from another device, a snapshot import. Chasing all
//! of them with invalidation hooks would leave the next new path stale, so the
//! entry stores the SHA-256 of the prompt it was made from and every read
//! compares it against the definition's current prompt. A binding whose hash no
//! longer matches is reported `in_sync: false` and the dialog says so, instead
//! of claiming a file feeds an agent it no longer feeds.
//!
//! Removing a claim is always safe; adding one is not. That asymmetry is why
//! `delete_persona` drops the entry before it destroys anything, and why a
//! reload writes the entry only after the prompt is durable.
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

/// One sidecar entry: which file, and the prompt it was read into.
///
/// The digest is what makes the binding falsifiable. Without it the sidecar can
/// only say "this agent was once loaded from this file", which stays on disk
/// and keeps being rendered as fact long after another path rewrote the prompt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct StoredPromptSource {
    /// The symlink-resolved absolute path of the prompt file.
    pub path: String,
    /// SHA-256 (lowercase hex) of the prompt text this entry was written from.
    pub prompt_sha256: String,
}

/// Definition id → the file bound to it, as stored in the sidecar.
pub(crate) type PromptSourceMap = BTreeMap<String, StoredPromptSource>;

/// A stored binding resolved against the definition's current prompt.
///
/// `in_sync` is the whole point: it is `false` when the definition's prompt is
/// no longer the text the binding was made from (a typed edit, an inbound
/// replacement) and when the definition is gone altogether, so the dialog can
/// render an explicit out-of-sync state rather than a false claim.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PromptSourceBinding {
    /// The bound file's absolute path.
    pub path: String,
    /// Whether the definition's prompt still equals the bound file's text as of
    /// the last reload.
    pub in_sync: bool,
}

/// SHA-256 of a prompt, lowercase hex — the sidecar's record of what was read.
pub(crate) fn prompt_digest(prompt: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(prompt.as_bytes()))
}

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

/// The prompt file bound to `definition_id`, resolved against the definition's
/// current prompt.
///
/// `current_prompt` is the definition's stored instructions, or `None` when the
/// definition no longer exists — an orphaned entry resolves to `in_sync: false`
/// like any other stale claim, and stays visible so it can still be cleared.
///
/// `Ok(None)` means no binding is stored, which is what the dialog shows as
/// "no instructions file". A sidecar that exists but cannot be read or parsed
/// is an error rather than `None`: reporting a corrupt file as "nothing is
/// bound" would invite the operator to rebind over a mapping that is still
/// there, and the next save would drop every other definition's entry.
pub(crate) fn prompt_source_binding_at(
    store_path: &Path,
    definition_id: &str,
    current_prompt: Option<&str>,
) -> Result<Option<PromptSourceBinding>, String> {
    Ok(load_prompt_sources_at(store_path)?
        .get(definition_id)
        .map(|stored| PromptSourceBinding {
            path: stored.path.clone(),
            in_sync: current_prompt
                .is_some_and(|prompt| prompt_digest(prompt) == stored.prompt_sha256),
        }))
}

/// Move a sidecar that cannot be parsed out of the way, and report where it
/// went.
///
/// The recovery affordance for a corrupt sidecar. Every read path refuses a
/// malformed file — deliberately, so a parse failure is never mistaken for "no
/// bindings" — which leaves `Clear` unable to help, because clearing one entry
/// must first read the rest. This is the explicit way out: the file is renamed,
/// never deleted, so nothing is destroyed and the operator can still inspect it.
///
/// Refused when the sidecar parses, so it cannot be used to drop healthy
/// bindings: recovery is for the state that has no other exit.
pub(crate) fn quarantine_prompt_sources_at(store_path: &Path) -> Result<PathBuf, String> {
    if !store_path.exists() {
        return Err(
            "There are no instructions-file settings on this machine to reset.".to_string(),
        );
    }
    if load_prompt_sources_at(store_path).is_ok() {
        return Err(
            "The instructions-file settings are readable, so there is nothing to reset. Use Clear to unbind one agent."
                .to_string(),
        );
    }
    // Colons are legal in an RFC-3339 stamp and illegal in a Windows filename.
    let stamp = crate::util::now_iso().replace(':', "-");
    let quarantined = store_path.with_extension(format!("corrupt-{stamp}.json"));
    std::fs::rename(store_path, &quarantined).map_err(|error| {
        format!(
            "failed to move the unreadable prompt-sources.json aside: {error} ({})",
            quarantined.display()
        )
    })?;
    Ok(quarantined)
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
/// `entry` carries the file's path *and* the prompt text that was read from it,
/// because the entry records both: the path to reload from and the digest that
/// later reads check the definition's prompt against.
///
/// The last step of a reload, run only once the prompt is durably the
/// definition's own: a mapping is a claim that the file's text is what the
/// agent uses, and this is the first moment that claim is true.
pub(crate) fn commit_prompt_source_at(
    store_path: &Path,
    definition_id: &str,
    entry: Option<(&Path, &str)>,
) -> Result<(), String> {
    let mut sources = load_prompt_sources_at(store_path)?;

    let Some((path, prompt)) = entry else {
        if sources.remove(definition_id).is_some() {
            save_prompt_sources_at(store_path, &sources)?;
        }
        return Ok(());
    };

    let stored = path
        .to_str()
        .ok_or_else(|| format!("Prompt file path is not valid UTF-8: {}", path.display()))?
        .to_string();
    sources.insert(
        definition_id.to_string(),
        StoredPromptSource {
            path: stored,
            prompt_sha256: prompt_digest(prompt),
        },
    );
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
