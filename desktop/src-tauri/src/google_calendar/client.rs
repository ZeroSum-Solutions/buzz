//! The bounded `events.list` walk of T11 decision 6 and T12a decisions 5 and
//! 13, plus the three mutations of T12a decision 11.
//!
//! Every bound here caps the quantity that actually costs: pages requested,
//! bytes read off the socket, events accumulated, and wall-clock time. A walk
//! that ends against a bound returns what it *proved* — never a complete-looking
//! empty tail — and carries the failure that stopped it, so nothing is
//! swallowed.
//!
//! The transport is a seam ([`CalendarTransport`]) so the walk, the caps and
//! the classification are the same code in a test as in the app. The shipped
//! transport pins one origin and path prefix — parsed, not prefix-matched —
//! refuses redirects, and does not read proxy settings from the process
//! environment, because a managed agent at operator trust can name both the
//! redirect target and the proxy.

use std::io::Read as _;
use std::time::Duration;

use super::dto::{
    parse_events_page, parse_single_event, window_bounds_rfc3339, AccessRole, CalendarEvent,
    DtoError,
};
use super::failure::{classify_list, classify_mutation, FailureState};
use super::interval::{ProvenInterval, TruncationReason, Window};
use super::redact::Redacted;

/// Largest number of `events.list` pages one walk requests.
pub const MAX_PAGES: usize = 10;
/// Largest total body size one walk reads, in bytes.
pub const MAX_TOTAL_BYTES: usize = 8 * 1024 * 1024;
/// Largest number of events one walk accumulates (T11 decision 6's partition
/// row bound).
pub const MAX_EVENTS: usize = 5_000;
/// Longest one walk may take.
pub const FETCH_DEADLINE_MS: i64 = 30_000;
/// `maxResults` asked of Google per page.
pub const PAGE_SIZE: usize = 250;
/// Longest one mutation may take.
pub const MUTATION_TIMEOUT_MS: i64 = 15_000;

/// The caps one walk runs under.
#[derive(Debug, Clone, Copy)]
pub struct FetchLimits {
    /// Largest number of pages requested.
    pub max_pages: usize,
    /// Largest total body size read, in bytes.
    pub max_total_bytes: usize,
    /// Largest number of events accumulated.
    pub max_events: usize,
    /// Longest the whole walk may take, in milliseconds.
    pub deadline_ms: i64,
}

impl Default for FetchLimits {
    fn default() -> Self {
        Self {
            max_pages: MAX_PAGES,
            max_total_bytes: MAX_TOTAL_BYTES,
            max_events: MAX_EVENTS,
            deadline_ms: FETCH_DEADLINE_MS,
        }
    }
}

/// The HTTP verbs this module uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    /// `GET`.
    Get,
    /// `POST`.
    Post,
    /// `PATCH`.
    Patch,
    /// `DELETE`.
    Delete,
}

impl HttpMethod {
    /// The verb as it appears on the wire.
    pub fn as_str(&self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Patch => "PATCH",
            HttpMethod::Delete => "DELETE",
        }
    }
}

/// One request, as a path and query the transport composes onto its base.
///
/// The access token is never part of a request: it travels as a header the
/// transport attaches, so no URL, log line or error can carry it.
#[derive(Debug, Clone)]
pub struct CalendarRequest {
    /// The verb.
    pub method: HttpMethod,
    /// Path below the API root, already percent-encoded.
    pub path: String,
    /// Query parameters, encoded by the transport.
    pub query: Vec<(String, String)>,
    /// JSON body, when the verb carries one.
    pub body: Option<Vec<u8>>,
    /// `If-Match`, sent on every mutation that must not clobber (T12a
    /// decision 11).
    pub if_match: Option<String>,
}

/// One response, with the body already bounded.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// The status code.
    pub status: u16,
    /// The body, at most the byte budget the caller passed.
    pub body: Vec<u8>,
    /// Whether the body was cut at that budget. A cut body is never parsed.
    pub truncated_at_cap: bool,
}

