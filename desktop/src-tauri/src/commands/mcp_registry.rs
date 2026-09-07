//! Tauri commands behind the MCP servers Settings panel and the per-agent
//! toggles.
//!
//! Every command here is a *user action*, and each one is one atomic persist
//! (AGENTS.md Review-Proven Rule 5): the registry document is rewritten whole,
//! and the configuration every agent will spawn from moves in one pointer
//! rename inside [`converge_now`]. A failure at any step leaves the previous
//! generation adopted, so agents keep running the configuration they were
//! started with rather than a half-applied one.
//!
//! Secret **values** travel one way. `save_mcp_registry_server` takes them,
//! hands them straight to the convergence, which writes them under the
//! reserved `mcp:` prefix bound to the generation it adopts. No command
//! returns one, and the DTOs below carry reference *names* only.

use std::collections::BTreeMap;

use tauri::AppHandle;

use crate::app_state::AppState;
use crate::managed_agents::mcp_registry::apply;
use crate::managed_agents::mcp_registry::apply::converge_now;
use crate::managed_agents::mcp_registry::load::{load_registry, LoadedEntry};
use crate::managed_agents::mcp_registry::schema::{
    RegistryDocument, RegistryEntry, RegistryTransport, MAX_ARGS, MAX_ARG_LEN, MAX_DOCUMENT_BYTES,
    MAX_DOCUMENT_SERVERS, MAX_ENTRY_BYTES, MAX_ENV_ENTRIES, MAX_ENV_NAME_LEN, MAX_ENV_VALUE_LEN,
    MAX_ID_LEN, MAX_NAME_LEN, MAX_SERVERS_PER_AGENT,
};
use crate::managed_agents::types::{AgentMcpServers, AGENT_MCP_SERVERS_VERSION};
use crate::managed_agents::ManagedAgentRecord;
use buzz_secret_store_pkg::{looks_like_reference, McpSecretRef};

/// One registry entry as the panel renders it.
///
/// The approve step needs the *exact* command line or URL the operator is
/// about to authorize, so it is projected verbatim. What is never projected is
/// a secret value: `env` is reduced to the variable name and the reference it
/// names, so a value cannot reach the renderer even if one were somehow
/// stored inline.
#[derive(Debug, Clone, serde::Serialize)]
pub struct McpRegistryEntryView {
    /// Stable id; the agent record's enabled list refers to this.
    pub id: String,
    /// Display name, which is also the generated config key.
    pub name: String,
    /// `"stdio"` or `"http"`.
    pub transport: String,
    /// Absolute command path for a stdio entry, else `None`.
    pub command: Option<String>,
    /// Command arguments for a stdio entry.
    pub args: Vec<String>,
    /// Upstream URL for an http entry, else `None`.
    pub url: Option<String>,
    /// Auth scheme for an http entry that declares one.
    pub auth_scheme: Option<String>,
    /// Declared environment, as `(name, reference-or-literal)` pairs. A
    /// reference is the `mcp:<id>` spelling; a literal is one the sentinel
    /// scan already cleared as non-credential.
    pub env: Vec<McpRegistryEnvView>,
    /// The loader's reason this entry is disabled, or `None` when it is
    /// usable. Rendered beside the entry, and it is the same string a spawn
    /// refuses with.
    pub rejection: Option<String>,
}

/// One declared environment entry, names only.
#[derive(Debug, Clone, serde::Serialize)]
pub struct McpRegistryEnvView {
    /// Variable name.
    pub name: String,
    /// The `mcp:<id>` reference, when this entry names one.
    pub reference: Option<String>,
    /// The literal value, when this entry carries one. Only values the
    /// sentinel scan cleared as non-credential ever reach here; a
    /// credential-shaped literal rejects the entry at load.
    pub literal: Option<String>,
}

/// What the panel loads.
#[derive(Debug, Clone, serde::Serialize)]
pub struct McpRegistryView {
    /// Every declared entry, in document order, each with its status.
    pub servers: Vec<McpRegistryEntryView>,
    /// Absolute path of the document, for the "reveal in Finder" affordance.
    pub document_path: String,
    /// Agents whose selection could not be resolved, with the message the
    /// panel shows and the spawn refuses with.
    pub refused: Vec<(String, String)>,
}

