//! Google Calendar as a scoped integration: the authorization contract and the
//! event contract.
//!
//! Accepted designs: `docs/plans/2026-09-04-calendar-authorization.md` (T11)
//! and `docs/plans/2026-09-04-calendar-view-design.md` (T12a). Every decision
//! this module implements is cited by number where it is implemented.
//!
//! Native production wiring lives in `commands::calendar`: human UI commands,
//! a personal primary calendar per Buzz identity, bounded OAuth/JWKS transport,
//! generation-fenced credentials and a separate SQLite render cache. The
//! renderer receives status and bounded event data, never grant tokens.
//!
//! The pieces, and the decisions each answers:
//!
//! * [`redact`] — the redacting wrapper every credential is held in
//!   (T11 decision 2).
//! * [`oauth`] — the PKCE authorization request, the `state` and `nonce`
//!   checks, and the exchange and ID-token conditions a binding needs
//!   (T11 decisions 1 and 2).
//! * [`loopback`] — the bounded loopback listener the callback arrives on
//!   (T11 decision 1).
//! * [`binding`] — the stored envelope and the transition-specific
//!   compare-and-set predicate every write passes (T11 decisions 2, 5 and 6).
//! * [`revocation`] — the revocation journal, its backoff and its terminal
//!   state (T11 decision 5).
//! * [`failure`] — the three error matrices and their four states
//!   (T11 decision 8).
//! * [`dto`] — the bounded event DTO and the only parser API-sourced text
//!   enters through (T12a decisions 1 and 2).
//! * [`interval`] — the interval a batch proves (T12a decision 13).
//! * [`client`] — the bounded `events.list` walk and the three mutations
//!   (T11 decision 6, T12a decisions 5, 11 and 13).
//!
//! Calendar remains unavailable to agent/CLI/MCP adapters. The credential key
//! stays outside `mcp:`; only explicit native UI commands are registered.

pub mod binding;
pub mod cache;
pub mod client;
pub mod dto;
pub mod failure;
pub mod interval;
pub mod loopback;
pub mod oauth;
pub mod provider;
pub mod redact;
pub mod revocation;

#[cfg(test)]
mod binding_tests;
#[cfg(test)]
mod client_tests;
#[cfg(test)]
mod mock_server;
#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;

use interval::Window;

/// Days of history one fetch covers (T11 decision 6).
pub const WINDOW_DAYS_BACK: i64 = 30;
/// Days ahead one fetch covers (T11 decision 6).
pub const WINDOW_DAYS_AHEAD: i64 = 90;
/// How long a successful authorization refresh keeps the view authoritative
/// (T11 decision 6). Absolute: never extended by a failure or a restart.
pub const STALE_AFTER_MS: i64 = 24 * 60 * 60 * 1000;

/// Milliseconds in a day.
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// The fixed 120-day horizon every fetch and every view stays inside
/// (T11 decision 6, T12a decision 8).
pub fn default_window(now_ms: i64) -> Window {
    Window::new(
        now_ms.saturating_sub(WINDOW_DAYS_BACK * DAY_MS),
        now_ms.saturating_add(WINDOW_DAYS_AHEAD * DAY_MS),
    )
}

/// The absolute staleness bound set by a successful authorization refresh at
/// `refreshed_at_ms`.
pub fn stale_after_ms(refreshed_at_ms: i64) -> i64 {
    refreshed_at_ms.saturating_add(STALE_AFTER_MS)
}