impl HttpResponse {
    /// Google's `error.errors[0].reason`, when the body carries one.
    ///
    /// The value is API-sourced text, so it is capped by
    /// [`normalize_reason`](super::failure::normalize_reason) before it is
    /// matched or stored.
    pub fn error_reason(&self) -> String {
        serde_json::from_slice::<serde_json::Value>(&self.body)
            .ok()
            .and_then(|body| {
                let error = body.get("error")?;
                if let Some(reason) = error.get("status").and_then(serde_json::Value::as_str) {
                    return Some(reason.to_string());
                }
                if let Some(reason) = error.as_str() {
                    return Some(reason.to_string());
                }
                error
                    .get("errors")?
                    .as_array()?
                    .first()?
                    .get("reason")?
                    .as_str()
                    .map(str::to_string)
            })
            .unwrap_or_default()
    }
}

/// Why a request never produced a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError(pub String);

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for TransportError {}

/// Sends one request and returns at most `max_body_bytes` of the response.
pub trait CalendarTransport {
    /// Send `request` with `access_token` as the bearer credential.
    ///
    /// # Errors
    /// Returns [`TransportError`] when no response arrived. The message must
    /// not carry the token or any header value.
    fn send(
        &self,
        request: &CalendarRequest,
        access_token: &Redacted<String>,
        timeout_ms: i64,
        max_body_bytes: usize,
    ) -> Result<HttpResponse, TransportError>;
}

/// Reads the wall clock. A seam so a deadline is testable without sleeping.
pub trait Clock {
    /// Now, in milliseconds since the Unix epoch.
    fn now_ms(&self) -> i64;
}

/// The system clock.
///
/// A clock set before the Unix epoch reads as 0. That is the safe direction
/// here: every deadline in this module is a duration added to the reading, so
/// a wrong reading shortens or lengthens one fetch, and never turns a bound
/// off.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as i64)
            .unwrap_or(0)
    }
}

/// What one walk returned.
#[derive(Debug, Clone)]
pub struct EventBatch {
    /// The events it proved, in API order.
    pub events: Vec<CalendarEvent>,
    /// The interval those events are authoritative for (T12a decision 13).
    pub interval: ProvenInterval,
    /// The caller's role on the calendar.
    pub access_role: AccessRole,
    /// The calendar's own zone, when the response named one.
    pub default_time_zone: Option<String>,
    /// Pages received whole.
    pub pages_fetched: usize,
    /// Bytes read off the socket.
    pub bytes_read: usize,
    /// Cancelled instances with no times, counted rather than dropped quietly.
    pub dropped_cancelled: usize,
    /// The failure that ended the walk, when one did. A truncated batch always
    /// carries the reason it stopped, with the detail behind it.
    pub stopped_by: Option<ClassifiedFailure>,
}

/// A classified failure and the detail that produced it.
///
/// The state decides what the caller does; the detail says why, so a transport
/// failure is never reduced to "network error" with its cause dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedFailure {
    /// What the caller must do.
    pub state: FailureState,
    /// The transport message, or Google's status and reason. Never a
    /// credential: both sources are built from values this module controls.
    pub detail: String,
}

impl ClassifiedFailure {
    /// Pair `state` with the detail that produced it.
    pub fn new(state: FailureState, detail: impl Into<String>) -> Self {
        Self {
            state,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for ClassifiedFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?} ({})", self.state, self.detail)
    }
}

/// Why a walk returned nothing usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// A classified API or transport failure, with no complete page behind it.
    Failure(ClassifiedFailure),
    /// A page could not be read.
    Dto(DtoError),
    /// The requested window cannot be expressed as RFC 3339 instants.
    UnrepresentableWindow,
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Failure(failure) => write!(f, "calendar request failed: {failure}"),
            FetchError::Dto(error) => write!(f, "{error}"),
            FetchError::UnrepresentableWindow => {
                write!(f, "the requested calendar window is out of range")
            }
        }
    }
}

impl std::error::Error for FetchError {}

