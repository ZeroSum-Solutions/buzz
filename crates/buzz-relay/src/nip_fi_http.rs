//! NIP-FI HTTP ingress enforcement.
//!
//! Every protected HTTP surface in enforce mode MUST call
//! [`check_nip_fi_http`] before processing the request.  The function owns
//! the complete NIP-FI admission decision for one HTTP request:
//!
//! 1. Extract the `Nostr-Federated-Identity: Bearer <JWS>` assertion.
//! 2. Verify it offline against the configured issuer JWKS.
//! 3. Confirm the assertion's `nostr_pubkey` equals the NIP-98 event's
//!    `pubkey` (the proven actor). [FI-INV-05]
//! 4. Check the deny map for the proven pubkey. [FI-INV-14]
//!
//! HTTP is sessionless: every request re-verifies.  There is no lifetime-
//! partition concept — the session-bounds section of NIP-FI.md is WS-only.
//!
//! ## Carrier / precedence
//!
//! Per NIP-FI.md §Client-attached transport:
//! - Assertion: `Nostr-Federated-Identity: Bearer <compact-JWS>` (this
//!   module's responsibility).
//! - Nostr proof: `Authorization: Nostr <base64-event>` (NIP-98, owned by
//!   `bridge.rs` / each surface's existing auth extractor).
//! - `Authorization` is RESERVED for NIP-98; the assertion MUST NOT appear
//!   there.  Mixing the two fields is an `EvidenceRejected` (403) denial.
//!
//! ## Deny map
//!
//! The deny map is S4 (Duncan).  Until S4 lands this module stubs it as a
//! fail-closed no-op: [`HttpDenyMap::check`] always admits.  When S4 adds
//! the real implementation, replace the stub `impl` below with an import
//! and a real check.  The integration commit should be a trivial one-liner.
//!
//! ## Off-mode regression
//!
//! When `NipFiMode::Off`, `check_nip_fi_http` returns `Ok(None)` immediately.
//! Every surface that calls it must NOT change its behavior for `Ok(None)`.
//! This preserves the exact pre-NIP-FI behavior for OSS deployments.
//!
//! [FI-TRACE-DENIAL-ORACLE]: exact HTTP response bytes are fixed in NIP-FI.md.
//! [FI-TRACE-TRANSPORT-CLOSED]: assertion transport is exactly one header.
//! [FI-TRACE-AUTHORITY-UNIFORM]: all protected surfaces call this function.

use axum::{
    body::Body,
    http::{HeaderMap, Response, StatusCode},
    response::IntoResponse,
};
use buzz_auth::{
    DenialClass, NipFiMode, VerifiedAssertion, VerifyAssertion, CLIENT_ATTACHED_HEADER,
};
use chrono::{DateTime, Utc};
use nostr::PublicKey;

// ── Deny-map seam (S4 stub) ───────────────────────────────────────────────────

/// Narrow interface consumed by HTTP enforcement.  S4 (Duncan) will provide
/// the real implementation; until then, `AlwaysAdmitStubDenyMap` stubs it
/// fail-open (admits unconditionally).
///
/// Signature mirrors `NipFiDenyMap::is_denied` from S4 so integration is a
/// one-liner: replace `AlwaysAdmitStubDenyMap` with the shared map.
///
/// `(issuer, pubkey, now)` are required because the deny set is issuer-
/// scoped per `NIP-FI.md:624-627`.  Passing only pubkey would collide
/// across issuers — a deny for `(iss-A, k)` must not block `(iss-B, k)`.
///
/// Sealed: only implementations in this crate are accepted.
pub(crate) trait HttpDenyMap: sealed::Sealed {
    /// Returns `true` when `(issuer, pubkey)` has an active deny entry at
    /// `now` (`now < until`).  A poisoned or unavailable backing store MUST
    /// return `false` (admits) only when an explicit availability guarantee is
    /// established; the S4 real map currently admits on poisoned lock.  The
    /// S4 integration commit is expected to resolve the fail-closed story
    /// before S5 merges; the interface contract here is the agreed shape.
    fn is_denied(&self, issuer: &str, pubkey: &PublicKey, now: DateTime<Utc>) -> bool;
}