fn redact_args(args: &[String]) -> Vec<String> {
    use buzz_secret_store_pkg::sentinel::{is_credential_name, scan_value};
    let mut out = Vec::with_capacity(args.len());
    let mut redact_next = false;
    for arg in args {
        if redact_next {
            out.push("<redacted>".to_string());
            redact_next = false;
            continue;
        }
        if is_credential_name(arg.trim_start_matches('-')) && arg.starts_with('-') {
            out.push(arg.clone());
            redact_next = true;
            continue;
        }
        if scan_value(arg).is_some() {
            out.push("<redacted>".to_string());
            continue;
        }
        out.push(arg.clone());
    }
    out
}

fn view_of(loaded: &LoadedEntry) -> McpRegistryEntryView {
    let entry = &loaded.entry;
    let (transport, command, args, url, auth_scheme) = match &entry.transport {
        RegistryTransport::Stdio { command, args } => {
            // A rejected entry's `command` is redacted the same way its `env`
            // literals already are: the loader's rejection reason is not
            // necessarily "this is a credential", but the executable path
            // itself can be credential-shaped (e.g. `/tmp/sk-live-.../server`)
            // and the panel must not render what caused the refusal (Sol T7c
            // round 2, item 6).
            let (effective_command, effective_args) = if loaded.rejection.is_some() {
                ("<redacted>".to_string(), redact_args(args))
            } else {
                (command.clone(), args.clone())
            };
            ("stdio", Some(effective_command), effective_args, None, None)
        }
        RegistryTransport::Http { url, auth } => {
            // Same reasoning as the stdio command above: a rejected URL can
            // carry a query-string or userinfo credential, which is exactly
            // why it was rejected — the panel must show the rejection
            // message, not the URL that triggered it.
            let effective_url = if loaded.rejection.is_some() {
                "<redacted>".to_string()
            } else {
                url.clone()
            };
            (
                "http",
                None,
                Vec::new(),
                Some(effective_url),
                auth.as_ref().map(|auth| auth.scheme.clone()),
            )
        }
    };
    McpRegistryEntryView {
        id: entry.id.clone(),
        name: entry.name.clone(),
        transport: transport.to_string(),
        command,
        args,
        url,
        auth_scheme,
        env: entry
            .env
            .iter()
            .map(|(name, value)| {
                let is_reference = looks_like_reference(value);
                let literal = if is_reference {
                    None
                } else if loaded.rejection.is_some() {
                    Some("<redacted>".to_string())
                } else {
                    Some(value.clone())
                };
                McpRegistryEnvView {
                    name: name.clone(),
                    reference: is_reference.then(|| value.clone()),
                    literal,
                }
            })
            .collect(),
        rejection: loaded.rejection.clone(),
    }
}

/// Largest accepted number of secret values one save may carry: one per
/// possible `env` reference plus one for an HTTP entry's own auth reference.
const MAX_SECRETS_PER_SAVE: usize = MAX_ENV_ENTRIES + 1;

