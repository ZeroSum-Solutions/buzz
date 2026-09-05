//! Tests for the bounded walk and the three mutations, driven end to end
//! against the mock Google Calendar server.
//!
//! The transport, the walk, the caps, the classification and the parser are all
//! the shipped ones; only the server is a fixture.

use std::sync::atomic::{AtomicI64, Ordering};

use serde_json::json;

use super::client::{
    client_event_id, delete_event, fetch_events, insert_event, patch_event, CalendarRequest,
    CalendarTransport, Clock, FetchError, FetchLimits, HttpMethod, HttpTransport, MutationError,
    TransportConfig,
};
use super::dto::AccessRole;
use super::failure::{FailureState, MutationReason, TerminalReason, TransientReason};
use super::interval::{Coverage, TruncationReason, Window};
use super::mock_server::{MockGoogle, ALICE_TOKEN, BOB_TOKEN, CALENDAR_ID};
use super::redact::Redacted;
use super::testing::SENTINEL;

// ── The bounded walk and the mutations, over the mock server ──────────────

struct FixedClock(AtomicI64, i64);

impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0.fetch_add(self.1, Ordering::Relaxed)
    }
}

fn frozen_clock() -> FixedClock {
    FixedClock(AtomicI64::new(0), 0)
}

fn transport_for(server: &MockGoogle) -> HttpTransport {
    HttpTransport::new(&TransportConfig::loopback(server.base_url()))
        .expect("a loopback transport builds")
}

fn window() -> Window {
    Window::new(0, 30 * 24 * 60 * 60 * 1000)
}

#[test]
fn google_calendar_two_principals_see_the_shared_calendar_with_their_own_roles() {
    let server = MockGoogle::start(3, 1_000_000);
    let transport = transport_for(&server);
    let clock = frozen_clock();

    let alice = fetch_events(
        &transport,
        &clock,
        CALENDAR_ID,
        window(),
        &Redacted::new(ALICE_TOKEN.to_string()),
        &FetchLimits::default(),
    )
    .expect("the owner reads the shared calendar");
    assert_eq!(alice.access_role, AccessRole::Owner);
    assert_eq!(alice.events.len(), 3);
    assert!(alice.events.iter().all(|event| event.can_edit));
    assert_eq!(alice.interval.coverage, Coverage::Complete);

    let bob = fetch_events(
        &transport,
        &clock,
        CALENDAR_ID,
        window(),
        &Redacted::new(BOB_TOKEN.to_string()),
        &FetchLimits::default(),
    )
    .expect("the reader reads the same calendar");
    assert_eq!(bob.access_role, AccessRole::Reader);
    assert_eq!(bob.events.len(), 3);
    assert!(
        bob.events
            .iter()
            .all(|event| !event.can_edit && !event.can_delete),
        "a reader gets no edit affordance on the same events"
    );
}

#[test]
fn google_calendar_acl_loss_is_terminal_and_purges() {
    let server = MockGoogle::start(2, 1_000_000);
    server.with_state(|state| {
        state
            .principals
            .get_mut(BOB_TOKEN)
            .expect("bob is a principal")
            .acl_lost = true;
    });
    let transport = transport_for(&server);
    let error = fetch_events(
        &transport,
        &frozen_clock(),
        CALENDAR_ID,
        window(),
        &Redacted::new(BOB_TOKEN.to_string()),
        &FetchLimits::default(),
    )
    .expect_err("a principal who lost the ACL cannot read");
    match error {
        FetchError::Failure(failure) => {
            assert_eq!(
                failure.state,
                FailureState::Terminal(TerminalReason::Forbidden)
            );
            assert!(
                failure.state.purges_cache(),
                "an ACL loss purges the cached rows"
            );
            assert!(
                failure.detail.contains("403"),
                "the state carries the answer behind it: {}",
                failure.detail
            );
        }
        other => panic!("expected a terminal failure, got {other}"),
    }
    // The other principal is unaffected: access is per-account, not per-app.
    fetch_events(
        &transport,
        &frozen_clock(),
        CALENDAR_ID,
        window(),
        &Redacted::new(ALICE_TOKEN.to_string()),
        &FetchLimits::default(),
    )
    .expect("the owner still reads it");
}

#[test]
fn google_calendar_walk_stops_at_the_page_cap_and_proves_only_what_completed() {
    let server = MockGoogle::start(5, 1_000_000);
    server.with_state(|state| state.page_size = 1);
    let transport = transport_for(&server);
    let limits = FetchLimits {
        max_pages: 2,
        ..FetchLimits::default()
    };
    let batch = fetch_events(
        &transport,
        &frozen_clock(),
        CALENDAR_ID,
        window(),
        &Redacted::new(ALICE_TOKEN.to_string()),
        &limits,
    )
    .expect("a capped walk still returns what it proved");
    assert_eq!(batch.pages_fetched, 2);
    assert_eq!(batch.events.len(), 2);
    assert_eq!(
        batch.interval.coverage,
        Coverage::Truncated(TruncationReason::PageCap)
    );
    assert_eq!(
        batch.interval.end_ms, 4_600_000,
        "the bound is the last complete page's maximum start, never its end"
    );
}