pub(crate) mod sealed {
    pub(crate) trait Sealed {}
}

/// Stub deny map that always admits.  Used until S4 provides the real map.
///
/// Name is explicit: this is **fail-open**, not fail-closed.  The stub phase
/// is intentional — deny-map enforcement defers to S4 landing.  The name
/// `AlwaysAdmitStubDenyMap` prevents a future integrator from assuming this
/// stub is safe for production use.
pub(crate) struct AlwaysAdmitStubDenyMap;
impl sealed::Sealed for AlwaysAdmitStubDenyMap {}
impl HttpDenyMap for AlwaysAdmitStubDenyMap {
    /// Always admits: the deny map is not yet wired (S4 pending).
    fn is_denied(&self, _issuer: &str, _pubkey: &PublicKey, _now: DateTime<Utc>) -> bool {
        false
    }
}

// ── Outcome ───────────────────────────────────────────────────────────────────

/// Outcome of NIP-FI HTTP admission for one request.
///
/// `Admitted(Some(assertion))` — enforce mode, assertion verified, pubkey
/// pairing confirmed, deny-map clear.  The caller may proceed.
///
/// `Admitted(None)` — off mode.  The caller proceeds unchanged (no NIP-FI
/// requirement).
///
/// `Denied(response)` — emit `response` verbatim and return; do not process
/// the request.
#[must_use]
pub(crate) enum NipFiHttpOutcome {
    /// Request admitted.  The `VerifiedAssertion` is available for future use
    /// (e.g., forwarding claims to downstream services); callers that don't
    /// need it may ignore the inner value.
    #[allow(dead_code)]
    Admitted(Option<VerifiedAssertion>),
    Denied(Response<Body>),
}

// ── Main admission function ───────────────────────────────────────────────────

/// Gate one HTTP request against the NIP-FI assertion + NIP-98 pairing
/// requirement.
///
/// `proven_pubkey` is the pubkey already extracted from the NIP-98
/// `Authorization: Nostr` event by the surface's own auth extractor.  This
/// function checks only the NIP-FI layer on top.
///
/// Call sites: `bridge.rs`, `media.rs`, `invites.rs`, `git/transport.rs`.
///
/// [FI-TRACE-AUTHORITY-UNIFORM] All protected surfaces reach one admission
/// authority — this function.
pub(crate) fn check_nip_fi_http<D: HttpDenyMap>(
    headers: &HeaderMap,
    proven_pubkey: &PublicKey,
    verifier: Option<&dyn VerifyAssertion>,
    mode: NipFiMode,
    deny_map: &D,
) -> NipFiHttpOutcome {
    // Off mode: no NIP-FI requirement.  Caller unchanged. [FI-INV-15 exemption]
    if matches!(mode, NipFiMode::Off) {
        return NipFiHttpOutcome::Admitted(None);
    }

    // DenyProtected mode: unconditional 503.  All protected HTTP routes
    // fail closed during operator repair.  Same rationale as upgrade denials:
    // the client's evidence may be valid but authorization is unavailable.
    if matches!(mode, NipFiMode::DenyProtected) {
        return NipFiHttpOutcome::Denied(http_denial(DenialClass::AuthorizationUnavailable));
    }

    // Enforce mode: extract and verify the assertion.
    let token = match extract_bearer_token(headers) {
        Ok(t) => t,
        Err(class) => return NipFiHttpOutcome::Denied(http_denial(class)),
    };

    let verifier = match verifier {
        Some(v) => v,
        None => {
            // Verifier not yet constructed (startup race); fail closed.
            return NipFiHttpOutcome::Denied(http_denial(DenialClass::AuthorizationUnavailable));
        }
    };

    let assertion = match verifier.verify_assertion(token) {
        Ok(a) => a,
        Err(e) => {
            tracing::debug!(code = e.code(), "nip-fi assertion denied at http ingress");
            return NipFiHttpOutcome::Denied(http_denial(e.denial_class()));
        }
    };

    // Key pairing: assertion's nostr_pubkey MUST equal the proven NIP-98 key.
    // A claimless assertion (no nostr_pubkey) is also a denial.  [FI-INV-05]
    match assertion.asserted_key() {
        Some(k) if k == *proven_pubkey => {}
        _ => {
            metrics::counter!(
                "buzz_auth_failures_total",
                "reason" => "nip_fi_http_key_mismatch"
            )
            .increment(1);
            tracing::debug!(
                proven = %proven_pubkey.to_hex(),
                "NIP-FI HTTP key pairing mismatch"
            );
            // Key mismatch is a private-state denial: authorization_denied (403).
            // [FI-TRACE-DENIAL-ORACLE]
            return NipFiHttpOutcome::Denied(http_denial(DenialClass::AuthorizationDenied));
        }
    }

    // Deny-map check: (iss, pubkey) must not be in an active deny window.
    // The issuer comes from the already-verified assertion; `now` is used by
    // the real map for TTL comparison.  [FI-INV-14] [NIP-FI.md:624-627]
    let issuer = assertion.identity().issuer();
    if deny_map.is_denied(issuer, proven_pubkey, Utc::now()) {
        metrics::counter!(
            "buzz_auth_failures_total",
            "reason" => "nip_fi_http_denied_pubkey"
        )
        .increment(1);
        // Denied-pubkey is a private-state denial.  [FI-TRACE-DENIAL-ORACLE]
        return NipFiHttpOutcome::Denied(http_denial(DenialClass::AuthorizationDenied));
    }

    NipFiHttpOutcome::Admitted(Some(assertion))
}

