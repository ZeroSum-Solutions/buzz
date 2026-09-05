//! The bounded event DTO of T12a decision 1, and the parser that is the only
//! way API-sourced text enters the fork.
//!
//! Every string Google returns is capped here, at the boundary, with one
//! `truncated` flag per field rather than one per event — T12a decision 1 makes
//! a truncated field read-only (decision 11), so the flag has to say *which*
//! field. The caps also hold one row inside T11 decision 6's 256 KiB bound.
//!
//! Editability is two booleans, `can_edit` and `can_delete`: T11 decision 3's
//! `accessRole` narrowed by event type and organizer. An unrecognized role or
//! event type is read-only — the narrowing fails closed.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::interval::Window;

/// Largest accepted `summary`, in characters.
pub const MAX_SUMMARY_CHARS: usize = 256;
/// Largest accepted `location`, in characters.
pub const MAX_LOCATION_CHARS: usize = 256;
/// Largest accepted `description`, in characters.
pub const MAX_DESCRIPTION_CHARS: usize = 4096;
/// Largest accepted event id, in characters (Google's own limit is 1024).
pub const MAX_ID_CHARS: usize = 1024;
/// Largest accepted `etag`, in characters.
pub const MAX_ETAG_CHARS: usize = 256;
/// Largest accepted IANA zone name, in characters.
pub const MAX_TIME_ZONE_CHARS: usize = 64;
/// Largest accepted `nextPageToken`, in characters.
pub const MAX_PAGE_TOKEN_CHARS: usize = 2048;
/// Largest accepted number of events in one page (Google's `maxResults` cap).
pub const MAX_EVENTS_PER_PAGE: usize = 2500;

/// Milliseconds in the widest positive UTC offset any IANA zone uses (UTC+14).
const MAX_ZONE_OFFSET_MS: i64 = 14 * 60 * 60 * 1000;

/// A capped string plus whether capping dropped anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TruncatedText {
    /// The capped value.
    pub value: String,
    /// Whether the source was longer than the cap.
    pub truncated: bool,
}

impl TruncatedText {
    /// Cap `raw` at `max_chars`, cutting on a character boundary.
    pub fn cap(raw: &str, max_chars: usize) -> Self {
        let mut value = String::new();
        let mut truncated = false;
        for (index, character) in raw.chars().enumerate() {
            if index >= max_chars {
                truncated = true;
                break;
            }
            value.push(character);
        }
        Self { value, truncated }
    }

    /// The empty value, which nothing truncated.
    pub fn empty() -> Self {
        Self {
            value: String::new(),
            truncated: false,
        }
    }
}

/// A `start` or `end` value. T12a decision 2 keeps the all-day variant a date,
/// never an instant, so a one-day event renders on exactly one day.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum EventTime {
    /// A half-open day range in the calendar's zone; `end.date` is exclusive.
    AllDay {
        /// `YYYY-MM-DD` in the calendar's zone.
        date: String,
    },
    /// An instant, plus the zone the source named.
    Timed {
        /// Milliseconds since the Unix epoch.
        instant_ms: i64,
        /// The source zone, when the API named one.
        time_zone: Option<String>,
    },
}

impl EventTime {
    /// The earliest instant this value can represent in any IANA zone.
    ///
    /// Ordering a page by start needs one comparable number, but an all-day
    /// value has no instant (decision 2). Using the earliest instant the date
    /// can begin anywhere (UTC+14) keeps T12a decision 13's proven bound
    /// conservative: the bound can only fall short of the truth, never past it.
    /// It is an ordering key, never a render instant.
    pub fn start_lower_bound_ms(&self) -> Option<i64> {
        match self {
            EventTime::Timed { instant_ms, .. } => Some(*instant_ms),
            EventTime::AllDay { date } => {
                parse_date_utc_ms(date).map(|midnight| midnight - MAX_ZONE_OFFSET_MS)
            }
        }
    }
}

/// Google's `status` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventStatus {
    /// `confirmed`.
    Confirmed,
    /// `tentative`.
    Tentative,
    /// `cancelled`.
    Cancelled,
    /// Anything else, including absent.
    Unknown,
}

impl EventStatus {
    fn from_api(raw: Option<&str>) -> Self {
        match raw {
            Some("confirmed") | None => EventStatus::Confirmed,
            Some("tentative") => EventStatus::Tentative,
            Some("cancelled") => EventStatus::Cancelled,
            Some(_) => EventStatus::Unknown,
        }
    }
}

/// The caller's role on the whole calendar (T11 decision 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessRole {
    /// `owner`.
    Owner,
    /// `writer`.
    Writer,
    /// `reader`.
    Reader,
    /// `freeBusyReader`.
    FreeBusyReader,
    /// Anything else, including absent. Read-only: the narrowing fails closed.
    Unknown,
}