/// Walk `events.list` over `window`, under `limits`.
///
/// Recurrence is expanded by Google (`singleEvents=true`, `orderBy=startTime`),
/// which is what makes T12a decision 13's half-open bound sound. A page whose
/// body was cut at the byte budget is discarded whole.
///
/// # Errors
/// Returns [`FetchError::Failure`] when the walk failed before any page
/// completed, or on any terminal failure — the caller purges on those.
/// [`FetchError::Dto`] when a received page could not be read, and
/// [`FetchError::UnrepresentableWindow`] for a window outside the expressible
/// range.
pub fn fetch_events<T: CalendarTransport + ?Sized, C: Clock + ?Sized>(
    transport: &T,
    clock: &C,
    calendar_id: &str,
    window: Window,
    access_token: &Redacted<String>,
    limits: &FetchLimits,
) -> Result<EventBatch, FetchError> {
    let (time_min, time_max) =
        window_bounds_rfc3339(window).ok_or(FetchError::UnrepresentableWindow)?;
    let deadline_ms = clock.now_ms().saturating_add(limits.deadline_ms);

    let mut events: Vec<CalendarEvent> = Vec::new();
    let mut access_role = AccessRole::Unknown;
    let mut default_time_zone: Option<String> = None;
    let mut last_complete_max_start: Option<i64> = None;
    let mut pages_fetched = 0usize;
    let mut bytes_read = 0usize;
    let mut dropped_cancelled = 0usize;
    let mut page_token: Option<String> = None;

    loop {
        // Every early return below builds the same shape through this macro, so
        // a truncated batch cannot be returned without its reason.
        macro_rules! stop {
            ($reason:expr, $failure:expr) => {
                return Ok(EventBatch {
                    events,
                    interval: ProvenInterval::truncated(window, last_complete_max_start, $reason),
                    access_role,
                    default_time_zone,
                    pages_fetched,
                    bytes_read,
                    dropped_cancelled,
                    stopped_by: $failure,
                })
            };
        }

        if pages_fetched >= limits.max_pages {
            stop!(TruncationReason::PageCap, None);
        }
        let remaining_bytes = limits.max_total_bytes.saturating_sub(bytes_read);
        if remaining_bytes == 0 {
            stop!(TruncationReason::ByteCap, None);
        }
        let remaining_ms = deadline_ms.saturating_sub(clock.now_ms());
        if remaining_ms <= 0 {
            stop!(TruncationReason::Deadline, None);
        }

        let mut query = vec![
            ("singleEvents".to_string(), "true".to_string()),
            ("orderBy".to_string(), "startTime".to_string()),
            ("timeMin".to_string(), time_min.clone()),
            ("timeMax".to_string(), time_max.clone()),
            ("maxResults".to_string(), PAGE_SIZE.to_string()),
        ];
        if let Some(token) = &page_token {
            query.push(("pageToken".to_string(), token.clone()));
        }
        let request = CalendarRequest {
            method: HttpMethod::Get,
            path: events_path(calendar_id),
            query,
            body: None,
            if_match: None,
        };

        let response = match transport.send(&request, access_token, remaining_ms, remaining_bytes) {
            Ok(response) => response,
            Err(error) => {
                let failure = ClassifiedFailure::new(classify_list(None, "", false), error.0);
                if last_complete_max_start.is_none() {
                    return Err(FetchError::Failure(failure));
                }
                stop!(TruncationReason::Transport, Some(failure));
            }
        };
        bytes_read = bytes_read.saturating_add(response.body.len());

        if response.status != 200 {
            let reason = response.error_reason();
            let failure = ClassifiedFailure::new(
                classify_list(Some(response.status), &reason, false),
                format!("HTTP {} {reason}", response.status),
            );
            if failure.state.purges_cache() || last_complete_max_start.is_none() {
                return Err(FetchError::Failure(failure));
            }
            stop!(TruncationReason::Transport, Some(failure));
        }
        if response.truncated_at_cap {
            // T12a decision 13: a page cut mid-stream is discarded whole, so it
            // can never contribute a start the walk then treats as proven.
            stop!(TruncationReason::ByteCap, None);
        }

        let page = parse_events_page(&response.body, remaining_bytes).map_err(FetchError::Dto)?;
        pages_fetched += 1;
        access_role = page.access_role;
        if default_time_zone.is_none() {
            default_time_zone = page.default_time_zone.clone();
        }
        dropped_cancelled += page.dropped_cancelled;
        page_token = page.next_page_token;

        // The cap bounds what is *accumulated*, so it is applied to the page
        // before the page is added: extending first and checking after would
        // return up to `max_events + PAGE_SIZE - 1` events, over the partition
        // bound the cap exists to hold.
        let room = limits.max_events.saturating_sub(events.len());
        let overshot = page.events.len() > room;
        let mut kept = page.events;
        kept.truncate(room);
        // The proven bound comes from what was kept, never from what the page
        // carried: a start whose siblings were dropped at the cap is not proven.
        if let Some(max_start) = kept
            .iter()
            .filter_map(|event| event.start.start_lower_bound_ms())
            .max()
        {
            last_complete_max_start =
                Some(last_complete_max_start.map_or(max_start, |seen| seen.max(max_start)));
        }
        events.extend(kept);

        if overshot || events.len() >= limits.max_events {
            stop!(TruncationReason::EventCap, None);
        }
        if page_token.is_none() {
            return Ok(EventBatch {
                events,
                interval: ProvenInterval::complete(window),
                access_role,
                default_time_zone,
                pages_fetched,
                bytes_read,
                dropped_cancelled,
                stopped_by: None,
            });
        }
    }
}