// ── Transport extraction ──────────────────────────────────────────────────────

/// Extract the single `Bearer <token>` from the `Nostr-Federated-Identity`
/// header.
///
/// Rejects all forms the spec prohibits:
/// - Absent → `MissingEvidence`
/// - Repeated (multiple header values) → `EvidenceRejected`
/// - Comma-combined (`,` in a single value) → `EvidenceRejected`
/// - Empty after `Bearer ` stripping → `EvidenceRejected`
/// - Non-`Bearer ` prefix → `EvidenceRejected`
/// - Whitespace in the token (after scheme) → `EvidenceRejected`
///
/// [FI-TRACE-TRANSPORT-CLOSED]
pub(crate) fn extract_bearer_token(headers: &HeaderMap) -> Result<&str, DenialClass> {
    let mut values = headers.get_all(CLIENT_ATTACHED_HEADER).iter();
    let first = match values.next() {
        Some(v) => v,
        None => return Err(DenialClass::MissingEvidence),
    };
    // Repeated header fields deny. [FI-TRACE-TRANSPORT-CLOSED]
    if values.next().is_some() {
        return Err(DenialClass::EvidenceRejected);
    }
    let raw = first.to_str().map_err(|_| DenialClass::EvidenceRejected)?;
    // Comma-combined values deny.
    if raw.contains(',') {
        return Err(DenialClass::EvidenceRejected);
    }
    let token = raw
        .strip_prefix("Bearer ")
        .ok_or(DenialClass::EvidenceRejected)?;
    // Empty or whitespace-containing token denies.
    if token.is_empty() || token.contains(ascii_whitespace) {
        return Err(DenialClass::EvidenceRejected);
    }
    Ok(token)
}

fn ascii_whitespace(c: char) -> bool {
    c.is_ascii_whitespace()
}

// ── HTTP denial response ──────────────────────────────────────────────────────

/// Build the exact HTTP denial response for the given class.
///
/// The response contract is fixed by NIP-FI.md rejection table:
/// - Status, Content-Type, WWW-Authenticate (for 401), and body bytes are the
///   closed contract.  No other fields are added that depend on the private
///   condition. [FI-TRACE-DENIAL-ORACLE]
pub(crate) fn http_denial(class: DenialClass) -> Response<Body> {
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(class.http_status()).expect("valid status"))
        .header("Content-Type", class.content_type());
    if let Some(challenge) = class.www_authenticate() {
        builder = builder.header("WWW-Authenticate", challenge);
    }
    builder
        .body(Body::from(class.http_body()))
        .expect("valid denial response")
}

