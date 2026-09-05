//! Tests for the event contract: the bounded DTO, the proven interval, the
//! failure matrices, the authorization request and the loopback listener.
//!
//! Every one drives shipped code — the shipped parser, the shipped
//! classification, the shipped checks — so deleting a cap, a `state` check or a
//! `nonce` check fails a test here.

use serde_json::json;

use super::binding::CalendarEnvelope;
use super::dto::{
    derive_capability, parse_events_page, AccessRole, DtoError, EventField, EventKind, EventStatus,
    EventTime, TruncatedText, MAX_DESCRIPTION_CHARS, MAX_ETAG_CHARS, MAX_EVENTS_PER_PAGE,
    MAX_ID_CHARS, MAX_LOCATION_CHARS, MAX_PAGE_TOKEN_CHARS, MAX_SUMMARY_CHARS, MAX_TIME_ZONE_CHARS,
};
use super::failure::{
    classify_list, classify_mutation, classify_token, FailureState, MutationReason, TerminalReason,
    TransientReason, MAX_REASON_CHARS,
};
use super::interval::{Coverage, ProvenInterval, TruncationReason, Window};
use super::loopback::{CallbackListener, ListenerError, ListenerLimits};
use super::oauth::{
    check_exchange, verify_callback_query, verify_id_token, AuthRequest, CallbackError,
    ExchangeError, IdTokenError, IdTokenExpectations, IdTokenSignatureVerifier, PkcePair,
    MAX_ID_TOKEN_BYTES, SCOPES,
};
use super::redact::Redacted;
use super::testing::{binding, CLIENT_ID, SENTINEL};

// ── The bounded DTO (T12a decision 1) ─────────────────────────────────────

#[test]
fn google_calendar_caps_every_text_field_independently() {
    let page = json!({
        "accessRole": "owner",
        "items": [{
            "id": "e1",
            "summary": "s".repeat(MAX_SUMMARY_CHARS + 10),
            "location": "l".repeat(MAX_LOCATION_CHARS - 1),
            "description": "d".repeat(MAX_DESCRIPTION_CHARS + 1),
            "start": { "dateTime": "2026-09-05T10:00:00Z" },
            "end": { "dateTime": "2026-09-05T11:00:00Z" },
        }],
    })
    .to_string();
    let parsed = parse_events_page(page.as_bytes(), 1024 * 1024).expect("the page parses");
    let event = &parsed.events[0];
    assert_eq!(event.summary.value.chars().count(), MAX_SUMMARY_CHARS);
    assert!(event.summary.truncated, "an over-cap summary is flagged");
    assert!(
        !event.location.truncated,
        "a field inside the cap keeps its own flag clear"
    );
    assert_eq!(
        event.description.value.chars().count(),
        MAX_DESCRIPTION_CHARS
    );
    assert!(event.description.truncated);
}

#[test]
fn google_calendar_caps_on_a_character_boundary() {
    let capped = TruncatedText::cap(&"é".repeat(300), MAX_SUMMARY_CHARS);
    assert_eq!(capped.value.chars().count(), MAX_SUMMARY_CHARS);
    assert!(capped.truncated);
    assert!(
        capped.value.is_char_boundary(capped.value.len()),
        "the cap never splits a character"
    );
}

#[test]
fn google_calendar_page_refuses_more_items_than_the_cap() {
    let item = json!({
        "id": "e",
        "start": { "date": "2026-09-05" },
        "end": { "date": "2026-09-06" },
    });
    let items: Vec<serde_json::Value> = (0..MAX_EVENTS_PER_PAGE + 1)
        .map(|index| {
            let mut item = item.clone();
            item["id"] = json!(format!("e{index}"));
            item
        })
        .collect();
    let page = json!({ "accessRole": "reader", "items": items }).to_string();
    let error = parse_events_page(page.as_bytes(), 100 * 1024 * 1024)
        .expect_err("a page over the item cap is refused");
    assert!(
        format!("{error}").contains("over the"),
        "the refusal names the cap: {error}"
    );
}