#[test]
fn google_calendar_walk_stops_at_the_byte_cap_without_parsing_a_cut_page() {
    let server = MockGoogle::start(2, 1_000_000);
    server.with_state(|state| state.page_filler_bytes = 40_000);
    let transport = transport_for(&server);
    let limits = FetchLimits {
        max_total_bytes: 4_096,
        ..FetchLimits::default()
    };
    let batch = fetch_events(
        &transport,
        &frozen_clock(),
        CALENDAR_ID,
        window(),
        &Redacted::new(ALICE_TOKEN.to_string()),
        &limits,
    )
    .expect("a cut page is discarded, not an error");
    assert_eq!(
        batch.interval.coverage,
        Coverage::Truncated(TruncationReason::ByteCap)
    );
    assert!(batch.events.is_empty());
    assert!(
        batch.interval.is_empty(),
        "zero complete pages prove nothing"
    );
    assert!(batch.bytes_read <= limits.max_total_bytes + 1);
}

#[test]
fn google_calendar_walk_stops_at_the_deadline() {
    let server = MockGoogle::start(4, 1_000_000);
    server.with_state(|state| state.page_size = 1);
    let transport = transport_for(&server);
    // Every clock read jumps 20 seconds, so the 30-second deadline passes after
    // the first page without the test sleeping.
    let clock = FixedClock(AtomicI64::new(0), 20_000);
    let batch = fetch_events(
        &transport,
        &clock,
        CALENDAR_ID,
        window(),
        &Redacted::new(ALICE_TOKEN.to_string()),
        &FetchLimits::default(),
    )
    .expect("a deadline truncates rather than failing");
    assert_eq!(
        batch.interval.coverage,
        Coverage::Truncated(TruncationReason::Deadline)
    );
    assert_eq!(batch.pages_fetched, 1);
}

#[test]
fn google_calendar_walk_stops_at_the_event_cap() {
    let server = MockGoogle::start(4, 1_000_000);
    server.with_state(|state| state.page_size = 2);
    let transport = transport_for(&server);
    let limits = FetchLimits {
        max_events: 2,
        ..FetchLimits::default()
    };
    let batch = fetch_events(
        &transport,
        &frozen_clock(),
        CALENDAR_ID,
        window(),
        &Redacted::new(ALICE_TOKEN.to_string()),
        &limits,
    )
    .expect("the event cap truncates");
    assert_eq!(batch.events.len(), 2);
    assert_eq!(
        batch.interval.coverage,
        Coverage::Truncated(TruncationReason::EventCap)
    );
}

#[test]
fn google_calendar_unknown_token_is_a_failure_not_an_empty_calendar() {
    let server = MockGoogle::start(2, 1_000_000);
    let transport = transport_for(&server);
    let error = fetch_events(
        &transport,
        &frozen_clock(),
        CALENDAR_ID,
        window(),
        &Redacted::new("not-a-token".to_string()),
        &FetchLimits::default(),
    )
    .expect_err("an unauthenticated walk is never an empty success");
    match error {
        FetchError::Failure(failure) => assert_eq!(
            failure.state,
            FailureState::Transient(TransientReason::NeedsRefresh)
        ),
        other => panic!("expected a classified failure, got {other}"),
    }
}

#[test]
fn google_calendar_transport_pins_https_and_never_reads_a_proxy_from_the_environment() {
    let config = TransportConfig::google();
    assert!(!config.reads_environment_proxy());
    assert!(config.base_url.starts_with("https://"));
    let plaintext = TransportConfig::loopback("http://calendar.internal/");
    let error = HttpTransport::new(&plaintext).expect_err("a plaintext base is refused");
    assert!(
        error.contains("https"),
        "only a loopback test server may be plaintext: {error}"
    );
    let no_slash = TransportConfig::loopback("https://www.googleapis.com/calendar/v3");
    assert!(
        HttpTransport::new(&no_slash).is_err(),
        "a base without a trailing slash would compose the wrong URL"
    );
}

#[test]
fn google_calendar_transport_does_not_follow_a_redirect() {
    let server = MockGoogle::start(0, 0);
    let transport = transport_for(&server);
    let response = transport
        .send(
            &CalendarRequest {
                method: HttpMethod::Get,
                path: "redirect-probe".to_string(),
                query: Vec::new(),
                body: None,
                if_match: None,
            },
            &Redacted::new(ALICE_TOKEN.to_string()),
            5_000,
            1024,
        )
        .expect("the redirect is returned, not followed");
    assert_eq!(
        response.status, 302,
        "a followed redirect would replay the bearer token at another host"
    );
}