/// Bound the DTO before it is merged into the document.
///
/// `save_mcp_registry_server` used to validate only secret-key syntax and the
/// *old* document, then write the merged document unconditionally — an
/// oversized `args`/`env`/id/name landed on disk before `validate_entry`
/// (which only runs at load/list time) ever rejected it. This runs the same
/// bounds `validate_entry` enforces, but before the write instead of after
/// (Sol T7c round 2, item 5).
///
/// # Errors
/// A message naming the breached cap.
fn validate_prospective_entry(entry: &RegistryEntry) -> Result<(), String> {
    if entry.id.is_empty() || entry.id.len() > MAX_ID_LEN {
        return Err(format!(
            "the server id must be 1 to {MAX_ID_LEN} bytes, got {}",
            entry.id.len()
        ));
    }
    if entry.name.is_empty() || entry.name.len() > MAX_NAME_LEN {
        return Err(format!(
            "the server name must be 1 to {MAX_NAME_LEN} bytes, got {}",
            entry.name.len()
        ));
    }
    if let RegistryTransport::Stdio { args, .. } = &entry.transport {
        if args.len() > MAX_ARGS {
            return Err(format!(
                "this entry declares {} arguments, over the {MAX_ARGS} cap",
                args.len()
            ));
        }
        for arg in args {
            if arg.len() > MAX_ARG_LEN {
                return Err(format!(
                    "one argument is {} bytes, over the {MAX_ARG_LEN}-byte cap",
                    arg.len()
                ));
            }
        }
    }
    if entry.env.len() > MAX_ENV_ENTRIES {
        return Err(format!(
            "this entry declares {} env vars, over the {MAX_ENV_ENTRIES} cap",
            entry.env.len()
        ));
    }
    for (name, value) in &entry.env {
        if name.len() > MAX_ENV_NAME_LEN {
            return Err(format!(
                "env var name `{name}` is {} bytes, over the {MAX_ENV_NAME_LEN}-byte cap",
                name.len()
            ));
        }
        if value.len() > MAX_ENV_VALUE_LEN {
            return Err(format!(
                "env var `{name}`'s value is {} bytes, over the {MAX_ENV_VALUE_LEN}-byte cap",
                value.len()
            ));
        }
    }
    let serialized =
        serde_json::to_vec(entry).map_err(|e| format!("cannot serialize this entry: {e}"))?;
    if serialized.len() > MAX_ENTRY_BYTES {
        return Err(format!(
            "this entry serializes to {} bytes, over the {MAX_ENTRY_BYTES}-byte cap",
            serialized.len()
        ));
    }
    Ok(())
}

/// Bound the secrets map before it reaches convergence: an unbounded count or
/// an oversized value would otherwise be written into the secret store with
/// no cap of its own (Sol T7c round 2, item 5).
///
/// # Errors
/// A message naming the breached cap.
fn validate_secrets(secrets: &BTreeMap<String, String>) -> Result<(), String> {
    if secrets.len() > MAX_SECRETS_PER_SAVE {
        return Err(format!(
            "this save carries {} secret values, over the {MAX_SECRETS_PER_SAVE} cap",
            secrets.len()
        ));
    }
    for (id, value) in secrets {
        if value.len() > MAX_ENV_VALUE_LEN {
            return Err(format!(
                "the value for `{id}` is {} bytes, over the {MAX_ENV_VALUE_LEN}-byte cap",
                value.len()
            ));
        }
    }
    Ok(())
}

/// The registry id shape, mirrored from the loader's own `check_id` (which is
/// private to `load.rs`): lowercase letters, digits, `_` and `-`, 1 to
/// [`MAX_ID_LEN`] bytes.
fn validate_registry_id_shape(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("a selected server id is empty".to_string());
    }
    if value.len() > MAX_ID_LEN {
        return Err(format!(
            "`{value}` is {} bytes, over the {MAX_ID_LEN}-byte id cap",
            value.len()
        ));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        return Err(format!(
            "`{value}` is not a usable mcp server id: only lowercase letters, digits, `_` and \
             `-`"
        ));
    }
    Ok(())
}