impl AccessRole {
    /// Map Google's `accessRole` string. An unrecognized role is read-only.
    pub fn from_api(raw: Option<&str>) -> Self {
        match raw {
            Some("owner") => AccessRole::Owner,
            Some("writer") => AccessRole::Writer,
            Some("reader") => AccessRole::Reader,
            Some("freeBusyReader") => AccessRole::FreeBusyReader,
            _ => AccessRole::Unknown,
        }
    }
}

/// Google's `eventType`. Only the two Buzz can render and edit are writable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// `default`.
    Default,
    /// `outOfOffice`.
    OutOfOffice,
    /// A type Buzz renders read-only (`birthday`, `fromGmail`, `focusTime`,
    /// `workingLocation`) or does not recognize.
    Other,
}

impl EventKind {
    fn from_api(raw: Option<&str>) -> Self {
        match raw {
            Some("default") | None => EventKind::Default,
            Some("outOfOffice") => EventKind::OutOfOffice,
            _ => EventKind::Other,
        }
    }
}

/// What the viewer may do to one event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capability {
    /// Whether `events.patch` is offered.
    pub can_edit: bool,
    /// Whether `events.delete` is offered.
    pub can_delete: bool,
}

/// Narrow a calendar role to one event (T12a decision 1).
///
/// A writer may edit any event on the calendar but may delete only one this
/// account organizes; every other role, an unrecognized event type and a
/// cancelled event are read-only.
pub fn derive_capability(
    role: AccessRole,
    kind: EventKind,
    status: EventStatus,
    organizer_is_self: bool,
) -> Capability {
    let read_only = Capability {
        can_edit: false,
        can_delete: false,
    };
    if status == EventStatus::Cancelled || kind == EventKind::Other {
        return read_only;
    }
    match role {
        AccessRole::Owner => Capability {
            can_edit: true,
            can_delete: true,
        },
        AccessRole::Writer => Capability {
            can_edit: true,
            can_delete: organizer_is_self,
        },
        AccessRole::Reader | AccessRole::FreeBusyReader | AccessRole::Unknown => read_only,
    }
}

/// One event, capped and narrowed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarEvent {
    /// Google's opaque id. Every later decision addresses the event by it.
    pub id: String,
    /// The `etag`, sent back as `If-Match` on a mutation (T12a decision 11).
    pub etag: Option<String>,
    /// Google's `status`.
    pub status: EventStatus,
    /// Set when this is one instance of a recurring series. Display metadata
    /// only: no RRULE parser enters the fork (T12a decision 5).
    pub recurring_event_id: Option<String>,
    /// Start value.
    pub start: EventTime,
    /// End value, exclusive.
    pub end: EventTime,
    /// Capped `summary`.
    pub summary: TruncatedText,
    /// Capped `location`.
    pub location: TruncatedText,
    /// Capped `description`.
    pub description: TruncatedText,
    /// Whether `events.patch` is offered for this event.
    pub can_edit: bool,
    /// Whether `events.delete` is offered for this event.
    pub can_delete: bool,
}

/// The three text fields a form can send back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventField {
    /// `summary`.
    Summary,
    /// `location`.
    Location,
    /// `description`.
    Description,
}

impl CalendarEvent {
    /// Whether one field may be edited.
    ///
    /// T12a decision 11: a field whose `truncated` flag is set is read-only, so
    /// a capped prefix can never overwrite Google's copy.
    pub fn can_edit_field(&self, field: EventField) -> bool {
        if !self.can_edit {
            return false;
        }
        !match field {
            EventField::Summary => self.summary.truncated,
            EventField::Location => self.location.truncated,
            EventField::Description => self.description.truncated,
        }
    }
}

/// One `events.list` page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventsPage {
    /// The events it carried, in API order.
    pub events: Vec<CalendarEvent>,
    /// Continuation token, when Google had more.
    pub next_page_token: Option<String>,
    /// The caller's role on the calendar.
    pub access_role: AccessRole,
    /// The calendar's own zone, when the response named one.
    pub default_time_zone: Option<String>,
    /// Cancelled instances that carried no start or end and were dropped.
    /// Reported, never silently discarded.
    pub dropped_cancelled: usize,
}

impl EventsPage {
    /// The largest start this page carried, as an ordering lower bound.
    ///
    /// `None` when the page carried no event with a usable start, which proves
    /// no interval (T12a decision 13).
    pub fn max_start_lower_bound_ms(&self) -> Option<i64> {
        self.events
            .iter()
            .filter_map(|event| event.start.start_lower_bound_ms())
            .max()
    }
}

