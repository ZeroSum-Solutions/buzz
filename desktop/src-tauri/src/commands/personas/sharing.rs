use tauri::{AppHandle, Manager};

use crate::{
    app_state::AppState,
    managed_agents::{
        load_personas,
        retention::{mark_synced, open_retention_db},
        AgentDefinition,
    },
};

use super::pending::{prepare_persona_publication, PreparedPersonaPublication};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PersonaSharePublicationStatus {
    Published,
    Queued,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetPersonaSharedResult {
    pub persona: AgentDefinition,
    pub publication_status: PersonaSharePublicationStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay_message: Option<String>,
    /// Why the local "this head is synced" bookkeeping failed, when the relay
    /// had already accepted the event. See [`publish_prepared_persona`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bookkeeping_error: Option<String>,
}

#[tauri::command]
pub async fn set_persona_shared(
    id: String,
    shared: bool,
    app: AppHandle,
) -> Result<SetPersonaSharedResult, String> {
    let prepared = tokio::task::spawn_blocking({
        let app = app.clone();
        move || {
            let state = app.state::<AppState>();
            let _store_guard = state
                .managed_agents_store_lock
                .lock()
                .map_err(|error| error.to_string())?;
            let personas = load_personas(&app)?;
            let persona = personas
                .iter()
                .find(|record| record.id == id)
                .ok_or_else(|| format!("agent {id} not found"))?;

            if persona.is_builtin {
                return Err("Built-in agents cannot be shared to the catalog.".to_string());
            }

            // Strict path: unlike ordinary definition saves, an enqueue failure
            // for this privacy-sensitive toggle must reach the command/UI.
            prepare_persona_publication(&app, &state, persona, Some(shared))
        }
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {e}"))??;

    let state = app.state::<AppState>();
    publish_prepared_persona(&state, prepared).await
}

/// Save a persona edit AND publish its catalog head, returning the same
/// `published | queued` outcome as [`set_persona_shared`].
///
/// The "save and publish" affordance in the edit dialog promises the change
/// reaches the catalog on save. Plain `update_persona` only enqueues
/// best-effort, so the UI could not report whether the relay accepted it. This
/// takes the identical input and reuses the strict preparation path, then awaits
/// the relay exactly like the share toggle does — a rejection or an unreachable
/// relay stays durably queued for the flush loop and is reported as `queued`.
#[tauri::command]
pub async fn update_persona_and_publish(
    input: crate::managed_agents::UpdatePersonaRequest,
    app: AppHandle,
) -> Result<SetPersonaSharedResult, String> {
    let (_, prepared) =
        super::update::update_persona_with(input, app.clone(), |app, state, persona| {
            // Strict path: this command's contract is to report the publication
            // outcome, so an enqueue failure must reach the UI rather than being
            // logged and swallowed.
            let result = prepare_persona_publication(app, state, persona, None)?;
            // F2: refresh any shared 30178 heads that include this persona.
            crate::commands::refresh_team_catalog_heads_for_persona(app, state, &persona.id);
            Ok(result)
        })
        .await?;

    let state = app.state::<AppState>();
    publish_prepared_persona(&state, prepared).await
}

pub(super) async fn publish_prepared_persona(
    state: &AppState,
    prepared: PreparedPersonaPublication,
) -> Result<SetPersonaSharedResult, String> {
    let api_base_url = crate::relay::relay_http_base_url(&prepared.scope.relay_url);
    let publish_result = crate::relay::submit_signed_event_at_with_keys(
        &prepared.event,
        state,
        &api_base_url,
        &prepared.scope.owner_keys,
    )
    .await;

    match publish_result {
        Ok(_) => {
            // The relay has accepted the head, and the persona and any sidecar
            // mapping are already durable. Marking the retained row synced is
            // bookkeeping *after* that point, so its failure must not be
            // reported as the call failing: a caller that sees `Err` here would
            // tell the user nothing was applied and, in the reload's case, keep
            // showing the pre-reload text that the next Save would write back —
            // undoing a change that is live both locally and on the relay.
            //
            // Nothing is swallowed, and nothing is stranded (Review-Proven Rule
            // 1): the retained row simply stays `pending_sync`, which is the
            // durable retry record the flush loop already acts on, and the
            // reason is returned so the UI can say the local sync record did
            // not update.
            let bookkeeping_error = mark_prepared_synced(&prepared).err();
            if let Some(reason) = bookkeeping_error.as_deref() {
                eprintln!(
                    "buzz-desktop: persona head published but the sync record did not update: {reason}"
                );
            }
            Ok(SetPersonaSharedResult {
                persona: prepared.persona,
                publication_status: PersonaSharePublicationStatus::Published,
                relay_message: None,
                bookkeeping_error,
            })
        }
        Err(error) => Ok(SetPersonaSharedResult {
            persona: prepared.persona,
            publication_status: PersonaSharePublicationStatus::Queued,
            relay_message: Some(error),
            bookkeeping_error: None,
        }),
    }
}

/// Record that the relay accepted this head, so the flush loop stops retrying it.
fn mark_prepared_synced(prepared: &PreparedPersonaPublication) -> Result<(), String> {
    let conn = open_retention_db(&prepared.scope.db_path)?;
    mark_synced(
        &conn,
        prepared.retained.kind,
        &prepared.retained.pubkey,
        &prepared.retained.d_tag,
        prepared.retained.created_at,
        &prepared.retained.content,
    )
}

#[cfg(all(test, not(target_os = "windows")))]
mod tests {
    use super::*;
    use crate::{
        app_state::build_app_state,
        commands::personas::pending::prepare_persona_publication_at,
        managed_agents::{
            retention::{get_retained_event, open_retention_db, RetentionScope},
            AgentDefinition,
        },
    };
    use std::collections::BTreeMap;

    fn persona() -> AgentDefinition {
        AgentDefinition {
            description: None,
            id: "catalog-reviewer".to_string(),
            display_name: "Catalog Reviewer".to_string(),
            avatar_url: None,
            system_prompt: "Review the catalog.".to_string(),
            runtime: None,
            model: None,
            provider: None,
            name_pool: Vec::new(),
            is_builtin: false,
            is_active: true,
            shared: false,
            source_team: None,
            source_team_persona_slug: None,
            catalog_source: None,
            team_catalog_source: None,
            env_vars: BTreeMap::new(),
            respond_to: None,
            respond_to_allowlist: Vec::new(),
            parallelism: None,
            created_at: "2026-07-27T00:00:00Z".to_string(),
            updated_at: "2026-07-27T00:00:00Z".to_string(),
        }
    }

    async fn spawn_relay(accepted: bool) -> String {
        use axum::{routing::post, Router};

        let app = Router::new().route(
            "/events",
            post(move |body: String| async move {
                let event: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                serde_json::json!({
                    "event_id": event.get("id").and_then(serde_json::Value::as_str).unwrap_or(""),
                    "accepted": accepted,
                    "message": if accepted { "" } else { "policy rejection" }
                })
                .to_string()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        format!("http://{addr}")
    }

    fn prepared(
        db_path: &std::path::Path,
        relay_url: String,
        keys: nostr::Keys,
        shared_override: Option<bool>,
    ) -> PreparedPersonaPublication {
        let (event, retained, persona) =
            prepare_persona_publication_at(db_path, &keys, &persona(), shared_override).unwrap();
        PreparedPersonaPublication {
            scope: RetentionScope {
                db_path: db_path.to_path_buf(),
                relay_url,
                owner_keys: keys,
            },
            event,
            retained,
            persona,
        }
    }

    #[tokio::test]
    async fn relay_rejection_stays_durably_queued() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("retention.db");
        let keys = nostr::Keys::generate();
        let owner = keys.public_key().to_hex();
        let prepared = prepared(&db_path, spawn_relay(false).await, keys, Some(true));
        let state = build_app_state();

        let result = publish_prepared_persona(&state, prepared).await.unwrap();

        assert_eq!(
            result.publication_status,
            PersonaSharePublicationStatus::Queued
        );
        assert!(result
            .relay_message
            .as_deref()
            .is_some_and(|message| message.contains("relay rejected event")));
        assert!(
            get_retained_event(
                &open_retention_db(&db_path).unwrap(),
                buzz_core_pkg::kind::KIND_PERSONA,
                &owner,
                "catalog-reviewer"
            )
            .unwrap()
            .unwrap()
            .pending_sync
        );
    }

    #[tokio::test]
    async fn unavailable_relay_stays_durably_queued() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("retention.db");
        let keys = nostr::Keys::generate();
        let owner = keys.public_key().to_hex();
        let prepared = prepared(&db_path, relay_url, keys, Some(true));
        let state = build_app_state();

        let result = publish_prepared_persona(&state, prepared).await.unwrap();

        assert_eq!(
            result.publication_status,
            PersonaSharePublicationStatus::Queued
        );
        assert!(result
            .relay_message
            .as_deref()
            .is_some_and(|message| message.starts_with("relay unreachable:")));
        assert!(
            get_retained_event(
                &open_retention_db(&db_path).unwrap(),
                buzz_core_pkg::kind::KIND_PERSONA,
                &owner,
                "catalog-reviewer"
            )
            .unwrap()
            .unwrap()
            .pending_sync
        );
    }

    #[tokio::test]
    async fn relay_acceptance_marks_the_scoped_head_synced() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("retention.db");
        let keys = nostr::Keys::generate();
        let owner = keys.public_key().to_hex();
        let prepared = prepared(&db_path, spawn_relay(true).await, keys, Some(true));
        let state = build_app_state();

        let result = publish_prepared_persona(&state, prepared).await.unwrap();

        assert_eq!(
            result.publication_status,
            PersonaSharePublicationStatus::Published
        );
        assert!(
            !get_retained_event(
                &open_retention_db(&db_path).unwrap(),
                buzz_core_pkg::kind::KIND_PERSONA,
                &owner,
                "catalog-reviewer"
            )
            .unwrap()
            .unwrap()
            .pending_sync
        );
    }

    /// `update_persona_and_publish` differs from the share toggle in one way:
    /// it passes no share override, so the edit must keep whatever the scoped
    /// head already says, and it reports the relay outcome to the caller.
    #[tokio::test]
    async fn test_update_and_publish_acceptance_publishes_the_edit_at_the_current_share_state() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("retention.db");
        let keys = nostr::Keys::generate();
        let owner = keys.public_key().to_hex();
        // The persona is already shared in this scope.
        prepare_persona_publication_at(&db_path, &keys, &persona(), Some(true)).unwrap();
        let prepared = prepared(&db_path, spawn_relay(true).await, keys, None);
        let state = build_app_state();

        let result = publish_prepared_persona(&state, prepared).await.unwrap();

        assert_eq!(
            result.publication_status,
            PersonaSharePublicationStatus::Published
        );
        assert!(
            result.persona.shared,
            "an ordinary edit must not silently unshare the persona"
        );
        assert!(
            !get_retained_event(
                &open_retention_db(&db_path).unwrap(),
                buzz_core_pkg::kind::KIND_PERSONA,
                &owner,
                "catalog-reviewer"
            )
            .unwrap()
            .unwrap()
            .pending_sync
        );
    }

    #[tokio::test]
    async fn test_update_and_publish_relay_rejection_reports_queued_not_failure() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("retention.db");
        let keys = nostr::Keys::generate();
        let owner = keys.public_key().to_hex();
        prepare_persona_publication_at(&db_path, &keys, &persona(), Some(true)).unwrap();
        let prepared = prepared(&db_path, spawn_relay(false).await, keys, None);
        let state = build_app_state();

        let result = publish_prepared_persona(&state, prepared).await.unwrap();

        assert_eq!(
            result.publication_status,
            PersonaSharePublicationStatus::Queued
        );
        assert!(result
            .relay_message
            .as_deref()
            .is_some_and(|message| message.contains("relay rejected event")));
        assert!(
            get_retained_event(
                &open_retention_db(&db_path).unwrap(),
                buzz_core_pkg::kind::KIND_PERSONA,
                &owner,
                "catalog-reviewer"
            )
            .unwrap()
            .unwrap()
            .pending_sync,
            "the edit stays queued for the flush loop"
        );
    }

    /// The save path swallows enqueue failures (`retain_persona_pending` logs
    /// them). This command promises a publication outcome, so the strict
    /// preparation it uses must surface the failure instead.
    #[tokio::test]
    async fn test_update_and_publish_enqueue_failure_is_returned() {
        let dir = tempfile::tempdir().unwrap();
        let keys = nostr::Keys::generate();

        let error = prepare_persona_publication_at(dir.path(), &keys, &persona(), None)
            .expect_err("a directory cannot be opened as the retention database");

        assert!(error.contains("failed to open retention db"));
    }

    /// The relay has accepted the head and the persona is already on disk. If
    /// the sync bookkeeping that follows turns the whole call into `Err`, every
    /// caller reports "nothing was applied" about a change that is live both
    /// locally and on the relay — and the prompt-source dialog goes further,
    /// keeping the pre-reload text that the next Save would write back over it.
    /// The failure is reported beside the outcome instead, and the retained row
    /// is left pending so the flush loop is still the durable retry.
    #[tokio::test]
    async fn a_bookkeeping_failure_after_relay_acceptance_is_reported_not_raised() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("retention.db");
        let keys = nostr::Keys::generate();
        let owner = keys.public_key().to_hex();
        let mut prepared = prepared(&db_path, spawn_relay(true).await, keys, Some(true));

        // Inject a failure at the bookkeeping boundary only: the event is
        // prepared and retained in the real database, and only the connection
        // the mark-synced pass opens is pointed at something that cannot be one.
        let unopenable = dir.path().join("not-a-database");
        std::fs::create_dir_all(&unopenable).unwrap();
        prepared.scope.db_path = unopenable;

        let result = publish_prepared_persona(&build_app_state(), prepared)
            .await
            .expect("the relay accepted the head, so the call must not fail");

        assert_eq!(
            result.publication_status,
            PersonaSharePublicationStatus::Published,
            "the relay accepted it; that is what happened"
        );
        assert!(
            result
                .bookkeeping_error
                .as_deref()
                .is_some_and(|reason| reason.contains("retention db")),
            "the failure must be reported, not swallowed, got {:?}",
            result.bookkeeping_error
        );
        assert!(
            get_retained_event(
                &open_retention_db(&db_path).unwrap(),
                buzz_core_pkg::kind::KIND_PERSONA,
                &owner,
                "catalog-reviewer"
            )
            .unwrap()
            .unwrap()
            .pending_sync,
            "the row stays pending, so the flush loop is still the durable retry record"
        );
    }
}
