//! `set_prompt_source_and_reload`: point an agent definition at a prompt file
//! on this machine and pull the file's text into the definition.
//!
//! The command owns no validation and no publish logic of its own. Path rules
//! live in [`crate::managed_agents::prompt_source`]; the prompt itself is
//! submitted through the ordinary persona update path
//! ([`super::update::update_persona_with_precondition`]), so the
//! definition-text validation, the kind:30175 head and the persona content hash
//! all behave exactly as they do for a hand-typed edit.
//!
//! **Order is load-bearing**: validate, read, persona save, *then* the mapping.
//! The sidecar mapping is a claim that the file's text is what the agent uses,
//! so it is written only once that claim is true. A failure at any earlier
//! boundary — a path that does not resolve, a file the definition validator
//! refuses (CRLF, control characters), a persona save that does not land —
//! leaves the sidecar untouched, so the stored mapping and the effective prompt
//! always agree.
//!
//! The definition is read in one pass and saved in another, and the store lock
//! cannot be held across both (the update path takes it itself). The read pass
//! therefore captures the definition's `updated_at` and the save refuses if it
//! moved, so a concurrent edit is reported rather than clobbered by this
//! request's replace-everything fields. The mapping write is a third hold, and
//! the precondition cannot cover it — this reload's save has already
//! committed — so the binding it reports is resolved against the store's
//! *current* prompt rather than asserted, which is what keeps this response and
//! the next [`get_prompt_source`] from disagreeing.
//!
//! [`get_prompt_source`] is the read half. Without it the sidecar would be
//! write-only: the dialog would open knowing of no binding, so the operator
//! would retype the whole absolute path on every reload and would be offered
//! `Clear` with nothing to tell them whether anything is bound. It resolves the
//! entry against the definition's current prompt, so a binding another write
//! path has invalidated reads back as out of sync instead of as fact.
//!
//! [`reset_prompt_sources`] is the recovery half. Every read refuses a sidecar
//! that cannot be parsed, which leaves `Clear` — itself a read-modify-write —
//! unable to help; this moves the unreadable file aside so the operator is not
//! stranded.

use tauri::{AppHandle, Manager};

use crate::{
    app_state::AppState,
    managed_agents::{
        load_personas,
        prompt_source::{
            commit_prompt_source_at, definition_with_prompt, prepare_prompt_source,
            prompt_source_binding_at, prompt_sources_store_path, quarantine_prompt_sources_at,
            update_request_from_definition, PromptSourceBinding, PromptSourceChange,
        },
        UpdatePersonaRequest,
    },
};

use super::pending::{prepare_persona_publication, PreparedPersonaPublication};
use super::sharing::{publish_prepared_persona, PersonaSharePublicationStatus};