/// Why one mutation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MutationError {
    /// A classified failure. Only [`FailureState::Terminal`] purges.
    Failure(ClassifiedFailure),
    /// The response could not be read.
    Dto(DtoError),
}

impl std::fmt::Display for MutationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MutationError::Failure(failure) => write!(f, "calendar mutation failed: {failure}"),
            MutationError::Dto(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for MutationError {}

/// Largest response body accepted from a single-event call.
pub const MAX_EVENT_BODY_BYTES: usize = 256 * 1024;

/// A client-generated event id: a UUID's 32 lowercase hex digits.
///
/// T12a decision 11 needs a create to be replayable with the same id after a
/// lost response. Hex digits are inside Google's base32hex id alphabet and 32
/// is inside its 5–1024 length range.
pub fn client_event_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Create one event with a caller-chosen id.
///
/// # Errors
/// See [`MutationError`]. Nothing retries automatically: a lost response is
/// replayed by the user, with the same id.
pub fn insert_event<T: CalendarTransport + ?Sized>(
    transport: &T,
    calendar_id: &str,
    event_id: &str,
    fields: &serde_json::Value,
    access_token: &Redacted<String>,
) -> Result<CalendarEvent, MutationError> {
    let mut body = fields.clone();
    match body.as_object_mut() {
        Some(object) => {
            object.insert(
                "id".to_string(),
                serde_json::Value::String(event_id.to_string()),
            );
        }
        None => {
            return Err(MutationError::Dto(DtoError::Malformed(
                "event fields are not an object".to_string(),
            )))
        }
    }
    let request = CalendarRequest {
        method: HttpMethod::Post,
        path: events_path(calendar_id),
        query: Vec::new(),
        body: Some(body.to_string().into_bytes()),
        if_match: None,
    };
    single_event_call(transport, &request, access_token)
}

/// Patch the changed fields of one event, fenced by its `etag`.
///
/// # Errors
/// See [`MutationError`]. A 412 arrives as
/// [`MutationReason::EtagConflict`](super::failure::MutationReason::EtagConflict),
/// which the caller resolves by refetching and showing both values.
pub fn patch_event<T: CalendarTransport + ?Sized>(
    transport: &T,
    calendar_id: &str,
    event_id: &str,
    etag: &str,
    changed_fields: &serde_json::Value,
    access_token: &Redacted<String>,
) -> Result<CalendarEvent, MutationError> {
    let request = CalendarRequest {
        method: HttpMethod::Patch,
        path: format!("{}/{}", events_path(calendar_id), encode_segment(event_id)),
        query: Vec::new(),
        body: Some(changed_fields.to_string().into_bytes()),
        if_match: Some(etag.to_string()),
    };
    single_event_call(transport, &request, access_token)
}

/// Delete one event, fenced by its `etag`.
///
/// # Errors
/// See [`MutationError`].
pub fn delete_event<T: CalendarTransport + ?Sized>(
    transport: &T,
    calendar_id: &str,
    event_id: &str,
    etag: &str,
    access_token: &Redacted<String>,
) -> Result<(), MutationError> {
    let request = CalendarRequest {
        method: HttpMethod::Delete,
        path: format!("{}/{}", events_path(calendar_id), encode_segment(event_id)),
        query: Vec::new(),
        body: None,
        if_match: Some(etag.to_string()),
    };
    let response = transport
        .send(
            &request,
            access_token,
            MUTATION_TIMEOUT_MS,
            MAX_EVENT_BODY_BYTES,
        )
        .map_err(transport_failure)?;
    // Google answers a successful delete with 204, and 410 for one already
    // gone — which decision 11 treats as "probe the calendar", not as success.
    if response.status == 200 || response.status == 204 {
        return Ok(());
    }
    Err(status_failure(&response))
}

fn single_event_call<T: CalendarTransport + ?Sized>(
    transport: &T,
    request: &CalendarRequest,
    access_token: &Redacted<String>,
) -> Result<CalendarEvent, MutationError> {
    let response = transport
        .send(
            request,
            access_token,
            MUTATION_TIMEOUT_MS,
            MAX_EVENT_BODY_BYTES,
        )
        .map_err(transport_failure)?;
    if response.status != 200 {
        return Err(status_failure(&response));
    }
    if response.truncated_at_cap {
        return Err(MutationError::Dto(DtoError::PageTooLarge {
            bytes: response.body.len(),
            cap: MAX_EVENT_BODY_BYTES,
        }));
    }
    // The mutating account can write, so its own role is at least writer; the
    // authoritative role still arrives with the next list.
    parse_single_event(&response.body, MAX_EVENT_BODY_BYTES, AccessRole::Writer)
        .map_err(MutationError::Dto)
}

/// A mutation failure that never reached Google, keeping the transport's own
/// message so the caller can say more than "the network failed".
fn transport_failure(error: TransportError) -> MutationError {
    MutationError::Failure(ClassifiedFailure::new(classify_mutation(None, ""), error.0))
}

/// A mutation failure Google answered, keeping its status and reason.
fn status_failure(response: &HttpResponse) -> MutationError {
    let reason = response.error_reason();
    MutationError::Failure(ClassifiedFailure::new(
        classify_mutation(Some(response.status), &reason),
        format!("HTTP {} {reason}", response.status),
    ))
}

/// The `events` collection path for `calendar_id`.
fn events_path(calendar_id: &str) -> String {
    format!("calendars/{}/events", encode_segment(calendar_id))
}

fn encode_segment(raw: &str) -> String {
    percent_encoding::utf8_percent_encode(raw, percent_encoding::NON_ALPHANUMERIC).to_string()
}

// ── The shipped transport ─────────────────────────────────────────────────

/// Which origin a transport is allowed to reach.
///
/// Not a free-form URL: an origin the caller could name is an origin an agent
/// that reached the caller could name, and every request carries a bearer
/// token. Only the two variants below exist, and one of them is test-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// The real Google Calendar API.
    Google,
    /// A loopback mock. Test-only.
    #[cfg(test)]
    LoopbackTest,
}