#[test]
fn google_calendar_page_refuses_a_body_over_the_byte_budget() {
    let page = json!({ "accessRole": "reader", "items": [] }).to_string();
    let error = parse_events_page(page.as_bytes(), 4).expect_err("an over-budget body is refused");
    assert!(format!("{error}").contains("bytes"), "{error}");
}

#[test]
fn google_calendar_capability_narrows_by_role_type_and_organizer() {
    let owner = derive_capability(
        AccessRole::Owner,
        EventKind::Default,
        EventStatus::Confirmed,
        false,
    );
    assert!(owner.can_edit && owner.can_delete);

    let writer_guest = derive_capability(
        AccessRole::Writer,
        EventKind::Default,
        EventStatus::Confirmed,
        false,
    );
    assert!(
        writer_guest.can_edit && !writer_guest.can_delete,
        "a writer edits but deletes only what this account organizes"
    );

    for role in [
        AccessRole::Reader,
        AccessRole::FreeBusyReader,
        AccessRole::Unknown,
    ] {
        let capability = derive_capability(role, EventKind::Default, EventStatus::Confirmed, true);
        assert!(
            !capability.can_edit && !capability.can_delete,
            "{role:?} is read-only"
        );
    }
    let other_kind = derive_capability(
        AccessRole::Owner,
        EventKind::Other,
        EventStatus::Confirmed,
        true,
    );
    assert!(
        !other_kind.can_edit,
        "an unrecognized event type is read-only"
    );
    let cancelled = derive_capability(
        AccessRole::Owner,
        EventKind::Default,
        EventStatus::Cancelled,
        true,
    );
    assert!(!cancelled.can_edit, "a cancelled event is read-only");
}

#[test]
fn google_calendar_unknown_access_role_is_read_only() {
    let page = json!({
        "accessRole": "somethingNew",
        "items": [{
            "id": "e1",
            "start": { "dateTime": "2026-09-05T10:00:00Z" },
            "end": { "dateTime": "2026-09-05T11:00:00Z" },
        }],
    })
    .to_string();
    let parsed = parse_events_page(page.as_bytes(), 1024 * 1024).expect("the page parses");
    assert_eq!(parsed.access_role, AccessRole::Unknown);
    assert!(!parsed.events[0].can_edit);
    assert!(!parsed.events[0].can_delete);
}

#[test]
fn google_calendar_truncated_field_is_not_editable() {
    let page = json!({
        "accessRole": "owner",
        "items": [{
            "id": "e1",
            "summary": "ok",
            "description": "d".repeat(MAX_DESCRIPTION_CHARS + 1),
            "start": { "dateTime": "2026-09-05T10:00:00Z" },
            "end": { "dateTime": "2026-09-05T11:00:00Z" },
        }],
    })
    .to_string();
    let parsed = parse_events_page(page.as_bytes(), 1024 * 1024).expect("the page parses");
    let event = &parsed.events[0];
    assert!(event.can_edit_field(EventField::Summary));
    assert!(
        !event.can_edit_field(EventField::Description),
        "a capped prefix must never overwrite Google's copy"
    );
}

#[test]
fn google_calendar_all_day_value_stays_a_date() {
    let page = json!({
        "accessRole": "owner",
        "items": [{
            "id": "e1",
            "start": { "date": "2026-09-05" },
            "end": { "date": "2026-09-06" },
        }],
    })
    .to_string();
    let parsed = parse_events_page(page.as_bytes(), 1024 * 1024).expect("the page parses");
    assert_eq!(
        parsed.events[0].start,
        EventTime::AllDay {
            date: "2026-09-05".to_string()
        },
        "an all-day value is never converted to an instant"
    );
}