#[test]
fn google_calendar_transport_error_carries_no_credential() {
    let transport = HttpTransport::new(&TransportConfig::loopback(
        "http://127.0.0.1:1/".to_string(),
    ))
    .expect("the transport builds");
    let error = transport
        .send(
            &CalendarRequest {
                method: HttpMethod::Get,
                path: "calendars".to_string(),
                query: vec![("q".to_string(), SENTINEL.to_string())],
                body: None,
                if_match: None,
            },
            &Redacted::new(SENTINEL.to_string()),
            250,
            1024,
        )
        .expect_err("nothing is listening on port 1");
    let rendered = format!("{error}");
    assert!(
        !rendered.contains(SENTINEL),
        "the error names no credential"
    );
    assert!(!rendered.is_empty(), "the failure is still surfaced");
}

#[test]
fn google_calendar_client_event_id_is_google_shaped() {
    let id = client_event_id();
    assert_eq!(id.len(), 32);
    assert!(
        id.chars()
            .all(|character| character.is_ascii_digit() || ('a'..='v').contains(&character)),
        "a client id must sit inside Google's base32hex alphabet: {id}"
    );
    assert_ne!(id, client_event_id(), "each create gets its own id");
}

#[test]
fn google_calendar_create_is_replayable_with_the_same_id() {
    let server = MockGoogle::start(0, 0);
    let transport = transport_for(&server);
    let token = Redacted::new(ALICE_TOKEN.to_string());
    let id = client_event_id();
    let fields = json!({
        "summary": "Sprint review",
        "start": { "dateTime": "2026-09-10T10:00:00Z" },
        "end": { "dateTime": "2026-09-10T11:00:00Z" },
    });
    let created =
        insert_event(&transport, CALENDAR_ID, &id, &fields, &token).expect("the create succeeds");
    assert_eq!(created.id, id);

    let replay = insert_event(&transport, CALENDAR_ID, &id, &fields, &token)
        .expect_err("a replay of the same id does not create a second event");
    match replay {
        MutationError::Failure(failure) => assert_eq!(
            failure.state,
            FailureState::MutationRejected(MutationReason::Unclassified),
            "the duplicate is rejected as this command alone"
        ),
        other => panic!("expected a classified failure, got {other}"),
    }
    server.with_state(|state| assert_eq!(state.events.len(), 1));
}

#[test]
fn google_calendar_patch_and_delete_are_fenced_by_the_etag() {
    let server = MockGoogle::start(1, 1_000_000);
    let transport = transport_for(&server);
    let token = Redacted::new(ALICE_TOKEN.to_string());
    let stale = patch_event(
        &transport,
        CALENDAR_ID,
        "event-0",
        "\"stale-etag\"",
        &json!({ "summary": "Renamed" }),
        &token,
    )
    .expect_err("a stale etag never overwrites a co-organizer's edit");
    match stale {
        MutationError::Failure(failure) => assert_eq!(
            failure.state,
            FailureState::MutationRejected(MutationReason::EtagConflict)
        ),
        other => panic!("expected a classified failure, got {other}"),
    }

    let patched = patch_event(
        &transport,
        CALENDAR_ID,
        "event-0",
        "\"etag-event-0\"",
        &json!({ "summary": "Renamed" }),
        &token,
    )
    .expect("the current etag patches");
    assert_eq!(patched.summary.value, "Renamed");

    match delete_event(
        &transport,
        CALENDAR_ID,
        "event-0",
        "\"etag-event-0\"",
        &token,
    ) {
        Err(MutationError::Failure(failure)) => assert_eq!(
            failure.state,
            FailureState::MutationRejected(MutationReason::EtagConflict),
            "a stale etag never deletes"
        ),
        other => panic!("expected an etag conflict, got {other:?}"),
    }
    delete_event(
        &transport,
        CALENDAR_ID,
        "event-0",
        patched.etag.as_deref().expect("the patch returned an etag"),
        &token,
    )
    .expect("the refreshed etag deletes");
    server.with_state(|state| assert!(state.events.is_empty()));
}

#[test]
fn google_calendar_missing_event_is_ambiguous_until_the_calendar_is_probed() {
    let server = MockGoogle::start(0, 0);
    let transport = transport_for(&server);
    let error = patch_event(
        &transport,
        CALENDAR_ID,
        "nothing-here",
        "\"etag\"",
        &json!({ "summary": "x" }),
        &Redacted::new(ALICE_TOKEN.to_string()),
    )
    .expect_err("a 404 is surfaced");
    match error {
        MutationError::Failure(failure) => assert_eq!(
            failure.state,
            FailureState::MutationRejected(MutationReason::EventMissingNeedsProbe),
            "a 404 never purges on its own; detail: {}",
            failure.detail
        ),
        other => panic!("expected a classified failure, got {other}"),
    }
}

#[test]
fn google_calendar_default_window_is_the_120_day_horizon() {
    let window = super::default_window(0);
    assert_eq!(window.start_ms, -30 * 24 * 60 * 60 * 1000);
    assert_eq!(window.end_ms, 90 * 24 * 60 * 60 * 1000);
    assert_eq!(super::stale_after_ms(1_000), 1_000 + super::STALE_AFTER_MS);
}