/// The exact scheme and host a shipped transport may reach.
const GOOGLE_ORIGIN: (&str, &str, u16) = ("https", "www.googleapis.com", 443);
/// The path prefix a shipped transport may reach below that origin.
const GOOGLE_PATH_PREFIX: &str = "/calendar/v3/";

/// Where the API lives and what the transport is allowed to do to reach it.
#[derive(Debug, Clone)]
pub struct TransportConfig {
    /// Root the request path is composed onto, with a trailing slash. Private:
    /// it is checked against [`TransportConfig::origin`] when the transport is
    /// built, and no caller may substitute another value.
    base_url: String,
    /// The origin the base URL must belong to.
    origin: Origin,
    /// Whether proxy settings may be read from the process environment.
    ///
    /// False in every shipped configuration: managed agents run at operator
    /// trust and can set `HTTPS_PROXY` in this process's environment, and a
    /// bearer token must not be routed by anything a spawned agent can name.
    /// There is no setter; the only constructor that sets it is test-only and
    /// exists so the containment has a test that fails when it is removed.
    use_environment_proxy: bool,
}

impl TransportConfig {
    /// The Google Calendar API root.
    pub fn google() -> Self {
        Self {
            base_url: format!(
                "{}://{}{GOOGLE_PATH_PREFIX}",
                GOOGLE_ORIGIN.0, GOOGLE_ORIGIN.1
            ),
            origin: Origin::Google,
            use_environment_proxy: false,
        }
    }