#[test]
fn google_calendar_malformed_event_is_surfaced_and_cancelled_ones_are_counted() {
    let missing_id = json!({
        "accessRole": "owner",
        "items": [{ "start": { "date": "2026-09-05" }, "end": { "date": "2026-09-06" } }],
    })
    .to_string();
    let error =
        parse_events_page(missing_id.as_bytes(), 1024 * 1024).expect_err("a missing id is fatal");
    assert!(format!("{error}").contains("id"), "{error}");

    let cancelled = json!({
        "accessRole": "owner",
        "items": [{ "id": "e1", "status": "cancelled" }],
    })
    .to_string();
    let parsed = parse_events_page(cancelled.as_bytes(), 1024 * 1024).expect("the page parses");
    assert!(parsed.events.is_empty());
    assert_eq!(
        parsed.dropped_cancelled, 1,
        "a dropped instance is reported, never discarded quietly"
    );
}

// ── The proven interval (T12a decision 13) ────────────────────────────────

#[test]
fn google_calendar_truncated_interval_ends_at_the_last_complete_page_start() {
    let window = Window::new(0, 10_000);
    let interval = ProvenInterval::truncated(window, Some(4_000), TruncationReason::PageCap);
    assert_eq!(interval.end_ms, 4_000, "never the maximum end");
    assert!(interval.proves_instant(3_999));
    assert!(
        !interval.proves_instant(4_000),
        "the bound itself is not proven: equal starts may sit on the next page"
    );
}

#[test]
fn google_calendar_zero_complete_pages_prove_nothing() {
    let window = Window::new(100, 10_000);
    let interval = ProvenInterval::truncated(window, None, TruncationReason::ByteCap);
    assert!(interval.is_empty());
    assert!(!interval.proves_instant(100));
}

#[test]
fn google_calendar_eviction_downgrades_a_complete_interval() {
    let mut interval = ProvenInterval::complete(Window::new(0, 10_000));
    assert_eq!(interval.coverage, Coverage::Complete);
    interval.downgrade_evicted();
    assert_eq!(
        interval.coverage,
        Coverage::Truncated(TruncationReason::Evicted),
        "`complete` holds only while its rows survive"
    );
}

#[test]
fn google_calendar_proves_only_days_wholly_inside_the_interval() {
    let interval = ProvenInterval::truncated(
        Window::new(0, 10_000),
        Some(5_000),
        TruncationReason::Deadline,
    );
    assert!(interval.proves_day(1_000, 2_000));
    assert!(
        !interval.proves_day(4_000, 6_000),
        "the day holding the bound is unknown, never empty"
    );
}

// ── The failure matrices (T11 decision 8) ─────────────────────────────────

#[test]
fn google_calendar_token_matrix_classifies_every_named_case() {
    assert_eq!(
        classify_token(Some(400), "invalid_grant"),
        FailureState::Terminal(TerminalReason::InvalidGrant)
    );
    assert_eq!(
        classify_token(Some(400), "invalid_scope"),
        FailureState::Terminal(TerminalReason::ScopeWithdrawn)
    );
    assert_eq!(
        classify_token(Some(401), "invalid_client"),
        FailureState::AppError
    );
    assert_eq!(
        classify_token(Some(503), ""),
        FailureState::Transient(TransientReason::ServerError)
    );
    assert_eq!(
        classify_token(None, ""),
        FailureState::Transient(TransientReason::Network)
    );
    assert_eq!(
        classify_token(Some(418), "teapot"),
        FailureState::Transient(TransientReason::Unclassified),
        "the default fails closed at stale_after rather than reading as revoked"
    );
}

