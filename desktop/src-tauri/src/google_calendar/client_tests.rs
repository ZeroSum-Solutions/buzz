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
fn google_calendar_transport_pins_the_google_origin_and_path() {
    let config = TransportConfig::google();
    assert!(!config.reads_environment_proxy());
    assert_eq!(config.base_url(), "https://www.googleapis.com/calendar/v3/");
    assert!(HttpTransport::new(&config).is_ok());

    // Every one of these starts with the right characters and none of them is
    // Google. A prefix check on the base URL accepts them; parsing the origin
    // does not.
    for rejected in [
        "http://www.googleapis.com/calendar/v3/",
        "https://www.googleapis.com.evil.test/calendar/v3/",
        "https://www.googleapis.com:8443/calendar/v3/",
        "https://www.googleapis.com/drive/v3/",
        "https://www.googleapis.com/calendar/v3",
        "https://user:secret@www.googleapis.com/calendar/v3/",
        "https://www.googleapis.com/calendar/v3/?to=https://evil.test/",
        "not-a-url",
    ] {
        let mut config = TransportConfig::google();
        config.force_base_url_for_test(rejected);
        assert!(
            HttpTransport::new(&config).is_err(),
            "`{rejected}` is not the Google Calendar origin and must not carry a bearer token"
        );
    }
}

/// A listener that accepts and immediately closes, counting what reached it.
struct ProxySentinel {
    addr: std::net::SocketAddr,
    seen: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ProxySentinel {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
            .expect("the proxy sentinel binds a loopback port");
        let addr = listener.local_addr().expect("the sentinel has an address");
        listener
            .set_nonblocking(true)
            .expect("the sentinel polls for shutdown");
        let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_seen = std::sync::Arc::clone(&seen);
        let worker_stop = std::sync::Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok(_) => {
                        worker_seen.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(5)),
                }
            }
        });
        Self {
            addr,
            seen,
            stop,
            handle: Some(handle),
        }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }

    fn connections(&self) -> usize {
        self.seen.load(Ordering::Relaxed)
    }
}