    /// The base URL requests are composed onto.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Whether this configuration would read proxy settings from the
    /// environment.
    pub fn reads_environment_proxy(&self) -> bool {
        self.use_environment_proxy
    }

    /// Replace the base URL while keeping the origin the configuration pins.
    ///
    /// Test-only, and the only way to construct a Google-origin configuration
    /// whose base URL is wrong: the shipped constructor cannot produce one, so
    /// without this the origin check would have nothing to reject.
    #[cfg(test)]
    pub(crate) fn force_base_url_for_test(&mut self, base_url: impl Into<String>) {
        self.base_url = base_url.into();
    }

    /// A configuration pointed at a loopback test server.
    ///
    /// Test-only, so no shipped build can reach a plaintext or non-Google
    /// origin with a token.
    #[cfg(test)]
    pub(crate) fn loopback(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            origin: Origin::LoopbackTest,
            use_environment_proxy: false,
        }
    }

    /// A loopback configuration that *does* read proxy settings from the
    /// environment.
    ///
    /// Test-only, and the negative control for the containment: with it a
    /// request follows `HTTPS_PROXY`, and without it the same request does not.
    #[cfg(test)]
    pub(crate) fn loopback_reading_environment_proxy(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            origin: Origin::LoopbackTest,
            use_environment_proxy: true,
        }
    }
}

/// Check that `base_url` is exactly the origin and path prefix `origin` allows.
///
/// Parsing rather than prefix-matching: `https://www.googleapis.com.evil.test/`
/// and `https://user@evil.test/?x=https://www.googleapis.com/` both start with
/// the right characters and neither is Google.
fn check_base_url(base_url: &str, origin: Origin) -> Result<(), String> {
    let parsed = reqwest::Url::parse(base_url)
        .map_err(|error| format!("the calendar API base is not a URL: {error}"))?;
    if !base_url.ends_with('/') {
        return Err("the calendar API base must end in `/`".to_string());
    }
    if parsed.username() != "" || parsed.password().is_some() {
        return Err("the calendar API base must carry no credentials".to_string());
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("the calendar API base must carry no query or fragment".to_string());
    }
    match origin {
        Origin::Google => {
            let (scheme, host, port) = GOOGLE_ORIGIN;
            if parsed.scheme() != scheme
                || parsed.host_str() != Some(host)
                || parsed.port_or_known_default() != Some(port)
            {
                return Err(format!(
                    "the calendar API base must be {scheme}://{host}, got `{base_url}`"
                ));
            }
            if !parsed.path().starts_with(GOOGLE_PATH_PREFIX) {
                return Err(format!(
                    "the calendar API base must be under {GOOGLE_PATH_PREFIX}, got `{base_url}`"
                ));
            }
            Ok(())
        }
        #[cfg(test)]
        Origin::LoopbackTest => {
            if parsed.host_str() != Some("127.0.0.1") {
                return Err(format!(
                    "a loopback test base must be on 127.0.0.1, got `{base_url}`"
                ));
            }
            Ok(())
        }
    }
}

