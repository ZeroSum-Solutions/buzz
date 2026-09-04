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
//! request's replace-everything fields.
//!
//! [`get_prompt_source`] is the read half. Without it the sidecar would be
//! write-only: the dialog would open knowing of no binding, so the operator
//! would retype the whole absolute path on every reload and would be offered
//! `Clear` with nothing to tell them whether anything is bound.

use tauri::{AppHandle, Manager};

use crate::{
    app_state::AppState,
    managed_agents::{
        load_personas,
        prompt_source::{
            commit_prompt_source_at, definition_with_prompt, prepare_prompt_source,
            prompt_source_at, update_request_from_definition, PromptSourceChange,
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
    /// The symlink-resolved absolute path now bound to the definition, so the
    /// dialog shows where a typed path actually landed. Present exactly when a
    /// mapping is stored: absent on a clear, and absent when the prompt was
    /// reloaded but the sidecar write failed (`mapping_error` says why).
    /// Machine-local: it is never part of the published definition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Why the mapping could not be stored, when the prompt itself was saved.
    /// Reported rather than swallowed: the prompt is live but the binding the
    /// user asked for is not, and only they can decide what to do about it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mapping_error: Option<String>,
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
                path: None,
                mapping_error: None,
                prompt: None,
            });
        }
        PromptSourceChange::Loaded { path, prompt } => (path, prompt),
    };

    let (expected_updated_at, request) =
        read_update_request(&app, definition_id.clone(), prompt).await?;

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
    Ok(crate::managed_agents::managed_agents_base_dir(app)?.join("prompt-sources.json"))
}

/// The prompt file bound to `definition_id` on this machine, or `None`.
///
/// The read half of the binding the dialog writes with
/// [`set_prompt_source_and_reload`]: on open the field seeds itself from this,
/// so the stored path is shown rather than retyped.
///
/// Deliberately independent of the definition itself — no existence or
/// built-in check — so a mapping whose agent was deleted is still visible and
/// therefore still clearable, matching the clear path's own behaviour.
#[tauri::command]
pub async fn get_prompt_source<R: tauri::Runtime>(
    definition_id: String,
    app: AppHandle<R>,
) -> Result<Option<String>, String> {
    let app = app.clone();
    tokio::task::spawn_blocking(move || -> Result<Option<String>, String> {
        let state = app.state::<AppState>();
        let _store_guard = state
            .managed_agents_store_lock
            .lock()
            .map_err(|error| error.to_string())?;
        prompt_source_at(&prompt_sources_path(&app)?, &definition_id)
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

    // Boundary 4 — the mapping. The prompt is durable now, so a failure here
    // cannot leave a mapping that disagrees with it; it leaves no mapping at
    // all, which is reported rather than silently dropped.
    let mapping_error = commit_mapping(&app, definition_id, source_path.clone()).await;
    let (path, mapping_error) = match mapping_error {
        Ok(()) => (Some(source_path.to_string_lossy().into_owned()), None),
        Err(error) => {
            eprintln!("buzz-desktop: prompt-source mapping write failed: {error}");
            (None, Some(error))
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
                path,
                mapping_error,
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
        path,
        mapping_error,
        prompt: Some(persona.system_prompt),
    })
}

/// Store the definition → path mapping under the store lock.
async fn commit_mapping<R: tauri::Runtime>(
    app: &AppHandle<R>,
    definition_id: String,
    source_path: std::path::PathBuf,
) -> Result<(), String> {
    let app = app.clone();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        let state = app.state::<AppState>();
        let _store_guard = state
            .managed_agents_store_lock
            .lock()
            .map_err(|error| error.to_string())?;
        commit_prompt_source_at(
            &prompt_sources_path(&app)?,
            &definition_id,
            Some(&source_path),
        )
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {e}"))?
}

#[cfg(all(test, not(target_os = "windows")))]
mod tests;