/// Bound and validate an agent's requested server selection before it is ever
/// written to the agent store.
///
/// `resolve_for_agent` enforces the [`MAX_SERVERS_PER_AGENT`] cap and rejects
/// an unknown id, but only downstream at convergence — by which point
/// `set_agent_mcp_servers` had already durably written the oversized or
/// duplicate-bearing selection and merely recorded a refusal beside it. A
/// selection at or under the cap but with duplicate ids also produces
/// duplicate generated server names that `buzz-acp` only refuses at spawn.
/// This gates count, id shape, existence, and uniqueness first, so a breach
/// changes nothing (Sol T7c round 2, item 1).
///
/// # Errors
/// A message naming the breached cap, the malformed id, or the duplicate.
fn validate_enabled_selection<R: tauri::Runtime>(
    app: &AppHandle<R>,
    enabled: &[String],
) -> Result<(), String> {
    if enabled.len() > MAX_SERVERS_PER_AGENT {
        return Err(format!(
            "this selection enables {} mcp servers, over the {MAX_SERVERS_PER_AGENT} cap",
            enabled.len()
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for id in enabled {
        validate_registry_id_shape(id)?;
        if !seen.insert(id.as_str()) {
            return Err(format!("`{id}` is enabled twice in the same selection"));
        }
    }
    let path = document_path(app)?;
    let document = read_document(&path)?;
    for id in enabled {
        if !document.servers.iter().any(|entry| &entry.id == id) {
            return Err(format!(
                "no mcp server with id `{id}` is declared in the registry"
            ));
        }
    }
    Ok(())
}

fn document_path<R: tauri::Runtime>(app: &AppHandle<R>) -> Result<std::path::PathBuf, String> {
    let paths = apply::registry_paths(app)?.ok_or_else(|| {
        "this build cannot resolve the agent working directory, so mcp server settings are \
         unavailable"
            .to_string()
    })?;
    Ok(paths.document())
}

/// Read the registry document and its per-entry status.
///
/// # Errors
/// A message when the document breaches a whole-document rule (a duplicate id
/// or name, or a byte cap). A per-entry failure is not an error: the entry is
/// returned with its `rejection` string, which is the same message a spawn
/// refuses with.
#[tauri::command]
pub fn list_mcp_registry_servers<R: tauri::Runtime>(
    app: AppHandle<R>,
) -> Result<McpRegistryView, String> {
    let path = document_path(&app)?;
    let registry = load_registry(&path).map_err(|e| e.to_string())?;
    Ok(McpRegistryView {
        servers: registry.entries.iter().map(view_of).collect(),
        document_path: path.display().to_string(),
        refused: Vec::new(),
    })
}

/// Insert or replace one registry entry, then adopt a new generation.
///
/// `secrets` maps a reference id (the part after `mcp:`) to the value the
/// operator typed. It is consumed here and never read back.
///
/// The order is deliberate and every prefix of it is a consistent state: the
/// entry is validated first, so a rejected one changes nothing; the document
/// is written next, which no running agent reads; and the convergence is last,
/// because it is the single write — one pointer rename — that changes what an
/// agent will spawn with. A failure at the convergence leaves the previous
/// generation adopted and the new entry visible but unadopted, which is what
/// the panel then shows.
///
/// # Errors
/// A message when the entry is unusable, when the document cannot be written,
/// or when the convergence fails.
#[tauri::command]
pub fn save_mcp_registry_server<R: tauri::Runtime>(
    app: AppHandle<R>,
    entry: RegistryEntry,
    secrets: BTreeMap<String, String>,
) -> Result<McpRegistryView, String> {
    use tauri::Manager;
    let state = app.state::<AppState>();
    let _lock = state
        .mcp_registry_store_lock
        .lock()
        .map_err(|e| format!("cannot acquire mcp registry lock: {e}"))?;

    validate_prospective_entry(&entry)?;
    validate_secrets(&secrets)?;

    for id in secrets.keys() {
        // The reference id is operator-typed, so it is validated against the
        // same closed namespace a generated config uses. `identity` and
        // `agent:*` are refused there, which is what stops a typed reference
        // from naming a private key's blob record.
        McpSecretRef::parse(&format!("mcp:{id}"))
            .map_err(|e| format!("`{id}` is not a usable secret name: {e}"))?;
    }
    let path = document_path(&app)?;
    let mut document = read_document(&path)?;
    match document.servers.iter().position(|e| e.id == entry.id) {
        Some(index) => document.servers[index] = entry,
        None => {
            if document.servers.len() >= MAX_DOCUMENT_SERVERS {
                return Err(format!(
                    "the registry already declares {MAX_DOCUMENT_SERVERS} servers, which is the cap"
                ));
            }
            document.servers.push(entry);
        }
    }
    // The whole prospective document is checked too: every individual entry
    // can be under its own cap and still push the merged document over
    // MAX_DOCUMENT_BYTES. Checked before the write, so a breach here changes
    // nothing on disk (Sol T7c round 2, item 5).
    let prospective_bytes = serde_json::to_vec(&document)
        .map_err(|e| format!("cannot serialize the mcp registry: {e}"))?;
    if prospective_bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(format!(
            "writing this entry would make the registry {} bytes, over the \
             {MAX_DOCUMENT_BYTES}-byte cap",
            prospective_bytes.len()
        ));
    }
    write_document(&path, &document)?;
    let converged = converge_now(&app, &secrets)?;
    let mut view = list_mcp_registry_servers(app.clone())?;
    view.refused = converged.refused;
    Ok(view)
}

/// Internal implementation of delete_mcp_registry_server taking an injectable
/// save closure for testing.
pub fn delete_mcp_registry_server_internal<R: tauri::Runtime, F>(
    app: &AppHandle<R>,
    id: &str,
    save_records: F,
) -> Result<McpRegistryView, String>
where
    F: FnOnce(&[ManagedAgentRecord]) -> Result<(), String>,
{
    use tauri::Manager;

    // Agent selections are written FIRST, the registry document LAST — the
    // reverse of write order costs less to leave torn. If the document write
    // below fails after this succeeds, every agent has already dropped `id`
    // and the registry document merely still declares a server nothing
    // references: a consistent, retriable state (a retried delete just
    // removes it from the document too). The old order left agents
    // referencing an id the registry no longer declared — not retriable
    // without a manual repair (Sol T7c round 2, item 4).
    //
    // This read-modify-write is also serialized with the canonical
    // `managed_agents_store_lock` every other agent-store mutator takes, not
    // only the registry-specific `mcp_registry_store_lock` the caller holds
    // across this whole command — lock order: `mcp_registry_store_lock`
    // (outer, held by the caller), then `managed_agents_store_lock` (inner,
    // here). Two different mutexes guarding the same file let a normal agent
    // edit and this delete race each other's read-modify-write (Sol T7c
    // round 2, item 3).
    let state = app.state::<AppState>();
    let agents_lock = state
        .managed_agents_store_lock
        .lock()
        .map_err(|e| format!("cannot acquire managed agents lock: {e}"))?;
    let mut records = crate::managed_agents::load_managed_agents(app)?;
    let mut touched = false;
    for record in &mut records {
        if let Some(selection) = record.mcp_servers.as_mut() {
            let before = selection.enabled.len();
            selection.enabled.retain(|enabled| enabled != id);
            touched |= selection.enabled.len() != before;
        }
    }
    if touched {
        save_records(&records).map_err(|e| {
            format!(
                "updating agent records to drop {id} failed: {e}; the mcp registry document \
                 was not changed, so {id} is still declared and this delete can be retried"
            )
        })?;
    }
    drop(agents_lock);

    let path = document_path(app)?;
    let mut document = read_document(&path)?;
    document.servers.retain(|entry| entry.id != id);
    write_document(&path, &document)?;

    let converged = converge_now(app, &BTreeMap::new())?;
    let mut view = list_mcp_registry_servers(app.clone())?;
    view.refused = converged.refused;
    Ok(view)
}

/// Delete one registry entry, drop its id from every agent, and adopt a new
/// generation.
///
/// Agent records are written first, the registry document last: every prefix
/// of that order is consistent and retriable (see
/// [`delete_mcp_registry_server_internal`]).
///
/// # Errors
/// A message when the document or the agent store cannot be written, or when
/// the convergence fails.
#[tauri::command]
pub fn delete_mcp_registry_server<R: tauri::Runtime>(
    app: AppHandle<R>,
    id: String,
) -> Result<McpRegistryView, String> {
    use tauri::Manager;
    let state = app.state::<AppState>();
    let _lock = state
        .mcp_registry_store_lock
        .lock()
        .map_err(|e| format!("cannot acquire mcp registry lock: {e}"))?;
    delete_mcp_registry_server_internal(&app, &id, |records| {
        crate::managed_agents::save_managed_agents(&app, records)
    })
}

/// Set one agent's enabled registry servers, then adopt a new generation.
///
/// Writing the value — rather than clearing it when the list is empty — is
/// what makes memo decision 8's absent-versus-empty distinction real: an
/// operator who turns every server off leaves `Some([])`, which is a decision,
/// while a record that never reached this command keeps `None`.
///
/// # Errors
/// A message when the agent is unknown, when the store cannot be written, or
/// when the convergence fails — including the refusal an unsupported transport
/// produces, which is surfaced rather than silently dropping the entry.
#[tauri::command]
pub fn set_agent_mcp_servers<R: tauri::Runtime>(
    app: AppHandle<R>,
    pubkey: String,
    enabled: Vec<String>,
) -> Result<McpRegistryView, String> {
    use tauri::Manager;
    let state = app.state::<AppState>();
    let _lock = state
        .mcp_registry_store_lock
        .lock()
        .map_err(|e| format!("cannot acquire mcp registry lock: {e}"))?;

    validate_enabled_selection(&app, &enabled)?;

    let agents_lock = state
        .managed_agents_store_lock
        .lock()
        .map_err(|e| format!("cannot acquire managed agents lock: {e}"))?;
    let mut records = crate::managed_agents::load_managed_agents(&app)?;
    let record = records
        .iter_mut()
        .find(|record| record.pubkey == pubkey)
        .ok_or_else(|| format!("no agent with pubkey {pubkey}"))?;
    record.mcp_servers = Some(AgentMcpServers {
        version: AGENT_MCP_SERVERS_VERSION,
        enabled,
    });
    crate::managed_agents::save_managed_agents(&app, &records)?;
    drop(agents_lock);
    let converged = converge_now(&app, &BTreeMap::new())?;
    let mut view = list_mcp_registry_servers(app.clone())?;
    view.refused = converged.refused;
    Ok(view)
}

/// One agent's current selection, for the definition dialog.
///
/// # Errors
/// A message when the agent store cannot be read.
#[tauri::command]
pub fn get_agent_mcp_servers<R: tauri::Runtime>(
    app: AppHandle<R>,
    pubkey: String,
) -> Result<Option<Vec<String>>, String> {
    let records = crate::managed_agents::load_managed_agents(&app)?;
    Ok(records
        .iter()
        .find(|record| record.pubkey == pubkey)
        .and_then(|record| record.mcp_servers.as_ref())
        .map(|selection| selection.enabled.clone()))
}

fn read_document(path: &std::path::Path) -> Result<RegistryDocument, String> {
    match crate::managed_agents::mcp_registry::load::read_bounded_no_follow(path)
        .map_err(|e| e.to_string())?
    {
        Some(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
            format!(
                "the mcp registry at {} is not valid json: {e}",
                path.display()
            )
        }),
        None => Ok(RegistryDocument {
            version: 1,
            servers: Vec::new(),
        }),
    }
}

fn write_document(path: &std::path::Path, document: &RegistryDocument) -> Result<(), String> {
    let body = serde_json::to_vec_pretty(document)
        .map_err(|e| format!("cannot serialize the mcp registry: {e}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    // Deliberately NOT `atomic_write_json`: that helper canonicalizes `path`
    // first, which is symlink-*preserving* by design for the agent store
    // (`storage.rs`'s own doc comment). For the registry document that same
    // canonicalize-then-write is a redirection hazard — something replacing
    // `path` with a symlink between this command's earlier read and this
    // write would have its write silently follow the link and land on
    // whatever it points at. This writer refuses instead: the temp file is
    // written and renamed at `path` itself with no canonicalization, and the
    // rename is preceded by a no-follow check as close to it as this API
    // allows (Sol T7c round 2, item 2).
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(format!(
                "refusing to write {}: a symbolic link now occupies the mcp registry \
                 document's path",
                path.display()
            ));
        }
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &body).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!(
                "refusing to write {}: a symbolic link now occupies the mcp registry \
                 document's path",
                path.display()
            ));
        }
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot rename {}: {e}", path.display()))
}

#[cfg(test)]
#[path = "mcp_registry_tests.rs"]
mod tests;