/// The shipped HTTP transport.
///
/// Blocking on purpose: it is called from a blocking task, never from the
/// async runtime's thread.
///
/// The `Debug` rendering carries the base URL and nothing else: no request, no
/// header and no credential passes through this type's state.
#[derive(Debug)]
pub struct HttpTransport {
    client: reqwest::blocking::Client,
    base_url: String,
}

impl HttpTransport {
    /// Build a transport for `config`.
    ///
    /// # Errors
    /// Returns a message when the base URL is not the exact origin and path
    /// prefix the configuration's origin allows, or when the HTTP client
    /// cannot be built.
    pub fn new(config: &TransportConfig) -> Result<Self, String> {
        check_base_url(&config.base_url, config.origin)?;
        let mut builder = reqwest::blocking::Client::builder()
            // A redirect would replay the bearer token at whatever host the
            // response named.
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10));
        if !config.reads_environment_proxy() {
            builder = builder.no_proxy();
        }
        let client = builder.build().map_err(|error| error.to_string())?;
        Ok(Self {
            client,
            base_url: config.base_url.clone(),
        })
    }
}

impl CalendarTransport for HttpTransport {
    fn send(
        &self,
        request: &CalendarRequest,
        access_token: &Redacted<String>,
        timeout_ms: i64,
        max_body_bytes: usize,
    ) -> Result<HttpResponse, TransportError> {
        let method = match request.method {
            HttpMethod::Get => reqwest::Method::GET,
            HttpMethod::Post => reqwest::Method::POST,
            HttpMethod::Patch => reqwest::Method::PATCH,
            HttpMethod::Delete => reqwest::Method::DELETE,
        };
        let url = format!("{}{}", self.base_url, request.path);
        let mut builder = self
            .client
            .request(method, url)
            .timeout(Duration::from_millis(timeout_ms.max(1) as u64))
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", access_token.expose()),
            )
            .query(&request.query);
        if let Some(etag) = &request.if_match {
            builder = builder.header(reqwest::header::IF_MATCH, etag);
        }
        if let Some(body) = &request.body {
            builder = builder
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.clone());
        }
        // The error is rebuilt from the parts that cannot carry a header value:
        // reqwest's own message never includes headers, but this keeps the
        // guarantee local and testable.
        let response = builder
            .send()
            .map_err(|error| TransportError(redact_transport_error(&error)))?;
        let status = response.status().as_u16();
        let mut body = Vec::new();
        let read = response
            .take((max_body_bytes as u64).saturating_add(1))
            .read_to_end(&mut body)
            .map_err(|error| TransportError(format!("reading the response body: {error}")))?;
        let truncated_at_cap = read > max_body_bytes;
        if truncated_at_cap {
            body.truncate(max_body_bytes);
        }
        Ok(HttpResponse {
            status,
            body,
            truncated_at_cap,
        })
    }
}

/// A transport error message with no header, credential or URL in it.
///
/// Only reqwest's classification and its source chain are used; reqwest's own
/// `Display` is skipped because it names the request URL. The caller still
/// learns enough to tell a refused connection from a timeout.
fn redact_transport_error(error: &reqwest::Error) -> String {
    let kind = if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "could not connect"
    } else if error.is_request() {
        "could not be sent"
    } else {
        "failed"
    };
    // The source chain is hyper's and io's rendering, which carries no URL and
    // no header; reqwest's own `Display` is the part that would name the URL,
    // and it is deliberately not used here.
    let mut detail = String::new();
    let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(error);
    while let Some(cause) = source {
        detail.push_str(": ");
        detail.push_str(&cause.to_string());
        source = cause.source();
    }
    format!("the calendar request {kind}{detail}")
}
