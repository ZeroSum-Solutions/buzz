//! `set_prompt_source_and_reload`: point an agent definition at a prompt file
//! on this machine and pull the file's text into the definition.
//!
//! The command owns no validation and no publish logic of its own. Path rules
//! live in [`crate::managed_agents::prompt_source`]; the prompt itself is
//! submitted through the ordinary persona update path
//! ([`super::update::update_persona_with`]), so the definition-text validation,
//! the kind:30175 head and the persona content hash all behave exactly as they
//! do for a hand-typed edit.

use tauri::{AppHandle, Manager};

use crate::{
    app_state::AppState,
    managed_agents::{
        load_personas,
        prompt_source::{
            definition_with_prompt, set_prompt_source_at, update_request_from_definition,
            PromptSourceChange,
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
    /// dialog shows where a typed path actually landed. Absent on a clear.
    /// Machine-local: it is never part of the published definition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// Set (or clear) the prompt file bound to `definition_id` and reload it.
///
/// `path` `None` clears the mapping and leaves the definition untouched.
/// `Some` validates the path, stores the mapping, reads the file, and submits
/// the text through the persona update path.
#[tauri::command]
pub async fn set_prompt_source_and_reload(
    definition_id: String,
    path: Option<String>,
    app: AppHandle,
) -> Result<SetPromptSourceResult, String> {
    let home = dirs::home_dir().ok_or_else(|| {
        "Cannot resolve your home directory, so a prompt file cannot be validated.".to_string()
    })?;

    // The mapping write and the request it produces happen under one hold of
    // the store lock: reading the definition in a second pass would let a
    // concurrent edit land in between and be clobbered by this request's
    // replace-everything fields.
    let request = tokio::task::spawn_blocking({
        let app = app.clone();
        move || -> Result<Option<(String, UpdatePersonaRequest)>, String> {
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
                return Err(
                    "Built-in agents cannot load their instructions from a file.".to_string(),
                );
            }
            let store_path = prompt_sources_path(&app)?;
            match set_prompt_source_at(&store_path, &definition_id, path.as_deref(), &home)? {
                PromptSourceChange::Cleared => Ok(None),
                PromptSourceChange::Loaded { prompt, path } => Ok(Some((
                    path.to_string_lossy().into_owned(),
                    update_request_from_definition(&definition_with_prompt(persona, &prompt)),
                ))),
            }
        }
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {e}"))??;

    let Some((path, request)) = request else {
        return Ok(SetPromptSourceResult {
            local_updated: false,
            publish: None,
            relay_message: None,
            path: None,
        });
    };

    submit_reloaded_prompt(app, path, request).await
}

/// Path of the machine-local prompt-source sidecar.
fn prompt_sources_path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    Ok(crate::managed_agents::managed_agents_base_dir(app)?.join("prompt-sources.json"))
}

/// Save the reloaded prompt through the persona update path and report how far
/// the kind:30175 head got.
///
/// The enqueue result is carried out of the store lock as a `Result` rather
/// than propagated: by the time the retain closure runs the definition is
/// already saved to disk, so a propagated error would report the change as not
/// applied when it was.
async fn submit_reloaded_prompt(
    app: AppHandle,
    path: String,
    request: UpdatePersonaRequest,
) -> Result<SetPromptSourceResult, String> {
    let (_, prepared): (_, Result<PreparedPersonaPublication, String>) =
        super::update::update_persona_with(request, app.clone(), |app, state, persona| {
            let prepared = prepare_persona_publication(app, state, persona, None);
            crate::commands::refresh_team_catalog_heads_for_persona(app, state, &persona.id);
            Ok(prepared)
        })
        .await?;

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
                path: Some(path),
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
        path: Some(path),
    })
}