impl Drop for ProxySentinel {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Build a transport with every proxy variable pointing at `proxy`.
///
/// reqwest reads the environment once, when the client is built, so the
/// variables are set only around that call and restored immediately. The
/// process environment is global, and this is the narrowest window in which the
/// containment can be observed at all: nothing else in this module reads a
/// proxy variable, because every shipped configuration disables proxy lookup.
fn transport_with_proxy_env(config: &TransportConfig, proxy: &str) -> HttpTransport {
    const VARS: [&str; 6] = [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ];
    let saved: Vec<(&str, Option<String>)> = VARS
        .iter()
        .map(|key| (*key, std::env::var(key).ok()))
        .collect();
    for key in VARS {
        std::env::set_var(key, proxy);
    }
    let built = HttpTransport::new(config);
    for (key, previous) in saved {
        match previous {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
    built.expect("a loopback transport builds")
}

#[test]
fn google_calendar_transport_never_routes_a_bearer_token_through_an_environment_proxy() {
    // The containment this binds: a managed agent runs at operator trust and
    // can set `HTTPS_PROXY` in this process's environment. Without
    // `no_proxy()`, every `Authorization: Bearer` header would go to a host the
    // agent named. The negative control below proves the assertion can fail.
    let server = MockGoogle::start(1, 1_000_000);
    let sentinel = ProxySentinel::start();
    let base = server.base_url();

    // Negative control: a configuration that *does* read the environment sends
    // the request to the sentinel instead of the server.
    let leaky = transport_with_proxy_env(
        &TransportConfig::loopback_reading_environment_proxy(base.clone()),
        &sentinel.url(),
    );
    let leaked = leaky.send(
        &CalendarRequest {
            method: HttpMethod::Get,
            path: "calendars/probe/events".to_string(),
            query: Vec::new(),
            body: None,
            if_match: None,
        },
        &Redacted::new(ALICE_TOKEN.to_string()),
        2_000,
        1024,
    );
    assert!(
        leaked.is_err(),
        "the sentinel is not a proxy, so a routed request cannot succeed"
    );
    assert!(
        sentinel.connections() > 0,
        "the negative control must actually reach the sentinel, or this test proves nothing"
    );
    let routed = sentinel.connections();
    server.with_state(|state| state.requests.clear());

    // The shipped configuration, with the same environment set at build time,
    // reaches the server directly.
    let pinned = transport_with_proxy_env(&TransportConfig::loopback(base), &sentinel.url());
    let response = pinned
        .send(
            &CalendarRequest {
                method: HttpMethod::Get,
                path: format!("calendars/{CALENDAR_ID}/events"),
                query: vec![("maxResults".to_string(), "1".to_string())],
                body: None,
                if_match: None,
            },
            &Redacted::new(ALICE_TOKEN.to_string()),
            5_000,
            64 * 1024,
        )
        .expect("the pinned transport reaches the loopback server directly");
    assert_eq!(response.status, 200);
    assert_eq!(
        server.with_state(|state| state.requests.len()),
        1,
        "the request must arrive at the server itself"
    );
    assert_eq!(
        sentinel.connections(),
        routed,
        "no part of the pinned request may reach the proxy the environment named"
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

#[test]
fn google_calendar_walk_never_accumulates_past_the_event_cap() {
    // The overshoot this binds: extending `events` with a whole page and only
    // then checking the cap returns up to `max_events + page - 1` events. With
    // a page of 4 and a cap of 3 that is 4, over T11 decision 6's row bound.
    let server = MockGoogle::start(8, 1_000_000);
    server.with_state(|state| state.page_size = 4);
    let transport = transport_for(&server);
    let limits = FetchLimits {
        max_events: 3,
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
    assert_eq!(
        batch.events.len(),
        3,
        "the walk must never hold more events than the cap allows"
    );
    assert_eq!(
        batch.interval.coverage,
        Coverage::Truncated(TruncationReason::EventCap)
    );
    // The dropped fourth event of the page shares the page's largest start, so
    // the proven interval must end at the last *kept* start, not the page's.
    let last_kept_start = batch
        .events
        .last()
        .and_then(|event| event.start.start_lower_bound_ms())
        .expect("a timed event has a start");
    assert_eq!(batch.interval.end_ms, last_kept_start);
    assert!(!batch.interval.proves_instant(last_kept_start));
}

#[test]
fn google_calendar_walk_binds_the_events_list_query_google_is_asked_for() {
    // Deleting any of these from the query leaves the walk working against the
    // mock and silently wrong against Google: without `singleEvents` a
    // recurring series arrives unexpanded (T12a decision 5 puts expansion at
    // Google), without `orderBy=startTime` the proven interval of decision 13
    // is unsound because pages are unordered, and without `timeMin`/`timeMax`
    // the walk asks for the whole calendar rather than T11 decision 6's window.
    let server = MockGoogle::start(3, 1_000_000);
    let transport = transport_for(&server);
    let window = Window::new(1_000_000, 1_000_000 + 7 * 24 * 60 * 60 * 1000);
    fetch_events(
        &transport,
        &frozen_clock(),
        CALENDAR_ID,
        window,
        &Redacted::new(ALICE_TOKEN.to_string()),
        &FetchLimits::default(),
    )
    .expect("the walk completes");

    let (method, path, query) = server.with_state(|state| {
        state
            .requests
            .first()
            .cloned()
            .expect("the walk made a request")
    });
    assert_eq!(method, "GET");
    let decoded_path = percent_encoding::percent_decode_str(&path)
        .decode_utf8_lossy()
        .to_string();
    assert_eq!(decoded_path, format!("/calendars/{CALENDAR_ID}/events"));
    let pairs: std::collections::HashMap<String, String> = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| {
            (
                key.to_string(),
                percent_encoding::percent_decode_str(value)
                    .decode_utf8_lossy()
                    .to_string(),
            )
        })
        .collect();
    assert_eq!(pairs.get("singleEvents").map(String::as_str), Some("true"));
    assert_eq!(pairs.get("orderBy").map(String::as_str), Some("startTime"));
    assert_eq!(
        pairs.get("maxResults").map(String::as_str),
        Some(super::client::PAGE_SIZE.to_string().as_str())
    );
    let (time_min, time_max) =
        super::dto::window_bounds_rfc3339(window).expect("the window is expressible");
    assert_eq!(pairs.get("timeMin"), Some(&time_min));
    assert_eq!(pairs.get("timeMax"), Some(&time_max));
    assert!(
        !query.contains("Bearer") && !query.contains(ALICE_TOKEN),
        "no credential may appear in a URL: {query}"
    );
}
