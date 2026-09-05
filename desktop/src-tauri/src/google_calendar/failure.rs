//! The three failure matrices of T11 decision 8.
//!
//! Four states, keyed by HTTP status plus Google's error `reason`, over three
//! matrices — token exchange and refresh, `events.list`, and mutations — each
//! with an explicit default branch:
//!
//! * [`FailureState::Terminal`] purges the cache per T11 decision 6 and offers
//!   Reconnect.
//! * [`FailureState::Transient`] backs off behind a stale view; past
//!   `stale_after` the caller shows `unreachable`, drops events and offers
//!   Retry.
//! * [`FailureState::AppError`] (`invalid_client`) offers neither affordance.
//! * [`FailureState::MutationRejected`] rejects that one command, changing no
//!   global state.
//!
//! The refresh and list defaults fail closed — a transient state bounded by
//! `stale_after` — rather than being read as success.

use serde::{Deserialize, Serialize};

/// Largest error reason accepted from Google before classification.
///
/// The reason is relay- and API-sourced text that reaches a state name and a
/// log line, so it is capped here, at the boundary, not where it is rendered.
pub const MAX_REASON_CHARS: usize = 128;

/// Why the binding cannot be used again without a reconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalReason {
    /// The refresh token was revoked or expired.
    InvalidGrant,
    /// A scope the integration needs is no longer granted.
    ScopeWithdrawn,
    /// A 401 that survived a forced refresh.
    Unauthorized,
    /// The mapped calendar is gone.
    CalendarNotFound,
    /// Read or write access to the calendar was withdrawn.
    Forbidden,
}

/// Why the request may succeed later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransientReason {
    /// No response: DNS, TLS, connection or read failure, or offline.
    Network,
    /// A 5xx from Google.
    ServerError,
    /// A 429, or a 403 in the rate-limit family.
    RateLimited,
    /// A 401 that has not yet been retried behind a forced refresh.
    NeedsRefresh,
    /// The matrix default: unrecognized, so it fails closed at `stale_after`.
    Unclassified,
}

/// Why one create, edit or delete was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationReason {
    /// `If-Match` failed: the event changed under the form (T12a decision 11).
    EtagConflict,
    /// Google refused the edit because this account is not the organizer.
    NotOrganizer,
    /// A 404 or 410 on the event. Ambiguous until the mapped calendar is
    /// probed: Google reuses it for "event gone" and "calendar gone".
    EventMissingNeedsProbe,
    /// The mutation matrix default: this command alone fails.
    Unclassified,
}

/// The classified outcome of one failed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "reason")]
pub enum FailureState {
    /// Purge and offer Reconnect.
    Terminal(TerminalReason),
    /// Back off behind the stale view, bounded by `stale_after`.
    Transient(TransientReason),
    /// A misconfigured OAuth client: neither Reconnect nor Retry helps.
    AppError,
    /// Reject this command only.
    MutationRejected(MutationReason),
}

impl FailureState {
    /// Whether the cache must be purged per T11 decision 6.
    pub fn purges_cache(&self) -> bool {
        matches!(self, FailureState::Terminal(_))
    }
}

/// Lowercase and cap an API-sourced reason before it is matched or stored.
pub fn normalize_reason(reason: &str) -> String {
    reason
        .trim()
        .chars()
        .take(MAX_REASON_CHARS)
        .flat_map(char::to_lowercase)
        .collect()
}

/// Reasons in the 403 rate-limit family.
fn is_rate_limit_reason(reason: &str) -> bool {
    matches!(
        reason,
        "ratelimitexceeded" | "userratelimitexceeded" | "quotaexceeded"
    )
}

/// Reasons in the 403 access-withdrawn family.
fn is_access_withdrawn_reason(reason: &str) -> bool {
    matches!(reason, "forbidden" | "insufficientpermissions")
}

/// Classify a token exchange or refresh failure.
///
/// `status` is `None` when no response arrived at all.
pub fn classify_token(status: Option<u16>, reason: &str) -> FailureState {
    let reason = normalize_reason(reason);
    let Some(status) = status else {
        return FailureState::Transient(TransientReason::Network);
    };
    match (status, reason.as_str()) {
        (_, "invalid_client") => FailureState::AppError,
        (_, "invalid_grant") => FailureState::Terminal(TerminalReason::InvalidGrant),
        (_, "invalid_scope") => FailureState::Terminal(TerminalReason::ScopeWithdrawn),
        (429, _) => FailureState::Transient(TransientReason::RateLimited),
        (500..=599, _) => FailureState::Transient(TransientReason::ServerError),
        // Default: fail closed behind the stale view rather than reading an
        // unrecognized exchange failure as a revocation.
        _ => FailureState::Transient(TransientReason::Unclassified),
    }
}

/// Classify an `events.list` failure.
///
/// `refresh_already_forced` says whether this request already ran behind a
/// forced token refresh; only then is a 401 terminal.
pub fn classify_list(
    status: Option<u16>,
    reason: &str,
    refresh_already_forced: bool,
) -> FailureState {
    let reason = normalize_reason(reason);
    let Some(status) = status else {
        return FailureState::Transient(TransientReason::Network);
    };
    match (status, reason.as_str()) {
        (401, _) if refresh_already_forced => FailureState::Terminal(TerminalReason::Unauthorized),
        (401, _) => FailureState::Transient(TransientReason::NeedsRefresh),
        (403, r) if is_rate_limit_reason(r) => {
            FailureState::Transient(TransientReason::RateLimited)
        }
        (403, r) if is_access_withdrawn_reason(r) => {
            FailureState::Terminal(TerminalReason::Forbidden)
        }
        (404, _) => FailureState::Terminal(TerminalReason::CalendarNotFound),
        (429, _) => FailureState::Transient(TransientReason::RateLimited),
        (500..=599, _) => FailureState::Transient(TransientReason::ServerError),
        // Default: fail closed at `stale_after`.
        _ => FailureState::Transient(TransientReason::Unclassified),
    }
}

/// Classify an `events.insert`, `events.patch` or `events.delete` failure.
pub fn classify_mutation(status: Option<u16>, reason: &str) -> FailureState {
    let reason = normalize_reason(reason);
    let Some(status) = status else {
        return FailureState::Transient(TransientReason::Network);
    };
    match (status, reason.as_str()) {
        (412, _) => FailureState::MutationRejected(MutationReason::EtagConflict),
        (403, "forbiddenfornonorganizer") => {
            FailureState::MutationRejected(MutationReason::NotOrganizer)
        }
        (403, r) if is_rate_limit_reason(r) => {
            FailureState::Transient(TransientReason::RateLimited)
        }
        (403, r) if is_access_withdrawn_reason(r) => {
            FailureState::Terminal(TerminalReason::Forbidden)
        }
        // T12a decision 11: Google reuses 404 and 410 for "event gone" and
        // "calendar gone", so the caller probes the mapped calendar before
        // deciding. Never a purge on its own.
        (404 | 410, _) => FailureState::MutationRejected(MutationReason::EventMissingNeedsProbe),
        (429, _) => FailureState::Transient(TransientReason::RateLimited),
        (500..=599, _) => FailureState::Transient(TransientReason::ServerError),
        // Default: this command alone fails, changing no global state.
        _ => FailureState::MutationRejected(MutationReason::Unclassified),
    }
}