#[test]
fn google_calendar_list_matrix_classifies_every_named_case() {
    assert_eq!(
        classify_list(Some(401), "", false),
        FailureState::Transient(TransientReason::NeedsRefresh)
    );
    assert_eq!(
        classify_list(Some(401), "", true),
        FailureState::Terminal(TerminalReason::Unauthorized),
        "only a 401 that survived a forced refresh is terminal"
    );
    assert_eq!(
        classify_list(Some(403), "rateLimitExceeded", false),
        FailureState::Transient(TransientReason::RateLimited)
    );
    assert_eq!(
        classify_list(Some(403), "insufficientPermissions", false),
        FailureState::Terminal(TerminalReason::Forbidden)
    );
    assert_eq!(
        classify_list(Some(404), "notFound", false),
        FailureState::Terminal(TerminalReason::CalendarNotFound)
    );
    assert!(classify_list(Some(404), "notFound", false).purges_cache());
    assert!(!classify_list(Some(429), "", false).purges_cache());
}

#[test]
fn google_calendar_mutation_matrix_rejects_only_that_command() {
    assert_eq!(
        classify_mutation(Some(412), "conditionNotMet"),
        FailureState::MutationRejected(MutationReason::EtagConflict)
    );
    assert_eq!(
        classify_mutation(Some(403), "forbiddenForNonOrganizer"),
        FailureState::MutationRejected(MutationReason::NotOrganizer)
    );
    assert_eq!(
        classify_mutation(Some(410), "deleted"),
        FailureState::MutationRejected(MutationReason::EventMissingNeedsProbe),
        "404 and 410 are ambiguous until the calendar is probed"
    );
    let default = classify_mutation(Some(400), "badRequest");
    assert_eq!(
        default,
        FailureState::MutationRejected(MutationReason::Unclassified)
    );
    assert!(!default.purges_cache(), "no global state changes");
}

#[test]
fn google_calendar_reason_is_capped_before_classification() {
    let long_reason = "x".repeat(MAX_REASON_CHARS * 4);
    let state = classify_list(Some(403), &long_reason, false);
    assert_eq!(
        state,
        FailureState::Transient(TransientReason::Unclassified),
        "an over-long reason matches nothing and still fails closed"
    );
    assert_eq!(
        super::failure::normalize_reason(&long_reason)
            .chars()
            .count(),
        MAX_REASON_CHARS
    );
}

// ── The authorization request (T11 decisions 1 and 2) ─────────────────────

struct AcceptSignature;

impl IdTokenSignatureVerifier for AcceptSignature {
    fn verify(&self, _input: &str, _signature: &[u8], _key_id: Option<&str>) -> Result<(), String> {
        Ok(())
    }
}

struct RejectSignature;

impl IdTokenSignatureVerifier for RejectSignature {
    fn verify(&self, _input: &str, _signature: &[u8], _key_id: Option<&str>) -> Result<(), String> {
        Err("no key matched".to_string())
    }
}

fn id_token(claims: serde_json::Value) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    format!(
        "{}.{}.{}",
        URL_SAFE_NO_PAD.encode(json!({ "alg": "RS256", "kid": "k1" }).to_string()),
        URL_SAFE_NO_PAD.encode(claims.to_string()),
        URL_SAFE_NO_PAD.encode([1u8, 2, 3])
    )
}

#[test]
fn google_calendar_rejects_mismatched_state() {
    let request =
        AuthRequest::new(CLIENT_ID, "http://127.0.0.1:1234", true).expect("entropy is available");
    let query = format!("code=abc&state={}x", request.state.expose());
    assert_eq!(
        request.verify_callback(&query).unwrap_err(),
        CallbackError::StateMismatch
    );
    let good = format!("code=abc&state={}", request.state.expose());
    assert_eq!(
        request
            .verify_callback(&good)
            .expect("a matching state is accepted")
            .expose(),
        "abc"
    );
}

