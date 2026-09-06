//! Provider error classification and reset-time parsing.
//!
//! `classify_at` is pure: it reads an [`AcpError`], never the clock, and takes
//! `now` as a parameter so a reset time parsed from
//! `resets 4:20am (America/Los_Angeles)` resolves to the same absolute instant
//! in a test as in production.
//!
//! Design: `docs/plans/2026-09-06-harness-reliability-design.md`, "Error classes".

use std::str::FromStr;

use chrono::{DateTime, Datelike, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;

use crate::acp::AcpError;

use super::{ErrorClass, MAX_RAW_ERROR_CHARS};

/// Longest IANA zone name accepted from a provider error string. The longest
/// real zone (`America/Argentina/ComodRivadavia`) is 32 characters; the cap is
/// generous but finite so a hostile error line cannot allocate.
const MAX_ZONE_CHARS: usize = 64;

/// Longest wall-clock token (`12:40am`) accepted after `resets`.
const MAX_TIME_TOKEN_CHARS: usize = 12;

/// How far past `now` a parsed reset time is still searched for. Three days
/// covers a wall time that does not exist today (a spring-forward gap) and one
/// that has already passed today.
const MAX_RESET_SEARCH_DAYS: u32 = 3;

/// Substrings that mean "the account is out of capacity for now".
///
/// Matched case-insensitively against the truncated error text.
const CAPACITY_MARKERS: &[&str] = &[
    "session limit",
    "rate limit",
    "rate_limit",
    "ratelimit",
    "overloaded",
    "quota",
    "too many requests",
    "usage limit",
];

/// Substrings that mean "the provider itself is broken right now".
const PROVIDER_INTERNAL_MARKERS: &[&str] = &[
    "internal error",
    "internal server error",
    "bad gateway",
    "service unavailable",
    "gateway timeout",
];

/// HTTP status codes that mean capacity exhaustion.
const CAPACITY_HTTP_CODES: &[&str] = &["429"];

/// HTTP status codes that mean the provider is broken (5xx).
const PROVIDER_INTERNAL_HTTP_CODES: &[&str] = &["500", "502", "503", "504", "529"];

/// Classify a provider error at a known instant.
///
/// The order is deliberate: an auth failure never retries, a capacity marker
/// wins over the `Internal error:` prefix the Claude session-limit line carries,
/// and only what is left falls through to `ProviderInternal` / `Unknown`.
pub fn classify_at(err: &AcpError, now: DateTime<Utc>) -> ErrorClass {
    let raw = truncate_chars(&err.to_string(), MAX_RAW_ERROR_CHARS);
    if is_auth_text(&raw) {
        return ErrorClass::Auth;
    }
    let lower = raw.to_lowercase();
    if has_marker(&lower, CAPACITY_MARKERS) || has_http_code(&lower, CAPACITY_HTTP_CODES) {
        return ErrorClass::CapacityExhausted {
            resets_at: parse_reset_at(&raw, now),
        };
    }
    if has_marker(&lower, PROVIDER_INTERNAL_MARKERS)
        || has_http_code(&lower, PROVIDER_INTERNAL_HTTP_CODES)
    {
        return ErrorClass::ProviderInternal;
    }
    ErrorClass::Unknown
}

/// The harness's pre-existing auth detection, unchanged (see
/// `is_auth_error` in `lib.rs`, which delegates here).
pub fn is_auth_text(raw: &str) -> bool {
    raw.contains("Re-authenticate") || raw.contains("API Error: 401")
}

/// Truncate to at most `max` characters on a char boundary.
///
/// Every provider string that reaches the ledger, the park file or a channel
/// notice passes through here first: the text is relay- and provider-sourced
/// and is never stored or forwarded at its original length.
pub fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect()
}

fn has_marker(lower: &str, markers: &[&str]) -> bool {
    markers.iter().any(|m| lower.contains(m))
}

