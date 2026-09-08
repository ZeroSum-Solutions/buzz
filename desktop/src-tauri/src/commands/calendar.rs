//! Personal Calendar commands. All provider work runs off the UI thread.
use crate::app_state::{keyring_service, AppState};
use crate::google_calendar::{
    self as calendar,
    binding::{
        self, Binding, Change, CommitContext, EnvelopeStore, JournalStep, KeychainEnvelopes,
    },
    cache::{Cache, Snapshot},
    client::{self, Clock, SystemClock},
    dto::{CalendarEvent, EventTime},
    oauth,
    provider::{Config, Provider},
    redact::Redacted,
    revocation::RevocationState,
};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use tauri::{AppHandle, Manager};
use tauri_plugin_opener::OpenerExt;

static OPERATIONS: Mutex<()> = Mutex::new(());
fn now() -> i64 {
    SystemClock.now_ms()
}
fn identity(app: &AppHandle) -> Result<String, String> {
    Ok(app
        .state::<AppState>()
        .signing_keys()?
        .public_key()
        .to_hex())
}
fn envelopes() -> KeychainEnvelopes<'static> {
    KeychainEnvelopes::new(crate::secret_store::SecretStore::shared(keyring_service()))
}
fn cache(app: &AppHandle) -> Result<Cache, String> {
    Cache::open(
        &app.path()
            .app_cache_dir()
            .map_err(|_| "calendar cache path unavailable")?
            .join("personal-calendar.sqlite"),
    )
}
fn context(identity: &str) -> CommitContext {
    CommitContext {
        current_identity_pubkey_hex: Some(identity.to_string()),
        now_ms: now(),
    }
}
fn ensure_identity(app: &AppHandle, expected: &str) -> Result<(), String> {
    if identity(app)? != expected {
        return Err("Buzz identity changed during calendar operation".into());
    }
    Ok(())
}
fn active(identity: &str, generation: Option<u64>) -> Result<Binding, String> {
    let binding = envelopes()
        .read(&binding::envelope_key(identity))
        .map_err(|e| e.to_string())?
        .active_binding
        .ok_or("Connect Google Calendar first")?;
    if binding.identity_pubkey_hex != identity
        || generation.is_some_and(|g| g != binding.generation)
    {
        return Err("Calendar connection changed; refresh before trying again".into());
    }
    Ok(binding)
}
#[derive(Serialize)]
pub struct Status {
    revocations: Vec<RevocationView>,
    configured: bool,
    connected: bool,
    email: Option<String>,
    generation: Option<u64>,
    pending_revocations: usize,
    error: Option<String>,
}
#[derive(Serialize)]
pub struct RevocationView {
    generation: u64,
    state: &'static str,
    purge_confirmed: bool,
}
fn status(app: &AppHandle, expected_identity: &str) -> Result<Status, String> {
    let configuration = Config::load();
    let configured = configuration.is_ok();
    let config_error = configuration.err();
    let state = app.state::<AppState>();
    let _identity = state
        .identity_mutation
        .lock()
        .map_err(|_| "identity lock unavailable")?;
    ensure_identity(app, expected_identity)?;
    let id = identity(app)?;
    let envelope = envelopes()
        .read(&binding::envelope_key(&id))
        .map_err(|e| e.to_string())?;
    let error = if config_error.is_some() {
        config_error
    } else if envelope
        .pending
        .values()
        .any(|p| p.state == RevocationState::Unconfirmed)
    {
        Some("A Google revocation remains unconfirmed".into())
    } else if envelope
        .pending
        .values()
        .any(|entry| entry.state == RevocationState::Retryable || !entry.purge_confirmed)
    {
        Some("Google disconnect cleanup is pending and will retry".into())
    } else if !envelope.pending.is_empty() {
        Some("Google revocation retries were stopped; provider revocation is unconfirmed".into())
    } else {
        None
    };
    Ok(Status {
        revocations: envelope
            .pending
            .values()
            .map(|entry| RevocationView {
                generation: entry.generation,
                state: match entry.state {
                    RevocationState::Retryable => "retryable",
                    RevocationState::Unconfirmed => "revocation_unconfirmed",
                    RevocationState::Abandoned => "abandoned",
                },
                purge_confirmed: entry.purge_confirmed,
            })
            .collect(),
        configured,
        connected: envelope.active_binding.is_some(),
        email: envelope.active_binding.as_ref().map(|b| b.email.clone()),
        generation: envelope.active_binding.map(|b| b.generation),
        pending_revocations: envelope.pending.len(),
        error,
    })
}
fn retry_revocations(app: &AppHandle, id: &str) -> Result<(), String> {
    let store = envelopes();
    let key = binding::envelope_key(id);
    let provider = Provider::new()?;
    for original in store
        .read(&key)
        .map_err(|e| e.to_string())?
        .pending
        .values()
    {
        let mut entry = original.clone();
        if !entry.purge_confirmed {
            cache(app)?.purge(id)?;
            store
                .commit(
                    &key,
                    Change::JournalProgress {
                        generation: entry.generation,
                        expected_revision: entry.revision,
                        step: JournalStep::PurgeConfirmed,
                    },
                    &context(id),
                )
                .map_err(|e| e.to_string())?;
            entry.revision += 1;
        }
        if entry.state == RevocationState::Retryable
            && !entry.revocation_confirmed
            && entry.next_attempt_at_ms <= now()
        {
            let response = if now() >= entry.deadline_ms {
                None
            } else {
                provider.revoke(&entry.refresh_token)
            };
            store
                .commit(
                    &key,
                    Change::JournalProgress {
                        generation: entry.generation,
                        expected_revision: entry.revision,
                        step: JournalStep::RevocationAttempt(response),
                    },
                    &context(id),
                )
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
fn refresh(
    id: &str,
    config: &Config,
    binding: &Binding,
) -> Result<Binding, calendar::provider::TokenFailure> {
    if binding.client_id != config.client_id {
        return Err("Calendar OAuth client changed; disconnect and reconnect".into());
    }
    let provider = Provider::new()?;
    let tokens = provider.refresh(config, &binding.refresh_token)?;
    let rotated = tokens
        .refresh_token
        .as_ref()
        .filter(|value| value.as_str() != binding.refresh_token.expose())
        .cloned();
    let outcome: Result<Binding, calendar::provider::TokenFailure> = (|| {
        if let Some(scopes) = &tokens.scope {
            for required in oauth::SCOPES {
                if !scopes.split_whitespace().any(|s| s == *required) {
                    return Err(calendar::provider::TokenFailure {
                        state: calendar::failure::FailureState::Terminal(
                            calendar::failure::TerminalReason::ScopeWithdrawn,
                        ),
                        detail: "Google Calendar scope was withdrawn; reconnect".into(),
                    });
                }
            }
        }
        if provider.subject(&tokens.access_token)? != binding.sub {
            return Err(calendar::provider::TokenFailure {
                state: calendar::failure::FailureState::Terminal(
                    calendar::failure::TerminalReason::Unauthorized,
                ),
                detail: "Google account changed during refresh".into(),
            });
        }
        envelopes()
            .commit(
                &binding::envelope_key(id),
                Change::Refresh {
                    generation: binding.generation,
                    access_token: Redacted::new(tokens.access_token),
                    access_expires_at_ms: now().saturating_add(tokens.expires_in * 1000),
                    stale_after_ms: calendar::stale_after_ms(now()),
                    refresh_token: tokens.refresh_token.map(Redacted::new),
                },
                &context(id),
            )
            .map_err(|e| e.to_string())?;
        active(id, Some(binding.generation)).map_err(Into::into)
    })();
    if outcome.is_err() {
        if let Some(token) = rotated {
            let token = Redacted::new(token);
            if envelopes()
                .commit(
                    &binding::envelope_key(id),
                    Change::DisconnectWithToken {
                        generation: binding.generation,
                        refresh_token: token.clone(),
                    },
                    &context(id),
                )
                .is_err()
            {
                let confirmed = provider.revoke(&token) == Some(200);
                return Err(if confirmed {"Refresh could not be saved; Google confirmed revocation"}else{"Refresh could not be saved and grant revocation is unconfirmed; remove Buzz access in your Google account before reconnecting"}.into());
            }
        }
    }
    outcome
}
fn connect(app: &AppHandle, expected_identity: &str) -> Result<Status, String> {
    let config = Config::load()?;
    ensure_identity(app, expected_identity)?;
    let id = expected_identity.to_string();
    let key = binding::envelope_key(&id);
    retry_revocations(app, &id)?;
    let before = envelopes().read(&key).map_err(|e| e.to_string())?;
    if before.active_binding.is_some() {
        return Err("A Google account is already connected".into());
    }
    if before
        .pending
        .values()
        .any(|entry| entry.state == RevocationState::Retryable)
    {
        return Err("Resolve pending Google revocations before connecting".into());
    }
    if before.pending.len() >= calendar::revocation::MAX_PENDING_REVOCATIONS {
        return Err("Clear a completed or abandoned revocation before connecting".into());
    }
    let listener = calendar::loopback::CallbackListener::bind().map_err(|e| e.to_string())?;
    let request = oauth::AuthRequest::new(&config.client_id, listener.redirect_uri(), true)
        .map_err(|e| e.to_string())?;
    app.opener()
        .open_url(request.authorization_url(), None::<&str>)
        .map_err(|_| "could not open Google authorization")?;
    let query = listener
        .wait_for_callback(&calendar::loopback::ListenerLimits::default())
        .map_err(|e| e.to_string())?;
    let code = request
        .verify_callback(&query)
        .map_err(|_| "Google authorization callback was rejected")?;
    let mut bytes = [0u8; 8];
    getrandom::getrandom(&mut bytes).map_err(|_| "calendar generation entropy unavailable")?;
    let provider = Provider::new()?;
    let tokens = provider.exchange(&config, &request, &code)?;
    let refresh_token = match tokens.refresh_token {
        Some(token) => token,
        None => {
            let confirmed = provider.revoke(&Redacted::new(tokens.access_token)) == Some(200);
            return Err(if confirmed {"Google returned no refresh token; grant revoked, reconnect with consent"}else{"Google returned no refresh token and revocation is unconfirmed; remove Buzz access in your Google account"}.into());
        }
    };
    // JavaScript numbers represent every generation exactly.
    let generation = (u64::from_le_bytes(bytes) & ((1u64 << 53) - 1)).max(1);
    // Take durable retry custody immediately, before claims verification or identity commit.
    let held = Binding {
        identity_pubkey_hex: id.clone(),
        client_id: config.client_id.clone(),
        sub: String::new(),
        email: String::new(),
        scopes: vec![],
        generation,
        access_token: Redacted::new(String::new()),
        refresh_token: Redacted::new(refresh_token),
        access_expires_at_ms: 0,
        stale_after_ms: 0,
    };
    if envelopes()
        .commit(
            &key,
            Change::HoldGrant(Box::new(held.clone())),
            &context(&id),
        )
        .is_err()
    {
        let revoked = provider.revoke(&held.refresh_token) == Some(200);
        return Err(if revoked {"Calendar could not save the grant; Google confirmed its revocation"} else {"Calendar could not save the grant and Google revocation is unconfirmed; remove Buzz access in your Google account before reconnecting"}.into());
    }
    let verified = (|| {
        let scopes: Vec<String> = tokens
            .scope
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        oauth::check_exchange(&scopes, true, tokens.id_token.is_some())
            .map_err(|e| e.to_string())?;
        let claims = oauth::verify_id_token(
            tokens.id_token.as_deref().unwrap_or_default(),
            &provider.verifier()?,
            &oauth::IdTokenExpectations {
                client_id: config.client_id.clone(),
                nonce: request.nonce.clone(),
                now_ms: now(),
            },
        )
        .map_err(|e| e.to_string())?;
        let state = app.state::<AppState>();
        let _identity = state
            .identity_mutation
            .lock()
            .map_err(|_| "identity lock unavailable")?;
        ensure_identity(app, &id)?;
        let binding = Binding {
            identity_pubkey_hex: id.clone(),
            client_id: config.client_id,
            sub: claims.sub,
            email: claims.email.unwrap_or_default(),
            scopes,
            generation,
            access_token: Redacted::new(tokens.access_token),
            refresh_token: held.refresh_token,
            access_expires_at_ms: now().saturating_add(tokens.expires_in * 1000),
            stale_after_ms: calendar::stale_after_ms(now()),
        };
        envelopes()
            .commit(
                &key,
                Change::ActivateHeldGrant(Box::new(binding)),
                &context(&id),
            )
            .map_err(|e| e.to_string())
    })();
    if let Err(error) = verified {
        let _ = retry_revocations(app, &id);
        return Err(error);
    }
    status(app, expected_identity)
}
fn disconnect(app: &AppHandle, generation: u64, expected_identity: &str) -> Result<Status, String> {
    let id;
    {
        let state = app.state::<AppState>();
        let _identity = state
            .identity_mutation
            .lock()
            .map_err(|_| "identity lock unavailable")?;
        ensure_identity(app, expected_identity)?;
        id = identity(app)?;
        envelopes()
            .commit(
                &binding::envelope_key(&id),
                Change::Disconnect { generation },
                &context(&id),
            )
            .map_err(|e| e.to_string())?;
    }
    // A failed cleanup leaves the durable journal visible in status.
    let cleanup = retry_revocations(app, &id);
    let mut result = status(app, expected_identity)?;
    if cleanup.is_err() {
        result.error = Some("Disconnected; local purge or Google revocation is pending".into());
    }
    Ok(result)
}
fn events(app: &AppHandle, expected_identity: &str) -> Result<Snapshot, String> {
    let config = Config::load()?;
    let state = app.state::<AppState>();
    let _identity = state
        .identity_mutation
        .lock()
        .map_err(|_| "identity lock unavailable")?;
    ensure_identity(app, expected_identity)?;
    let id = identity(app)?;
    let binding = active(&id, None)?;
    let refreshed = match refresh(&id, &config, &binding) {
        Ok(binding) => binding,
        Err(error) => {
            if matches!(error.state, calendar::failure::FailureState::Transient(_)) {
                if let Some(saved) = cache(app)?.read_stale(
                    &id,
                    binding.generation,
                    now(),
                    binding.stale_after_ms,
                )? {
                    ensure_identity(app, &id)?;
                    active(&id, Some(binding.generation))?;
                    return Ok(saved);
                }
            } else if error.state.purges_cache() {
                cache(app)?.purge(&id)?;
            }
            return Err(error.to_string());
        }
    };
    let transport = client::HttpTransport::new(&client::TransportConfig::google())?;
    match client::fetch_events(
        &transport,
        &SystemClock,
        "primary",
        calendar::default_window(now()),
        &refreshed.access_token,
        &client::FetchLimits::default(),
    ) {
        Ok(batch) => {
            if batch.stopped_by.as_ref().is_some_and(|failure| {
                failure.state.purges_cache()
                    || matches!(
                        failure.state,
                        calendar::failure::FailureState::Transient(
                            calendar::failure::TransientReason::NeedsRefresh
                        )
                    )
            }) {
                cache(app)?.purge(&id)?;
                return Err("Google Calendar access was withdrawn; reconnect".into());
            }
            let mut snapshot = Snapshot {
                events: batch.events,
                interval: Some(batch.interval),
                stale: batch.stopped_by.is_some(),
                refreshed_at_ms: Some(now()),
                generation: refreshed.generation,
            };
            if snapshot.stale {
                for event in &mut snapshot.events {
                    event.can_edit = false;
                    event.can_delete = false;
                }
            }
            ensure_identity(app, &id)?;
            active(&id, Some(refreshed.generation))?;
            cache(app)?.write(&id, &snapshot, refreshed.stale_after_ms, now())?;
            Ok(snapshot)
        }
        Err(error) => {
            if matches!(&error, client::FetchError::Failure(failure) if failure.state.purges_cache() || matches!(failure.state,calendar::failure::FailureState::Transient(calendar::failure::TransientReason::NeedsRefresh)))
            {
                cache(app)?.purge(&id)?;
                return Err(error.to_string());
            }
            if let Some(saved) = cache(app)?.read_stale(
                &id,
                refreshed.generation,
                now(),
                refreshed.stale_after_ms,
            )? {
                ensure_identity(app, &id)?;
                return Ok(saved);
            }
            Err(error.to_string())
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateInput {
    expected_identity: String,
    expected_generation: u64,
    event_id: String,
    fields: Fields,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateInput {
    expected_identity: String,
    expected_generation: u64,
    event_id: String,
    etag: String,
    fields: Fields,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeleteInput {
    expected_identity: String,
    expected_generation: u64,
    event_id: String,
    etag: String,
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Fields {
    summary: Option<String>,
    location: Option<String>,
    description: Option<String>,
    start: Option<EventTime>,
    end: Option<EventTime>,
}
fn fields_json(fields: &Fields, create: bool) -> Result<serde_json::Value, String> {
    let mut values = serde_json::Map::new();
    for (name, value, cap) in [
        ("summary", &fields.summary, 256),
        ("location", &fields.location, 256),
        ("description", &fields.description, 4096),
    ] {
        if let Some(value) = value {
            if value.chars().count() > cap {
                return Err(format!("{name} is too long"));
            }
            values.insert(name.into(), serde_json::Value::String(value.clone()));
        }
    }
    if create && (fields.start.is_none() || fields.end.is_none() || fields.summary.is_none()) {
        return Err("summary, start and end are required".into());
    }
    if fields.start.is_some() != fields.end.is_some() {
        return Err("start and end must be changed together".into());
    }
    if let (Some(start), Some(end)) = (&fields.start, &fields.end) {
        let ordered = match (start, end) {
            (EventTime::AllDay { date: a }, EventTime::AllDay { date: b }) => a < b,
            (EventTime::Timed { instant_ms: a, .. }, EventTime::Timed { instant_ms: b, .. }) => {
                a < b
            }
            _ => false,
        };
        if !ordered {
            return Err("event end must follow start with the same time type".into());
        }
    }
    for (name, time) in [("start", &fields.start), ("end", &fields.end)] {
        if let Some(time) = time {
            let value = match time {
                EventTime::AllDay { date } => {
                    if date.len() != 10
                        || chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_err()
                    {
                        return Err("invalid all-day date".into());
                    }
                    serde_json::json!({"date":date})
                }
                EventTime::Timed {
                    instant_ms,
                    time_zone,
                } => {
                    if time_zone.as_ref().is_some_and(|z| z.len() > 64) {
                        return Err("time zone too long".into());
                    }
                    let instant = chrono::DateTime::from_timestamp_millis(*instant_ms)
                        .ok_or("invalid event time")?;
                    let mut value = serde_json::json!({"dateTime":instant.to_rfc3339()});
                    if let Some(zone) = time_zone {
                        value["timeZone"] = zone.clone().into();
                    }
                    value
                }
            };
            values.insert(name.into(), value);
        }
    }
    if values.is_empty() {
        return Err("no event fields changed".into());
    }
    Ok(values.into())
}
fn validate_id(id: &str, etag: Option<&str>, create: bool) -> Result<(), String> {
    if id.is_empty() || id.len() > 1024 || id.chars().any(char::is_control) {
        return Err("invalid event ID".into());
    }
    if create
        && (id.len() < 5
            || !id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'v').contains(&b)))
    {
        return Err("new event ID must use Google base32hex format".into());
    }
    if etag.is_some_and(|e| e.is_empty() || e.len() > 256 || e.chars().any(char::is_control)) {
        return Err("invalid event version".into());
    }
    Ok(())
}
fn mutate<T>(
    app: &AppHandle,
    expected_identity: &str,
    generation: u64,
    action: impl FnOnce(
        &client::HttpTransport,
        &Binding,
        calendar::dto::AccessRole,
    ) -> Result<T, String>,
) -> Result<T, String> {
    let config = Config::load()?;
    let state = app.state::<AppState>();
    let _identity = state
        .identity_mutation
        .lock()
        .map_err(|_| "identity lock unavailable")?;
    ensure_identity(app, expected_identity)?;
    let id = identity(app)?;
    let binding = active(&id, Some(generation))?;
    let refreshed = refresh(&id, &config, &binding).map_err(|error| error.to_string())?;
    let transport = client::HttpTransport::new(&client::TransportConfig::google())?;
    active(&id, Some(generation))?;
    ensure_identity(app, &id)?;
    // Refresh Google calendar ACL immediately before every write; cached UI is never authority.
    let role = fresh_role(&transport, &refreshed)?;
    let result = action(&transport, &refreshed, role);
    cache(app)?.purge(&id)?;
    ensure_identity(app, &id)?;
    active(&id, Some(generation))?;
    result
}
fn provider_read(
    transport: &impl client::CalendarTransport,
    binding: &Binding,
    path: String,
    query: Vec<(String, String)>,
) -> Result<Vec<u8>, String> {
    let response = transport
        .send(
            &client::CalendarRequest {
                method: client::HttpMethod::Get,
                path,
                query,
                body: None,
                if_match: None,
            },
            &binding.access_token,
            15_000,
            client::MAX_EVENT_BODY_BYTES,
        )
        .map_err(|e| e.to_string())?;
    if response.status != 200 || response.truncated_at_cap {
        return Err("Google calendar authority could not be refreshed".into());
    }
    Ok(response.body)
}
fn fresh_role(
    transport: &impl client::CalendarTransport,
    binding: &Binding,
) -> Result<calendar::dto::AccessRole, String> {
    let bytes = provider_read(
        transport,
        binding,
        "calendars/primary/events".into(),
        vec![("maxResults".into(), "1".into())],
    )?;
    let page = calendar::dto::parse_events_page(&bytes, client::MAX_EVENT_BODY_BYTES)
        .map_err(|e| e.to_string())?;
    if !matches!(
        page.access_role,
        calendar::dto::AccessRole::Owner | calendar::dto::AccessRole::Writer
    ) {
        return Err("Google Calendar is read-only".into());
    }
    Ok(page.access_role)
}
fn fresh_event(
    transport: &impl client::CalendarTransport,
    binding: &Binding,
    id: &str,
    role: calendar::dto::AccessRole,
) -> Result<CalendarEvent, String> {
    let segment = percent_encoding::utf8_percent_encode(id, percent_encoding::NON_ALPHANUMERIC);
    let bytes = provider_read(
        transport,
        binding,
        format!("calendars/primary/events/{segment}"),
        vec![],
    )?;
    calendar::dto::parse_single_event(&bytes, client::MAX_EVENT_BODY_BYTES, role)
        .map_err(|e| e.to_string())
}
fn same_time(left: Option<&EventTime>, right: &EventTime) -> bool {
    match (left, right) {
        (Some(EventTime::Timed { instant_ms: a, .. }), EventTime::Timed { instant_ms: b, .. }) => {
            a == b
        }
        (Some(EventTime::AllDay { date: a }), EventTime::AllDay { date: b }) => a == b,
        _ => false,
    }
}
fn matches_created_event(event: &CalendarEvent, fields: &Fields) -> bool {
    event.can_edit
        && event.recurring_event_id.is_none()
        && same_time(fields.start.as_ref(), &event.start)
        && same_time(fields.end.as_ref(), &event.end)
        && fields.summary.as_deref() == Some(event.summary.value.as_str())
        && !event.summary.truncated
        && fields.location.as_deref().unwrap_or("") == event.location.value
        && !event.location.truncated
        && fields.description.as_deref().unwrap_or("") == event.description.value
        && !event.description.truncated
}
fn check_edit(event: &CalendarEvent, etag: &str, fields: Option<&Fields>) -> Result<(), String> {
    if event.etag.as_deref() != Some(etag) {
        return Err("Event changed in Google Calendar; reload before editing".into());
    }
    match fields {
        None if !event.can_delete => return Err("This event cannot be deleted".into()),
        Some(fields) => {
            if !event.can_edit {
                return Err("This event cannot be edited".into());
            }
            for (present, field) in [
                (fields.summary.is_some(), calendar::dto::EventField::Summary),
                (
                    fields.location.is_some(),
                    calendar::dto::EventField::Location,
                ),
                (
                    fields.description.is_some(),
                    calendar::dto::EventField::Description,
                ),
            ] {
                if present && !event.can_edit_field(field) {
                    return Err("A truncated event field cannot be overwritten".into());
                }
            }
        }
        _ => {}
    }
    Ok(())
}
async fn blocking<T: Send + 'static>(
    action: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = OPERATIONS
            .lock()
            .map_err(|_| "calendar operation lock unavailable")?;
        action()
    })
    .await
    .map_err(|_| "calendar task failed".to_string())?
}
#[tauri::command]
pub async fn calendar_status(app: AppHandle, expected_identity: String) -> Result<Status, String> {
    blocking(move || status(&app, &expected_identity)).await
}
#[tauri::command]
pub async fn calendar_connect(app: AppHandle, expected_identity: String) -> Result<Status, String> {
    blocking(move || connect(&app, &expected_identity)).await
}
#[tauri::command]
pub async fn calendar_disconnect(
    app: AppHandle,
    expected_generation: u64,
    expected_identity: String,
) -> Result<Status, String> {
    blocking(move || disconnect(&app, expected_generation, &expected_identity)).await
}
#[tauri::command]
pub async fn calendar_events(
    app: AppHandle,
    expected_identity: String,
) -> Result<Snapshot, String> {
    blocking(move || events(&app, &expected_identity)).await
}
#[tauri::command]
pub async fn calendar_create(app: AppHandle, input: CreateInput) -> Result<CalendarEvent, String> {
    blocking(move || {
        validate_id(&input.event_id, None, true)?;
        let fields = fields_json(&input.fields, true)?;
        mutate(
            &app,
            &input.expected_identity,
            input.expected_generation,
            |t, b, role| {
                match client::insert_event(t, "primary", &input.event_id, &fields, &b.access_token)
                {
                    Ok(event) => Ok(event),
                    Err(error) => {
                        // A create response can be lost after Google has committed it.
                        // Only recover the exact client ID and exact submitted fields.
                        if let Ok(existing) = fresh_event(t, b, &input.event_id, role) {
                            if matches_created_event(&existing, &input.fields) {
                                return Ok(existing);
                            }
                        }
                        Err(error.to_string())
                    }
                }
            },
        )
    })
    .await
}
#[tauri::command]
pub async fn calendar_update(app: AppHandle, input: UpdateInput) -> Result<CalendarEvent, String> {
    blocking(move || {
        validate_id(&input.event_id, Some(&input.etag), false)?;
        let fields = fields_json(&input.fields, false)?;
        mutate(
            &app,
            &input.expected_identity,
            input.expected_generation,
            |t, b, role| {
                let current = fresh_event(t, b, &input.event_id, role)?;
                check_edit(&current, &input.etag, Some(&input.fields))?;
                client::patch_event(
                    t,
                    "primary",
                    &input.event_id,
                    &input.etag,
                    &fields,
                    &b.access_token,
                )
                .map_err(|e| e.to_string())
            },
        )
    })
    .await
}
#[tauri::command]
pub async fn calendar_delete(app: AppHandle, input: DeleteInput) -> Result<(), String> {
    blocking(move || {
        validate_id(&input.event_id, Some(&input.etag), false)?;
        mutate(
            &app,
            &input.expected_identity,
            input.expected_generation,
            |t, b, role| {
                let current = fresh_event(t, b, &input.event_id, role)?;
                check_edit(&current, &input.etag, None)?;
                client::delete_event(t, "primary", &input.event_id, &input.etag, &b.access_token)
                    .map_err(|e| e.to_string())
            },
        )
    })
    .await
}

fn recover_revocation(
    app: &AppHandle,
    expected_identity: &str,
    generation: u64,
    clear: bool,
) -> Result<Status, String> {
    {
        let state = app.state::<AppState>();
        let _identity = state
            .identity_mutation
            .lock()
            .map_err(|_| "identity lock unavailable")?;
        ensure_identity(app, expected_identity)?;
        let change = if clear {
            Change::ClearRevocation { generation }
        } else {
            Change::AbandonRevocation { generation }
        };
        envelopes()
            .commit(
                &binding::envelope_key(expected_identity),
                change,
                &context(expected_identity),
            )
            .map_err(|e| e.to_string())?;
    }
    // Purge remains independent from explicitly abandoned provider retry.
    let _ = retry_revocations(app, expected_identity);
    status(app, expected_identity)
}
#[tauri::command]
pub async fn calendar_abandon_revocation(
    app: AppHandle,
    expected_identity: String,
    generation: u64,
) -> Result<Status, String> {
    blocking(move || recover_revocation(&app, &expected_identity, generation, false)).await
}
#[tauri::command]
pub async fn calendar_clear_revocation(
    app: AppHandle,
    expected_identity: String,
    generation: u64,
) -> Result<Status, String> {
    blocking(move || recover_revocation(&app, &expected_identity, generation, true)).await
}

/// Retry durable cleanup while the app is running. Every attempt has provider timeouts.
pub fn start_retry_worker(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            if app
                .state::<AppState>()
                .shutdown_started
                .load(std::sync::atomic::Ordering::Acquire)
            {
                break;
            }
            let handle = app.clone();
            let _ = blocking(move || {
                let blob = crate::secret_store::SecretStore::shared(keyring_service())
                    .load_all_readonly()?;
                if let Some(blob) = blob {
                    let identities: Vec<_> = blob
                        .keys()
                        .filter_map(|key| key.strip_prefix(binding::ENVELOPE_KEY_PREFIX))
                        .collect();
                    if identities.len() > 64 {
                        return Err("calendar identity cap exceeded".into());
                    }
                    for id in identities {
                        if retry_revocations(&handle, id).is_err() {
                            eprintln!("Calendar cleanup remains pending for a stored identity");
                        }
                    }
                }
                Ok(())
            })
            .await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn calendar_form_rejects_attendees_and_oversized_text() {
        assert!(serde_json::from_value::<Fields>(
            serde_json::json!({"attendees":[{"email":"any@example.com"}]})
        )
        .is_err());
        assert!(fields_json(
            &Fields {
                summary: Some("x".repeat(257)),
                ..Default::default()
            },
            false
        )
        .is_err());
    }
    #[test]
    fn calendar_form_preserves_all_day_exclusive_end() {
        let fields = Fields {
            summary: Some("Example".into()),
            start: Some(EventTime::AllDay {
                date: "2026-09-08".into(),
            }),
            end: Some(EventTime::AllDay {
                date: "2026-09-09".into(),
            }),
            ..Default::default()
        };
        let body = fields_json(&fields, true).unwrap();
        assert_eq!(body["start"]["date"], "2026-09-08");
        assert_eq!(body["end"]["date"], "2026-09-09");
        assert!(body.get("attendees").is_none());
    }
    #[test]
    fn calendar_form_rejects_missing_pair_and_invalid_dates() {
        let fields = Fields {
            start: Some(EventTime::AllDay {
                date: "2026-09-08".into(),
            }),
            ..Default::default()
        };
        assert!(fields_json(&fields, false).is_err());
        let fields = Fields {
            start: Some(EventTime::AllDay {
                date: "2026-02-30".into(),
            }),
            end: Some(EventTime::AllDay {
                date: "2026-03-02".into(),
            }),
            ..Default::default()
        };
        assert!(fields_json(&fields, false).is_err());
    }
    #[test]
    fn calendar_commands_require_generation_and_reject_account_override() {
        assert!(serde_json::from_value::<CreateInput>(
            serde_json::json!({"eventId":"aaaaa","fields":{}})
        )
        .is_err());
        assert!(serde_json::from_value::<DeleteInput>(serde_json::json!({"expectedGeneration":1,"eventId":"aaaaa","etag":"v1","calendarId":"someone"})).is_err());
        assert!(validate_id("AAAAA", None, true).is_err());
        assert!(validate_id("aaaaa", None, true).is_ok());
    }
}

#[cfg(test)]
mod authority_tests {
    use super::*;
    struct WriterTransport;
    impl client::CalendarTransport for WriterTransport {
        fn send(
            &self,
            request: &client::CalendarRequest,
            _token: &Redacted<String>,
            _timeout: i64,
            _cap: usize,
        ) -> Result<client::HttpResponse, client::TransportError> {
            let body = if request.path == "calendars/primary/events" {
                serde_json::json!({"accessRole":"writer","items":[]})
            } else {
                assert_eq!(request.path, "calendars/primary/events/event1");
                serde_json::json!({"id":"event1","etag":"v1","status":"confirmed","summary":"Example","organizer":{"self":false},"start":{"date":"2026-09-08"},"end":{"date":"2026-09-09"}})
            };
            Ok(client::HttpResponse {
                status: 200,
                body: serde_json::to_vec(&body).unwrap(),
                truncated_at_cap: false,
            })
        }
    }
    fn binding() -> Binding {
        Binding {
            identity_pubkey_hex: "fixture".into(),
            client_id: "fixture".into(),
            sub: "fixture".into(),
            email: String::new(),
            scopes: vec![],
            generation: 1,
            access_token: Redacted::new("fixture".into()),
            refresh_token: Redacted::new("fixture".into()),
            access_expires_at_ms: 1,
            stale_after_ms: 1,
        }
    }
    #[test]
    fn calendar_fresh_writer_authority_is_not_upgraded_to_owner() {
        let transport = WriterTransport;
        let binding = binding();
        let role = fresh_role(&transport, &binding).unwrap();
        let event = fresh_event(&transport, &binding, "event1", role).unwrap();
        assert!(!event.can_delete);
        assert!(check_edit(&event, "v1", None).is_err());
    }
    #[test]
    fn calendar_create_recovery_requires_exact_fields_and_version_checks_preserve_conflicts() {
        let event = fresh_event(
            &WriterTransport,
            &binding(),
            "event1",
            calendar::dto::AccessRole::Owner,
        )
        .unwrap();
        let mut fields = Fields {
            summary: Some("Example".into()),
            start: Some(event.start.clone()),
            end: Some(event.end.clone()),
            ..Default::default()
        };
        assert!(matches_created_event(&event, &fields));
        fields.summary = Some("Another event".into());
        assert!(!matches_created_event(&event, &fields));
        assert!(check_edit(&event, "old-version", None).is_err());
    }
}