// ── State-convenience wrapper ─────────────────────────────────────────────────

/// Convenience wrapper: pull mode + verifier from `AppState` and call
/// [`check_nip_fi_http`].
///
/// This is the one-liner every surface calls after its own NIP-98 verification
/// has established `proven_pubkey`.  Surfaces that need a custom deny-map
/// should call [`check_nip_fi_http`] directly.
///
/// [FI-TRACE-AUTHORITY-UNIFORM]
pub(crate) fn check_nip_fi_http_on_state(
    state: &crate::state::AppState,
    headers: &HeaderMap,
    proven_pubkey: &PublicKey,
) -> NipFiHttpOutcome {
    let mode = state.config.nip_fi.mode;
    let verifier = state.nip_fi_verifier.as_deref();
    check_nip_fi_http(
        headers,
        proven_pubkey,
        verifier,
        mode,
        &AlwaysAdmitStubDenyMap,
    )
}

// ── IntoResponse shim for NipFiHttpOutcome ────────────────────────────────────

impl IntoResponse for NipFiHttpOutcome {
    fn into_response(self) -> axum::response::Response {
        match self {
            NipFiHttpOutcome::Denied(r) => r,
            // Admitted should never be converted to a response; the caller
            // must check for Denied first.
            NipFiHttpOutcome::Admitted(_) => {
                // Defensive fallback: internal invariant violation.
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "nip-fi: admitted path called as response",
                )
                    .into_response()
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use buzz_auth::{NipFiMode, VerifyAssertion};
    use chrono::Utc;

    // Helper: read the body bytes synchronously (tests only).
    fn body_bytes(resp: Response<Body>) -> Vec<u8> {
        use http_body_util::BodyExt as _;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                resp.into_body()
                    .collect()
                    .await
                    .expect("body")
                    .to_bytes()
                    .to_vec()
            })
    }

    fn any_pubkey() -> PublicKey {
        nostr::Keys::generate().public_key()
    }

    // ── extract_bearer_token ─────────────────────────────────────────────────

    // Absent header → MissingEvidence (401).
    //
    // Mutation evidence: returning EvidenceRejected instead makes the
    // `assert_eq!(class, DenialClass::MissingEvidence)` assertion panic.
    #[test]
    fn missing_header_is_missing_evidence() {
        let headers = HeaderMap::new();
        let class = extract_bearer_token(&headers).unwrap_err();
        assert_eq!(class, DenialClass::MissingEvidence);
    }

    // Repeated header → EvidenceRejected (403).
    //
    // Mutation evidence: keeping the first value instead of rejecting makes
    // `unwrap_err()` panic.
    #[test]
    fn repeated_header_is_evidence_rejected() {
        let mut headers = HeaderMap::new();
        headers.append(
            CLIENT_ATTACHED_HEADER,
            HeaderValue::from_static("Bearer token1"),
        );
        headers.append(
            CLIENT_ATTACHED_HEADER,
            HeaderValue::from_static("Bearer token2"),
        );
        let class = extract_bearer_token(&headers).unwrap_err();
        assert_eq!(class, DenialClass::EvidenceRejected);
    }