/// Match a bare HTTP status code with a non-digit on both sides, so `429` in
/// `(429)` or `429 Too Many Requests` matches but `1429000` does not.
fn has_http_code(lower: &str, codes: &[&str]) -> bool {
    let bytes = lower.as_bytes();
    codes.iter().any(|code| {
        lower.match_indices(code).any(|(idx, _)| {
            let before_ok = idx == 0 || !bytes[idx - 1].is_ascii_digit();
            let after = idx + code.len();
            let after_ok = after >= bytes.len() || !bytes[after].is_ascii_digit();
            before_ok && after_ok
        })
    })
}

/// Parse `resets H:MM(am|pm) (IANA zone)` into the next occurrence of that wall
/// time in that zone strictly after `now`.
///
/// Returns `None` when the marker, the time or the zone cannot be read — the
/// caller then falls back to the 30-minute default pause.
pub fn parse_reset_at(raw: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let lower = raw.to_lowercase();
    let marker = lower.find("resets")?;
    // Slice the ORIGINAL text at the same byte offset: `to_lowercase` can
    // change length for non-ASCII, so only index `raw` when the prefix is
    // ASCII. Reset lines are ASCII; bail out otherwise rather than panic.
    if !raw.is_char_boundary(marker) {
        return None;
    }
    let rest = raw.get(marker + "resets".len()..)?.trim_start();

    let time_token: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == ':' || c.is_ascii_alphabetic())
        .take(MAX_TIME_TOKEN_CHARS)
        .collect();
    let time = parse_wall_time(&time_token)?;

    let after_time = rest.get(time_token.len()..)?.trim_start();
    let zone_body = after_time.strip_prefix('(')?;
    let end = zone_body.find(')')?;
    if end > MAX_ZONE_CHARS {
        return None;
    }
    let tz = Tz::from_str(zone_body.get(..end)?.trim()).ok()?;

    next_local_occurrence(tz, time, now)
}

/// Parse `4:20am`, `12:40AM`, `16:05` into a wall time.
fn parse_wall_time(token: &str) -> Option<NaiveTime> {
    let lower = token.to_ascii_lowercase();
    let (digits, meridiem) = if let Some(head) = lower.strip_suffix("am") {
        (head, Some(false))
    } else if let Some(head) = lower.strip_suffix("pm") {
        (head, Some(true))
    } else {
        (lower.as_str(), None)
    };
    let (h, m) = digits.split_once(':')?;
    let hour: u32 = h.parse().ok()?;
    let minute: u32 = m.parse().ok()?;
    if minute > 59 {
        return None;
    }
    let hour24 = match meridiem {
        // 12am is 00:00, 12pm is 12:00; every other hour shifts by 12 for pm.
        Some(true) if hour == 12 => 12,
        Some(true) if hour < 12 => hour + 12,
        Some(false) if hour == 12 => 0,
        Some(false) if hour < 12 => hour,
        None if hour < 24 => hour,
        _ => return None,
    };
    NaiveTime::from_hms_opt(hour24, minute, 0)
}

/// The next instant at which the clock in `tz` reads `time`, strictly after
/// `now`.
///
/// A wall time that is ambiguous (the repeated hour when DST ends) resolves to
/// the earlier of the two instants: the agent should probe as soon as the
/// provider could plausibly be back. A wall time that does not exist (the
/// skipped hour when DST starts) rolls to the next day.
fn next_local_occurrence(tz: Tz, time: NaiveTime, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let mut date = now.with_timezone(&tz).date_naive();
    for _ in 0..=MAX_RESET_SEARCH_DAYS {
        let naive = date.and_time(time);
        let candidate = match tz.from_local_datetime(&naive) {
            chrono::LocalResult::Single(dt) => Some(dt.with_timezone(&Utc)),
            chrono::LocalResult::Ambiguous(earlier, _) => Some(earlier.with_timezone(&Utc)),
            chrono::LocalResult::None => None,
        };
        if let Some(candidate) = candidate {
            if candidate > now {
                return Some(candidate);
            }
        }
        date = date.succ_opt()?;
        // Guard against a pathological calendar walk; `succ_opt` already
        // returns None at the end of the representable range.
        if date.year() > now.year() + 1 {
            return None;
        }
    }
    None
}