/// Why a page or an event could not be accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DtoError {
    /// The page body was over the byte cap the caller passed.
    PageTooLarge {
        /// Body size.
        bytes: usize,
        /// The cap it exceeded.
        cap: usize,
    },
    /// The body was not the JSON object `events.list` returns.
    Malformed(String),
    /// The page carried more items than [`MAX_EVENTS_PER_PAGE`].
    TooManyItems {
        /// Item count.
        count: usize,
        /// The cap it exceeded.
        cap: usize,
    },
    /// One item was missing a field with no defensible default.
    MissingField {
        /// Index of the item in the page.
        index: usize,
        /// The field name.
        field: &'static str,
    },
    /// One item carried a field the parser could not read.
    InvalidField {
        /// Index of the item in the page.
        index: usize,
        /// The field name.
        field: &'static str,
    },
}

impl std::fmt::Display for DtoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DtoError::PageTooLarge { bytes, cap } => {
                write!(f, "events.list page is {bytes} bytes, over the {cap} cap")
            }
            DtoError::Malformed(detail) => write!(f, "events.list page is malformed: {detail}"),
            DtoError::TooManyItems { count, cap } => {
                write!(
                    f,
                    "events.list page carried {count} items, over the {cap} cap"
                )
            }
            DtoError::MissingField { index, field } => {
                write!(f, "event {index} is missing `{field}`")
            }
            DtoError::InvalidField { index, field } => {
                write!(f, "event {index} has an unreadable `{field}`")
            }
        }
    }
}

impl std::error::Error for DtoError {}

/// Parse one `events.list` page.
///
/// `max_bytes` is the caller's remaining byte budget (T11 decision 6), checked
/// before any parsing so an oversized body costs one length comparison.
///
/// # Errors
/// Returns [`DtoError`] when the body is over `max_bytes`, is not an
/// `events.list` object, carries more than [`MAX_EVENTS_PER_PAGE`] items, or
/// holds an item the parser cannot read. Nothing is dropped silently: a
/// cancelled instance with no start or end is counted in
/// [`EventsPage::dropped_cancelled`].
pub fn parse_events_page(body: &[u8], max_bytes: usize) -> Result<EventsPage, DtoError> {
    if body.len() > max_bytes {
        return Err(DtoError::PageTooLarge {
            bytes: body.len(),
            cap: max_bytes,
        });
    }
    let root: Value =
        serde_json::from_slice(body).map_err(|error| DtoError::Malformed(error.to_string()))?;
    let object = root
        .as_object()
        .ok_or_else(|| DtoError::Malformed("body is not an object".to_string()))?;

    let access_role = AccessRole::from_api(object.get("accessRole").and_then(Value::as_str));
    let default_time_zone = object
        .get("timeZone")
        .and_then(Value::as_str)
        .map(|zone| TruncatedText::cap(zone, MAX_TIME_ZONE_CHARS).value);
    let next_page_token = object
        .get("nextPageToken")
        .and_then(Value::as_str)
        .map(|token| TruncatedText::cap(token, MAX_PAGE_TOKEN_CHARS).value)
        .filter(|token| !token.is_empty());

    const EMPTY: &[Value] = &[];
    let items: &[Value] = match object.get("items") {
        None | Some(Value::Null) => EMPTY,
        Some(Value::Array(items)) => items.as_slice(),
        Some(_) => return Err(DtoError::Malformed("`items` is not an array".to_string())),
    };
    if items.len() > MAX_EVENTS_PER_PAGE {
        return Err(DtoError::TooManyItems {
            count: items.len(),
            cap: MAX_EVENTS_PER_PAGE,
        });
    }

    let mut events = Vec::with_capacity(items.len());
    let mut dropped_cancelled = 0usize;
    for (index, item) in items.iter().enumerate() {
        match parse_event(item, index, access_role)? {
            Some(event) => events.push(event),
            None => dropped_cancelled += 1,
        }
    }

    Ok(EventsPage {
        events,
        next_page_token,
        access_role,
        default_time_zone,
        dropped_cancelled,
    })
}

/// Parse a single-event response body (`events.insert` or `events.patch`).
///
/// # Errors
/// Returns [`DtoError`] when the body is over `max_bytes`, is not an event
/// object, or carries a shape with nothing to render.
pub fn parse_single_event(
    body: &[u8],
    max_bytes: usize,
    access_role: AccessRole,
) -> Result<CalendarEvent, DtoError> {
    if body.len() > max_bytes {
        return Err(DtoError::PageTooLarge {
            bytes: body.len(),
            cap: max_bytes,
        });
    }
    let value: Value =
        serde_json::from_slice(body).map_err(|error| DtoError::Malformed(error.to_string()))?;
    parse_event(&value, 0, access_role)?.ok_or_else(|| {
        DtoError::Malformed("the event response carried no start or end".to_string())
    })
}