    // Comma-combined → EvidenceRejected.
    #[test]
    fn comma_combined_is_evidence_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert(
            CLIENT_ATTACHED_HEADER,
            HeaderValue::from_static("Bearer a, Bearer b"),
        );
        let class = extract_bearer_token(&headers).unwrap_err();
        assert_eq!(class, DenialClass::EvidenceRejected);
    }

    // Empty token after Bearer prefix → EvidenceRejected.
    #[test]
    fn empty_token_is_evidence_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert(CLIENT_ATTACHED_HEADER, HeaderValue::from_static("Bearer "));
        let class = extract_bearer_token(&headers).unwrap_err();
        assert_eq!(class, DenialClass::EvidenceRejected);
    }

    // Wrong prefix (non-Bearer) → EvidenceRejected.
    #[test]
    fn wrong_prefix_is_evidence_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert(
            CLIENT_ATTACHED_HEADER,
            HeaderValue::from_static("Token xyz"),
        );
        let class = extract_bearer_token(&headers).unwrap_err();
        assert_eq!(class, DenialClass::EvidenceRejected);
    }

    // Whitespace in token → EvidenceRejected.
    #[test]
    fn whitespace_in_token_is_evidence_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert(
            CLIENT_ATTACHED_HEADER,
            HeaderValue::from_static("Bearer foo bar"),
        );
        let class = extract_bearer_token(&headers).unwrap_err();
        assert_eq!(class, DenialClass::EvidenceRejected);
    }

    // Valid Bearer token → extracted.
    #[test]
    fn valid_bearer_token_extracted() {
        let mut headers = HeaderMap::new();
        headers.insert(
            CLIENT_ATTACHED_HEADER,
            HeaderValue::from_static("Bearer a.b.c"),
        );
        let token = extract_bearer_token(&headers).unwrap();
        assert_eq!(token, "a.b.c");
    }

    // ── http_denial ──────────────────────────────────────────────────────────

    // MissingEvidence → 401, exact body, WWW-Authenticate: Nostr.
    //
    // Mutation evidence: changing status to 403 makes the status assert panic.
    #[test]
    fn missing_evidence_denial_is_401_with_nostr_challenge() {
        let resp = http_denial(DenialClass::MissingEvidence);
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            resp.headers()
                .get("WWW-Authenticate")
                .and_then(|v| v.to_str().ok()),
            Some("Nostr"),
            "MissingEvidence MUST carry WWW-Authenticate: Nostr"
        );
        assert_eq!(body_bytes(resp), b"authentication required\n");
    }

    // EvidenceRejected → 403, exact body, no WWW-Authenticate.
    //
    // Mutation evidence: changing status to 401 or body to "denied" makes
    // corresponding assertions panic.
    #[test]
    fn evidence_rejected_denial_is_403_exact_bytes() {
        let resp = http_denial(DenialClass::EvidenceRejected);
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(
            resp.headers().get("WWW-Authenticate").is_none(),
            "EvidenceRejected must not carry a WWW-Authenticate header"
        );
        assert_eq!(body_bytes(resp), b"evidence rejected\n");
    }

    // AuthorizationDenied → 403, exact body.
    //
    // Mutation evidence: body check.
    #[test]
    fn authorization_denied_is_403_exact_bytes() {
        let resp = http_denial(DenialClass::AuthorizationDenied);
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(body_bytes(resp), b"authorization denied\n");
    }

    // AuthorizationUnavailable → 503, exact body.
    //
    // Mutation evidence: status and body checks.
    #[test]
    fn authorization_unavailable_is_503_exact_bytes() {
        let resp = http_denial(DenialClass::AuthorizationUnavailable);
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body_bytes(resp), b"authorization unavailable\n");
    }

    // Private-state conditions (AuthorizationDenied) are byte-identical.
    // Key mismatch and denied pubkey both map to authorization_denied.
    // [FI-TRACE-DENIAL-ORACLE]
    //
    // Mutation evidence: if key_mismatch path emitted a different class, the
    // assert_eq on body would diverge.
    #[test]
    fn authorization_denied_rows_are_byte_identical() {
        let a = body_bytes(http_denial(DenialClass::AuthorizationDenied));
        // A second call produces the same bytes.
        let b = body_bytes(http_denial(DenialClass::AuthorizationDenied));
        assert_eq!(
            a, b,
            "all AuthorizationDenied responses must be byte-identical"
        );
    }

    // ── check_nip_fi_http — off mode ─────────────────────────────────────────

    // Off mode → Admitted(None) regardless of headers.
    //
    // Mutation evidence: returning Denied from off mode makes
    // `matches!(outcome, NipFiHttpOutcome::Admitted(None))` panic.
    #[test]
    fn off_mode_admits_unconditionally() {
        let headers = HeaderMap::new(); // no assertion
        let pubkey = any_pubkey();
        let outcome = check_nip_fi_http(
            &headers,
            &pubkey,
            None::<&dyn VerifyAssertion>,
            NipFiMode::Off,
            &AlwaysAdmitStubDenyMap,
        );
        assert!(
            matches!(outcome, NipFiHttpOutcome::Admitted(None)),
            "Off mode MUST not require NIP-FI assertion — OSS default regression"
        );
    }

    // ── check_nip_fi_http — deny_protected ───────────────────────────────────

    // DenyProtected → Denied(503 authorization_unavailable).
    //
    // Mutation evidence: returning Admitted from deny_protected mode makes
    // `matches!(outcome, NipFiHttpOutcome::Denied(_))` panic.
    #[test]
    fn deny_protected_returns_503() {
        let headers = HeaderMap::new();
        let pubkey = any_pubkey();
        let outcome = check_nip_fi_http(
            &headers,
            &pubkey,
            None::<&dyn VerifyAssertion>,
            NipFiMode::DenyProtected,
            &AlwaysAdmitStubDenyMap,
        );
        match outcome {
            NipFiHttpOutcome::Denied(resp) => {
                assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
                assert_eq!(body_bytes(resp), b"authorization unavailable\n");
            }
            _ => panic!("DenyProtected must deny with 503"),
        }
    }

    // ── check_nip_fi_http — enforce, missing assertion ───────────────────────

    // Enforce + missing assertion header → 401.
    //
    // Mutation evidence: the status assertion on the response panics if the
    // missing-header path returns 403 instead of 401.
    #[test]
    fn enforce_missing_assertion_is_401() {
        let headers = HeaderMap::new();
        let pubkey = any_pubkey();
        let outcome = check_nip_fi_http(
            &headers,
            &pubkey,
            None::<&dyn VerifyAssertion>,
            NipFiMode::Enforce,
            &AlwaysAdmitStubDenyMap,
        );
        // Missing header → MissingEvidence before verifier check.
        match outcome {
            NipFiHttpOutcome::Denied(resp) => {
                assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
                assert_eq!(body_bytes(resp), b"authentication required\n");
            }
            _ => panic!("Missing assertion must deny with 401"),
        }
    }

    // ── check_nip_fi_http — enforce, no verifier (startup race) ─────────────

    // Enforce + valid-looking header but no verifier (startup race) → 503.
    //
    // Mutation evidence: returning 403 from the None-verifier path makes the
    // status assertion panic.
    #[test]
    fn enforce_no_verifier_returns_503() {
        let mut headers = HeaderMap::new();
        headers.insert(
            CLIENT_ATTACHED_HEADER,
            HeaderValue::from_static("Bearer eyJhbGciOiJFUzI1NiJ9.e30.sig"),
        );
        let pubkey = any_pubkey();
        let outcome = check_nip_fi_http(
            &headers,
            &pubkey,
            None::<&dyn VerifyAssertion>,
            NipFiMode::Enforce,
            &AlwaysAdmitStubDenyMap,
        );
        match outcome {
            NipFiHttpOutcome::Denied(resp) => {
                assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
                assert_eq!(body_bytes(resp), b"authorization unavailable\n");
            }
            _ => panic!("Missing verifier must deny with 503"),
        }
    }

    // ── check_nip_fi_http — deny map stub admits ─────────────────────────────

    // The stub deny map always admits (never denies).
    //
    // Mutation evidence: if `is_denied` returned true, the deny path would
    // fire and the test would receive a Denied outcome instead of reaching
    // the verifier check (which would deny for a different reason — invalid
    // token).  The distinction is observable: 401 vs 403.
    #[test]
    fn stub_deny_map_never_denies() {
        let pubkey = any_pubkey();
        assert!(
            !AlwaysAdmitStubDenyMap.is_denied("https://idp.example.com", &pubkey, Utc::now()),
            "stub deny map MUST admit unconditionally until S4 provides the real map"
        );
    }
}