#[test]
fn google_calendar_requires_nonce_echo() {
    let request =
        AuthRequest::new(CLIENT_ID, "http://127.0.0.1:1234", true).expect("entropy is available");
    let expectations = IdTokenExpectations {
        client_id: CLIENT_ID.to_string(),
        nonce: Redacted::new(request.nonce.expose().clone()),
        now_ms: 1_000_000,
    };
    let without_nonce = id_token(json!({
        "iss": "https://accounts.google.com",
        "aud": CLIENT_ID,
        "sub": "google-sub-1",
        "exp": 2_000,
    }));
    assert_eq!(
        verify_id_token(&without_nonce, &AcceptSignature, &expectations),
        Err(IdTokenError::Nonce)
    );
    let wrong_nonce = id_token(json!({
        "iss": "https://accounts.google.com",
        "aud": CLIENT_ID,
        "sub": "google-sub-1",
        "exp": 2_000,
        "nonce": "not-the-one",
    }));
    assert_eq!(
        verify_id_token(&wrong_nonce, &AcceptSignature, &expectations),
        Err(IdTokenError::Nonce)
    );
    let good = id_token(json!({
        "iss": "https://accounts.google.com",
        "aud": CLIENT_ID,
        "sub": "google-sub-1",
        "exp": 2_000,
        "nonce": request.nonce.expose(),
        "email": "person@example.com",
    }));
    let claims = verify_id_token(&good, &AcceptSignature, &expectations)
        .expect("an echoed nonce is accepted");
    assert_eq!(claims.sub, "google-sub-1");
}

#[test]
fn google_calendar_id_token_needs_a_verified_signature_and_matching_claims() {
    let nonce = "nonce-value";
    let expectations = IdTokenExpectations {
        client_id: CLIENT_ID.to_string(),
        nonce: Redacted::new(nonce.to_string()),
        now_ms: 1_000_000,
    };
    let base = |overrides: serde_json::Value| {
        let mut claims = json!({
            "iss": "https://accounts.google.com",
            "aud": CLIENT_ID,
            "sub": "google-sub-1",
            "exp": 2_000,
            "nonce": nonce,
        });
        for (key, value) in overrides.as_object().expect("an object of overrides") {
            claims[key] = value.clone();
        }
        id_token(claims)
    };
    assert!(matches!(
        verify_id_token(&base(json!({})), &RejectSignature, &expectations),
        Err(IdTokenError::Signature(_))
    ));
    assert_eq!(
        verify_id_token(
            &base(json!({ "iss": "https://evil.example" })),
            &AcceptSignature,
            &expectations
        ),
        Err(IdTokenError::Issuer)
    );
    assert_eq!(
        verify_id_token(
            &base(json!({ "aud": "another-client" })),
            &AcceptSignature,
            &expectations
        ),
        Err(IdTokenError::Audience)
    );
    assert_eq!(
        verify_id_token(
            &base(json!({ "exp": 500 })),
            &AcceptSignature,
            &expectations
        ),
        Err(IdTokenError::Expired)
    );
    assert_eq!(
        verify_id_token(&base(json!({ "sub": "" })), &AcceptSignature, &expectations),
        Err(IdTokenError::Subject)
    );
    assert_eq!(
        verify_id_token("not.a.token.at.all", &AcceptSignature, &expectations),
        Err(IdTokenError::Malformed("not three segments"))
    );
}

#[test]
fn google_calendar_callback_bounds_query_size_and_count() {
    let expected = "state-value";
    let oversized = format!("state={expected}&code={}", "a".repeat(9000));
    assert!(matches!(
        verify_callback_query(&oversized, expected),
        Err(CallbackError::OverBound(_))
    ));
    let many: Vec<String> = (0..40).map(|index| format!("p{index}=1")).collect();
    assert!(matches!(
        verify_callback_query(&many.join("&"), expected),
        Err(CallbackError::OverBound(_))
    ));
    let long_value = format!("state={expected}&code={}", "a".repeat(2100));
    assert!(matches!(
        verify_callback_query(&long_value, expected),
        Err(CallbackError::OverBound(_))
    ));
}