/// Outcome of [`set_prompt_source_and_reload`].
///
/// `publish` is a string rather than the two-state
/// [`PersonaSharePublicationStatus`] because the reload has a third outcome the
/// share toggle does not: the local record can be saved and the durable enqueue
/// still fail. Reporting that as `queued` would claim a retry exists when none
/// was recorded, so it is reported as `failed:<reason>` instead. `publish` is
/// absent when nothing was submitted (the clear path).
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetPromptSourceResult {
    /// Whether the definition's stored prompt was updated on this machine.
    pub local_updated: bool,
    /// `"published"`, `"queued"`, or `"failed:<reason>"`; absent on a clear.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publish: Option<String>,
    /// The relay's message when the head could not be published.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay_message: Option<String>,
    /// The binding stored for this definition **after** the attempt, resolved
    /// against the prompt the definition now holds. Absent on a clear, and
    /// absent when nothing is stored at all.
    ///
    /// On a failed sidecar write this is the binding that survived, not
    /// `None`: the write is atomic, so an earlier binding is still on disk and
    /// still what the next `Reload` would restore. Reporting `None` there would
    /// tell the dialog nothing is bound while the sidecar says otherwise, and
    /// the claim would come back — stale — on the next open. It is reported
    /// with `in_sync: false`, because the prompt that just landed did not come
    /// from that file. Machine-local: never part of the published definition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binding: Option<PromptSourceBinding>,
    /// Why the mapping could not be stored, when the prompt itself was saved.
    /// Reported rather than swallowed: the prompt is live but the binding the
    /// user asked for is not, and only they can decide what to do about it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mapping_error: Option<String>,
    /// Why the local sync bookkeeping failed after the relay accepted the head.
    /// The publish landed and the retained row simply stays pending, so the
    /// flush loop republishes it; reporting the failure as an error would undo
    /// a change that is already live locally and on the relay.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bookkeeping_error: Option<String>,
    /// The definition's stored instructions after the reload, so the open
    /// dialog can show the file's text instead of the draft it replaced.
    /// Absent on a clear.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

/// Set (or clear) the prompt file bound to `definition_id` and reload it.
///
/// `path` `None` clears the mapping and leaves the definition untouched.
/// `Some` validates the path, reads the file, saves the text through the
/// persona update path, and only then stores the mapping.
#[tauri::command]
pub async fn set_prompt_source_and_reload<R: tauri::Runtime>(
    definition_id: String,
    path: Option<String>,
    app: AppHandle<R>,
) -> Result<SetPromptSourceResult, String> {
    let home = dirs::home_dir().ok_or_else(|| {
        "Cannot resolve your home directory, so a prompt file cannot be validated.".to_string()
    })?;

    // Boundaries 1 and 2 — validate and read. Neither writes anything.
    let prepared = tokio::task::spawn_blocking({
        let path = path.clone();
        move || prepare_prompt_source(path.as_deref(), &home)
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {e}"))??;

    let (source_path, prompt) = match prepared {
        PromptSourceChange::Cleared => {
            // Clearing touches no definition, so it needs no persona pass: the
            // effective prompt stays exactly as the last reload left it, and
            // removing the mapping only drops the claim that a file feeds it.
            // Deliberately independent of the definition's existence, so a
            // mapping whose agent or file is gone can still be removed.
            let app = app.clone();
            tokio::task::spawn_blocking(move || -> Result<(), String> {
                let state = app.state::<AppState>();
                let _store_guard = state
                    .managed_agents_store_lock
                    .lock()
                    .map_err(|error| error.to_string())?;
                commit_prompt_source_at(&prompt_sources_path(&app)?, &definition_id, None)
            })
            .await
            .map_err(|e| format!("spawn_blocking failed: {e}"))??;
            return Ok(SetPromptSourceResult {
                local_updated: false,
                publish: None,
                relay_message: None,
                binding: None,
                mapping_error: None,
                bookkeeping_error: None,
                prompt: None,
            });
        }
        PromptSourceChange::Loaded { path, prompt } => (path, prompt),
    };

    let (expected_updated_at, request) =
        read_update_request(&app, definition_id.clone(), prompt.clone()).await?;

    // The window this command cannot lock across. `update_persona_with_precondition`
    // takes the store lock itself, so the read pass above and the save pass below
    // are two separate holds and a concurrent save can land between them. Tests
    // put a writer in exactly this window; in production the hook is not compiled.
    #[cfg(all(test, not(target_os = "windows")))]
    tests::run_read_save_barrier(&definition_id);

    submit_reloaded_prompt(
        app,
        definition_id,
        source_path,
        expected_updated_at,
        request,
    )
    .await
}

/// Read the definition under the store lock and project it onto the update
/// request that carries the reloaded prompt.
///
/// The returned request's `id` and the captured `updated_at` are the optimistic
/// precondition the save is checked against.
async fn read_update_request<R: tauri::Runtime>(
    app: &AppHandle<R>,
    definition_id: String,
    prompt: String,
) -> Result<(String, UpdatePersonaRequest), String> {
    let app = app.clone();
    tokio::task::spawn_blocking(move || -> Result<(String, UpdatePersonaRequest), String> {
        let state = app.state::<AppState>();
        let _store_guard = state
            .managed_agents_store_lock
            .lock()
            .map_err(|error| error.to_string())?;
        let personas = load_personas(&app)?;
        let persona = personas
            .iter()
            .find(|record| record.id == definition_id)
            .ok_or_else(|| format!("agent {definition_id} not found"))?;
        if persona.is_builtin {
            return Err("Built-in agents cannot load their instructions from a file.".to_string());
        }
        Ok((
            persona.updated_at.clone(),
            update_request_from_definition(&definition_with_prompt(persona, &prompt)),
        ))
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {e}"))?
}

/// Path of the machine-local prompt-source sidecar.
fn prompt_sources_path<R: tauri::Runtime>(
    app: &AppHandle<R>,
) -> Result<std::path::PathBuf, String> {
    prompt_sources_store_path(app)
}

/// The prompt file bound to `definition_id` on this machine, or `None`.
///
/// The read half of the binding the dialog writes with
/// [`set_prompt_source_and_reload`]: on open the field seeds itself from this,
/// so the stored path is shown rather than retyped.
///
/// The entry is resolved against the definition's **current** prompt, so a
/// binding that some other write path has invalidated — a hand-typed edit, an
/// inbound kind:30175 replacement, a snapshot import — reads back with
/// `in_sync: false` instead of being rendered as fact. That check is what keeps
/// the claim honest without a hook in every one of those paths.
///
/// Deliberately independent of the definition's existence and built-in flag: a
/// mapping whose agent was deleted is still visible (out of sync, because there
/// is no prompt to match) and therefore still clearable, matching the clear
/// path's own behaviour.
#[tauri::command]
pub async fn get_prompt_source<R: tauri::Runtime>(
    definition_id: String,
    app: AppHandle<R>,
) -> Result<Option<PromptSourceBinding>, String> {
    let app = app.clone();
    tokio::task::spawn_blocking(move || -> Result<Option<PromptSourceBinding>, String> {
        let state = app.state::<AppState>();
        let _store_guard = state
            .managed_agents_store_lock
            .lock()
            .map_err(|error| error.to_string())?;
        let current_prompt = load_personas(&app)?
            .into_iter()
            .find(|record| record.id == definition_id)
            .map(|record| record.system_prompt);
        prompt_source_binding_at(
            &prompt_sources_path(&app)?,
            &definition_id,
            current_prompt.as_deref(),
        )
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {e}"))?
}

/// Move an unreadable prompt-sources sidecar aside, and report where it went.
///
/// The recovery path for the one state the ordinary controls cannot leave.
/// `Clear` is a read-modify-write of the sidecar, so on a malformed file it
/// fails exactly as the seed did; without this the operator has a field that
/// reports an error and two buttons that cannot fix it.
///
/// **Every agent's binding is affected** — the whole file moves — so the dialog
/// says so before offering the action, and the file is renamed rather than
/// deleted. Refused when the sidecar parses, so it can never be used to drop
/// working bindings.
#[tauri::command]
pub async fn reset_prompt_sources<R: tauri::Runtime>(app: AppHandle<R>) -> Result<String, String> {
    let app = app.clone();
    tokio::task::spawn_blocking(move || -> Result<String, String> {
        let state = app.state::<AppState>();
        let _store_guard = state
            .managed_agents_store_lock
            .lock()
            .map_err(|error| error.to_string())?;
        Ok(quarantine_prompt_sources_at(&prompt_sources_path(&app)?)?
            .to_string_lossy()
            .into_owned())
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {e}"))?
}

/// Save the reloaded prompt through the persona update path, store the mapping,
/// and report how far the kind:30175 head got.
///
/// The enqueue result is carried out of the store lock as a `Result` rather
/// than propagated: by the time the retain closure runs the definition is
/// already saved to disk, so a propagated error would report the change as not
/// applied when it was.
async fn submit_reloaded_prompt<R: tauri::Runtime>(
    app: AppHandle<R>,
    definition_id: String,
    source_path: std::path::PathBuf,
    expected_updated_at: String,
    request: UpdatePersonaRequest,
) -> Result<SetPromptSourceResult, String> {
    // Boundary 3 — the persona save. A rejected definition text (CRLF, control
    // characters, an over-long prompt) fails here, before any mapping exists.
    let (persona, prepared): (_, Result<PreparedPersonaPublication, String>) =
        super::update::update_persona_with_precondition(
            request,
            Some(expected_updated_at),
            app.clone(),
            |app, state, persona| {
                let prepared = prepare_persona_publication(app, state, persona, None);
                crate::commands::refresh_team_catalog_heads_for_persona(app, state, &persona.id);
                Ok(prepared)
            },
        )
        .await?;

    // The second window this command cannot lock across. The persona save above
    // released the store lock before `commit_mapping` retakes it, so another
    // writer — a second dialog, an inbound kind:30175 replacement — can land in
    // between and make the effective prompt something this reload never read.
    // Tests put a writer in exactly this window; in production the hook is not
    // compiled.
    #[cfg(all(test, not(target_os = "windows")))]
    tests::run_save_mapping_barrier(&definition_id);

    // Boundary 4 — the mapping. The prompt is durable now, so this write can
    // only add a claim that is already true. A failure here is reported rather
    // than swallowed, together with whatever binding survived on disk: the
    // sidecar write is atomic, so a previous binding is still there and still
    // what the next Reload would restore, and hiding it would let the stale
    // claim reappear on the next open with nothing to explain it.
    let (binding, mapping_error) = match commit_mapping(
        &app,
        definition_id.clone(),
        source_path.clone(),
        &persona.system_prompt,
    )
    .await
    {
        Ok(binding) => (binding, None),
        Err(error) => {
            eprintln!("buzz-desktop: prompt-source mapping write failed: {error}");
            let surviving = surviving_binding(&app, &definition_id).await;
            (surviving, Some(error))
        }
    };

    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(reason) => {
            // The prompt is on disk; only the relay head is missing. Say so
            // rather than reporting a queued retry that was never recorded.
            eprintln!("buzz-desktop: prompt-source retain failed: {reason}");
            return Ok(SetPromptSourceResult {
                local_updated: true,
                publish: Some(format!("failed:{reason}")),
                relay_message: None,
                binding,
                mapping_error,
                bookkeeping_error: None,
                prompt: Some(persona.system_prompt),
            });
        }
    };

    let state = app.state::<AppState>();
    let published = publish_prepared_persona(&state, prepared).await?;
    Ok(SetPromptSourceResult {
        local_updated: true,
        publish: Some(
            match published.publication_status {
                PersonaSharePublicationStatus::Published => "published",
                PersonaSharePublicationStatus::Queued => "queued",
            }
            .to_string(),
        ),
        relay_message: published.relay_message,
        binding,
        mapping_error,
        bookkeeping_error: published.bookkeeping_error,
        prompt: Some(persona.system_prompt),
    })
}

/// The binding still on disk after a failed mapping write, resolved against the
/// prompt the persona store holds **now**. Best-effort: if the sidecar cannot be
/// read either, the `mapping_error` already carries the failure and the dialog
/// shows no binding.
///
/// One blocking pass under `managed_agents_store_lock`, exactly as
/// [`commit_mapping`] resolves the success case — and for the same reason. The
/// prompt this reload submitted is not the store's current value once another
/// writer lands, so comparing the sidecar's digest against it would report an
/// `in_sync` the next [`get_prompt_source`] contradicts. Holding the lock across
/// the persona read and the sidecar read makes the two answers agree.
async fn surviving_binding<R: tauri::Runtime>(
    app: &AppHandle<R>,
    definition_id: &str,
) -> Option<PromptSourceBinding> {
    let app = app.clone();
    let definition_id = definition_id.to_string();
    tokio::task::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let _store_guard = state.managed_agents_store_lock.lock().ok()?;
        let current_prompt = load_personas(&app)
            .ok()?
            .into_iter()
            .find(|record| record.id == definition_id)
            .map(|record| record.system_prompt);
        prompt_source_binding_at(
            &prompt_sources_path(&app).ok()?,
            &definition_id,
            current_prompt.as_deref(),
        )
        .ok()
        .flatten()
    })
    .await
    .ok()
    .flatten()
}

/// Store the definition → path mapping under the store lock, and report the
/// binding that write produced.
///
/// The binding is resolved inside the same lock hold, against the prompt the
/// persona store holds **now** — not against the prompt this reload submitted.
/// The two differ whenever another writer landed between the persona save and
/// this write, and the sidecar records the digest of the text that was read
/// from the file, so a hard-coded `in_sync: true` would tell the dialog the
/// file feeds the agent while the effective prompt is somebody else's. Reading
/// the store here is the same check `get_prompt_source` makes on the next open,
/// so the answer this call returns and the answer the next open returns agree.
async fn commit_mapping<R: tauri::Runtime>(
    app: &AppHandle<R>,
    definition_id: String,
    source_path: std::path::PathBuf,
    prompt: &str,
) -> Result<Option<PromptSourceBinding>, String> {
    let app = app.clone();
    let prompt = prompt.to_string();
    tokio::task::spawn_blocking(move || -> Result<Option<PromptSourceBinding>, String> {
        let state = app.state::<AppState>();
        let _store_guard = state
            .managed_agents_store_lock
            .lock()
            .map_err(|error| error.to_string())?;
        let store_path = prompt_sources_path(&app)?;
        commit_prompt_source_at(
            &store_path,
            &definition_id,
            Some((source_path.as_path(), prompt.as_str())),
        )?;
        let current_prompt = load_personas(&app)?
            .into_iter()
            .find(|record| record.id == definition_id)
            .map(|record| record.system_prompt);
        prompt_source_binding_at(&store_path, &definition_id, current_prompt.as_deref())
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {e}"))?
}

#[cfg(all(test, not(target_os = "windows")))]
mod tests;