/// Parse one item of an `events.list` page.
///
/// Returns `Ok(None)` for a cancelled instance carrying neither start nor end —
/// the one shape Google returns that has nothing to render. The caller counts
/// those rather than discarding them silently.
///
/// # Errors
/// Returns [`DtoError`] when the item is not an object, is missing `id`, or
/// carries a start or end the parser cannot read.
fn parse_event(
    item: &Value,
    index: usize,
    access_role: AccessRole,
) -> Result<Option<CalendarEvent>, DtoError> {
    let object = item.as_object().ok_or(DtoError::InvalidField {
        index,
        field: "items[]",
    })?;
    let status = EventStatus::from_api(object.get("status").and_then(Value::as_str));
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .ok_or(DtoError::MissingField { index, field: "id" })?;
    let id = TruncatedText::cap(id, MAX_ID_CHARS);
    if id.truncated || id.value.is_empty() {
        return Err(DtoError::InvalidField { index, field: "id" });
    }

    let start = object
        .get("start")
        .map(|value| parse_event_time(value, index, "start"));
    let end = object
        .get("end")
        .map(|value| parse_event_time(value, index, "end"));
    let (start, end) = match (start, end) {
        (Some(start), Some(end)) => (start?, end?),
        _ if status == EventStatus::Cancelled => return Ok(None),
        (None, _) => {
            return Err(DtoError::MissingField {
                index,
                field: "start",
            })
        }
        (_, None) => {
            return Err(DtoError::MissingField {
                index,
                field: "end",
            })
        }
    };

    let kind = EventKind::from_api(object.get("eventType").and_then(Value::as_str));
    let organizer_is_self = object
        .get("organizer")
        .and_then(Value::as_object)
        .and_then(|organizer| organizer.get("self"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let capability = derive_capability(access_role, kind, status, organizer_is_self);

    let text = |field: &str, cap: usize| {
        object
            .get(field)
            .and_then(Value::as_str)
            .map(|raw| TruncatedText::cap(raw, cap))
            .unwrap_or_else(TruncatedText::empty)
    };

    Ok(Some(CalendarEvent {
        id: id.value,
        etag: object
            .get("etag")
            .and_then(Value::as_str)
            .map(|etag| TruncatedText::cap(etag, MAX_ETAG_CHARS).value),
        status,
        recurring_event_id: object
            .get("recurringEventId")
            .and_then(Value::as_str)
            .map(|raw| TruncatedText::cap(raw, MAX_ID_CHARS).value),
        start,
        end,
        summary: text("summary", MAX_SUMMARY_CHARS),
        location: text("location", MAX_LOCATION_CHARS),
        description: text("description", MAX_DESCRIPTION_CHARS),
        can_edit: capability.can_edit,
        can_delete: capability.can_delete,
    }))
}

/// Parse a `{ date }` or `{ dateTime, timeZone? }` value.
fn parse_event_time(
    value: &Value,
    index: usize,
    field: &'static str,
) -> Result<EventTime, DtoError> {
    let object = value
        .as_object()
        .ok_or(DtoError::InvalidField { index, field })?;
    if let Some(date) = object.get("date").and_then(Value::as_str) {
        // Validated but deliberately not converted: decision 2 keeps an
        // all-day value a date in the calendar's zone.
        if parse_date_utc_ms(date).is_none() {
            return Err(DtoError::InvalidField { index, field });
        }
        return Ok(EventTime::AllDay {
            date: date.to_string(),
        });
    }
    let raw = object
        .get("dateTime")
        .and_then(Value::as_str)
        .ok_or(DtoError::MissingField { index, field })?;
    let instant = chrono::DateTime::parse_from_rfc3339(raw)
        .map_err(|_| DtoError::InvalidField { index, field })?;
    Ok(EventTime::Timed {
        instant_ms: instant.timestamp_millis(),
        time_zone: object
            .get("timeZone")
            .and_then(Value::as_str)
            .map(|zone| TruncatedText::cap(zone, MAX_TIME_ZONE_CHARS).value),
    })
}

/// Parse `YYYY-MM-DD` as milliseconds at UTC midnight, or `None` when the
/// string is not a valid date.
fn parse_date_utc_ms(date: &str) -> Option<i64> {
    let parsed = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    Some(
        parsed
            .and_time(chrono::NaiveTime::MIN)
            .and_utc()
            .timestamp_millis(),
    )
}

/// The `timeMin`/`timeMax` pair for a window, as RFC 3339 in UTC.
///
/// Returns `None` when either bound is outside the range a timestamp can
/// express. The caller surfaces that rather than substituting a default, which
/// would silently query a window nobody asked for.
pub fn window_bounds_rfc3339(window: Window) -> Option<(String, String)> {
    Some((
        to_rfc3339_utc(window.start_ms)?,
        to_rfc3339_utc(window.end_ms)?,
    ))
}

fn to_rfc3339_utc(instant_ms: i64) -> Option<String> {
    Some(
        chrono::DateTime::from_timestamp_millis(instant_ms)?
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    )
}