#[test]
fn google_calendar_callback_surfaces_a_reported_error_and_missing_fields() {
    assert!(matches!(
        verify_callback_query("error=access_denied&state=s", "s"),
        Err(CallbackError::Reported(_))
    ));
    assert_eq!(
        verify_callback_query("code=abc", "s").unwrap_err(),
        CallbackError::Missing("state")
    );
    assert_eq!(
        verify_callback_query("state=s", "s").unwrap_err(),
        CallbackError::Missing("code")
    );
}

#[test]
fn google_calendar_authorization_url_carries_pkce_and_the_named_scopes() {
    let request =
        AuthRequest::new(CLIENT_ID, "http://127.0.0.1:9999", true).expect("entropy is available");
    let url = request.authorization_url();
    assert!(url.contains("code_challenge_method=S256"));
    assert!(url.contains("access_type=offline"));
    assert!(url.contains("prompt=consent"));
    assert!(
        url.contains(
            &percent_encoding::utf8_percent_encode(
                &request.pkce.challenge,
                percent_encoding::NON_ALPHANUMERIC
            )
            .to_string()
        ),
        "the challenge travels in the URL, percent-encoded"
    );
    assert!(
        !url.contains(request.pkce.verifier.expose()),
        "the verifier never leaves this process before the exchange"
    );
    for scope in SCOPES {
        let encoded =
            percent_encoding::utf8_percent_encode(scope, percent_encoding::NON_ALPHANUMERIC)
                .to_string();
        assert!(url.contains(&encoded), "the URL requests {scope}");
    }
    assert!(
        !url.contains("calendar.events.readonly") && !url.contains("calendarList"),
        "no scope beyond the three T11 decision 1 names"
    );
}

#[test]
fn google_calendar_exchange_without_a_refresh_token_writes_no_binding() {
    let granted: Vec<String> = SCOPES.iter().map(|scope| (*scope).to_string()).collect();
    assert_eq!(
        check_exchange(&granted, false, true),
        Err(ExchangeError::NoRefreshToken)
    );
    assert_eq!(
        check_exchange(&granted, true, false),
        Err(ExchangeError::NoIdToken)
    );
    let short: Vec<String> = granted.iter().take(1).cloned().collect();
    assert!(matches!(
        check_exchange(&short, true, true),
        Err(ExchangeError::ScopeMissing(_))
    ));
    assert_eq!(check_exchange(&granted, true, true), Ok(()));
}

#[test]
fn google_calendar_credentials_never_render() {
    let secret = Redacted::new(SENTINEL.to_string());
    assert!(!format!("{secret:?}").contains(SENTINEL));
    assert!(!format!("{secret}").contains(SENTINEL));

    let pkce = PkcePair::generate().expect("entropy is available");
    assert!(!format!("{pkce:?}").contains(pkce.verifier.expose()));

    let request =
        AuthRequest::new(CLIENT_ID, "http://127.0.0.1:1", false).expect("entropy is available");
    let rendered = format!("{request:?}");
    assert!(!rendered.contains(request.state.expose()));
    assert!(!rendered.contains(request.nonce.expose()));

    let envelope = CalendarEnvelope {
        active_binding: Some(binding(1)),
        pending: Default::default(),
    };
    assert!(
        !format!("{envelope:?}").contains(SENTINEL),
        "no token reaches a Debug dump"
    );
}

// ── The loopback listener (T11 decision 1) ────────────────────────────────

fn send_raw(port: u16, request: &str) {
    use std::io::Write as _;
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))
        .expect("the listener accepts a loopback connection");
    stream
        .write_all(request.as_bytes())
        .expect("the request writes");
    stream.flush().expect("the request flushes");
}

#[test]
fn google_calendar_loopback_returns_the_callback_query() {
    let listener = CallbackListener::bind().expect("a loopback port binds");
    let port = listener.port();
    assert!(listener.redirect_uri().starts_with("http://127.0.0.1:"));
    let sender = std::thread::spawn(move || {
        send_raw(
            port,
            "GET /?code=abc&state=xyz HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        );
    });
    let query = listener
        .wait_for_callback(&ListenerLimits::default())
        .expect("the callback arrives");
    sender.join().expect("the sender finishes");
    assert_eq!(query, "code=abc&state=xyz");
}

#[test]
fn google_calendar_loopback_refuses_an_oversized_request() {
    let listener = CallbackListener::bind().expect("a loopback port binds");
    let port = listener.port();
    let limits = ListenerLimits {
        max_request_bytes: 64,
        max_connections: 1,
        read_timeout_ms: 2_000,
        wait_timeout_ms: 10_000,
    };
    let sender = std::thread::spawn(move || {
        send_raw(
            port,
            &format!(
                "GET /?code={}&state=xyz HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
                "a".repeat(4096)
            ),
        );
    });
    let error = listener
        .wait_for_callback(&limits)
        .expect_err("an oversized request is refused");
    let _ = sender.join();
    assert_eq!(error, ListenerError::TooManyConnections(1));
}

#[test]
fn google_calendar_loopback_stops_after_the_connection_cap() {
    let listener = CallbackListener::bind().expect("a loopback port binds");
    let port = listener.port();
    let limits = ListenerLimits {
        max_request_bytes: 8192,
        max_connections: 2,
        read_timeout_ms: 1_000,
        wait_timeout_ms: 10_000,
    };
    let sender = std::thread::spawn(move || {
        for _ in 0..4 {
            send_raw(port, "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        }
    });
    let error = listener
        .wait_for_callback(&limits)
        .expect_err("the cap ends the wait");
    let _ = sender.join();
    assert_eq!(error, ListenerError::TooManyConnections(2));
}

#[test]
fn google_calendar_loopback_times_out_with_no_connection_at_all() {
    let listener = CallbackListener::bind().expect("a loopback port binds");
    let limits = ListenerLimits {
        max_request_bytes: 8192,
        max_connections: 8,
        read_timeout_ms: 5_000,
        wait_timeout_ms: 120,
    };
    let started = std::time::Instant::now();
    let error = listener
        .wait_for_callback(&limits)
        .expect_err("the wait ends on its own deadline");
    assert_eq!(error, ListenerError::TimedOut);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "the wait must end on `wait_timeout_ms`, not on the read timeout"
    );
}

#[test]
fn google_calendar_loopback_deadline_bounds_a_stalled_connection() {
    // The failure this binds: the per-read timeout is the only bound inside
    // `read_callback`, and it resets on every byte. A local process that sends
    // one byte and then goes quiet — never sending CRLFCRLF — holds the whole
    // flow for the full read timeout, and one that sends a byte just inside
    // each timeout holds it for `max_request_bytes` times that. Carrying the
    // wait's deadline into the read loop is what stops both.
    let listener = CallbackListener::bind().expect("a loopback port binds");
    let port = listener.port();
    let limits = ListenerLimits {
        max_request_bytes: 8192,
        max_connections: 8,
        // Two orders of magnitude past the whole wait: only the wait's own
        // deadline can end this connection.
        read_timeout_ms: 30_000,
        wait_timeout_ms: 250,
    };
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sender_stop = std::sync::Arc::clone(&stop);
    let sender = std::thread::spawn(move || {
        use std::io::Write as _;
        let Ok(mut stream) = std::net::TcpStream::connect(("127.0.0.1", port)) else {
            return;
        };
        // One byte of a request head that is never terminated, then silence
        // while the connection stays open.
        if stream.write_all(b"G").is_err() || stream.flush().is_err() {
            return;
        }
        while !sender_stop.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    });

    let started = std::time::Instant::now();
    let error = listener
        .wait_for_callback(&limits)
        .expect_err("a connection that never finishes its request cannot hold the wait open");
    let elapsed = started.elapsed();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = sender.join();

    assert_eq!(error, ListenerError::TimedOut);
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "the wait ran for {elapsed:?} against a 250 ms budget; the deadline must bound the \
         connection, not each read"
    );
}

// ── The remaining field caps (T12a decision 1) ────────────────────────────

#[test]
fn google_calendar_caps_the_id_etag_zone_and_page_token() {
    // Every one of these leaves the DTO on a wire: the id addresses an event,
    // the etag becomes an `If-Match` header, the zone is rendered, and the page
    // token becomes the next request's `pageToken` query value. Each is bounded
    // in the way its use allows — an id that would be cut is refused or
    // dropped, because a cut id names a different event.
    let page_token = "t".repeat(MAX_PAGE_TOKEN_CHARS + 500);
    let body = json!({
        "accessRole": "owner",
        "timeZone": "Z".repeat(MAX_TIME_ZONE_CHARS + 40),
        "nextPageToken": page_token,
        "items": [{
            "id": "i".repeat(MAX_ID_CHARS),
            "etag": "e".repeat(MAX_ETAG_CHARS + 300),
            "recurringEventId": "r".repeat(MAX_ID_CHARS + 700),
            "status": "confirmed",
            "summary": "capped",
            "start": {
                "dateTime": "2026-01-01T10:00:00Z",
                "timeZone": "z".repeat(MAX_TIME_ZONE_CHARS + 9),
            },
            "end": { "dateTime": "2026-01-01T11:00:00Z" },
        }],
    })
    .to_string();

    let page = parse_events_page(body.as_bytes(), body.len()).expect("the page parses");
    assert_eq!(
        page.default_time_zone.as_deref().map(str::len),
        Some(MAX_TIME_ZONE_CHARS),
        "the calendar zone is capped"
    );
    assert_eq!(
        page.next_page_token.as_deref().map(str::len),
        Some(MAX_PAGE_TOKEN_CHARS),
        "the token that becomes the next outbound query value is capped"
    );
    let event = page.events.first().expect("the item parses");
    assert_eq!(event.id.len(), MAX_ID_CHARS);
    assert_eq!(
        event.etag.as_deref().map(str::len),
        Some(MAX_ETAG_CHARS),
        "a cut etag still fences: it can only fail the mutation, never clobber"
    );
    assert_eq!(
        event.recurring_event_id, None,
        "an id that would have to be cut is dropped, never truncated into another id"
    );
    match &event.start {
        EventTime::Timed { time_zone, .. } => assert_eq!(
            time_zone.as_deref().map(str::len),
            Some(MAX_TIME_ZONE_CHARS)
        ),
        other => panic!("expected a timed start, got {other:?}"),
    }
}

#[test]
fn google_calendar_event_id_over_the_cap_is_refused_not_truncated() {
    let body = json!({
        "accessRole": "owner",
        "items": [{
            "id": "i".repeat(MAX_ID_CHARS + 1),
            "status": "confirmed",
            "start": { "dateTime": "2026-01-01T10:00:00Z" },
            "end": { "dateTime": "2026-01-01T11:00:00Z" },
        }],
    })
    .to_string();
    let error = parse_events_page(body.as_bytes(), body.len())
        .expect_err("an id over the cap is not an event this build can address");
    assert_eq!(
        error,
        DtoError::InvalidField {
            index: 0,
            field: "id"
        }
    );
}

#[test]
fn google_calendar_id_token_over_the_byte_cap_is_refused_before_it_is_split() {
    let oversized = format!(
        "{}.{}.{}",
        "a".repeat(MAX_ID_TOKEN_BYTES),
        "b".repeat(16),
        "c".repeat(16)
    );
    assert!(oversized.len() > MAX_ID_TOKEN_BYTES);
    let expectations = IdTokenExpectations {
        client_id: CLIENT_ID.to_string(),
        nonce: Redacted::new("nonce".to_string()),
        now_ms: 0,
    };
    assert_eq!(
        verify_id_token(&oversized, &AcceptSignature, &expectations),
        Err(IdTokenError::Malformed("over the byte cap")),
        "the byte cap runs before the token is split, so nothing downstream sees it"
    );
}
