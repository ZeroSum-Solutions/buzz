//! Channel notice templates.
//!
//! At most one notice per pause per channel, one per park, one per breaker
//! open. Every caller-supplied string is capped here, at the DTO, before it
//! reaches a relay post: the reason text comes from a provider error and the
//! agent name from configuration.
//!
//! Design: `docs/plans/2026-09-06-harness-reliability-design.md`, "Notices in the channel".

use chrono::{DateTime, Utc};

use super::error_class::truncate_chars;

/// Longest agent name shown in a notice.
pub const MAX_NAME_CHARS: usize = 64;

/// Longest reason shown in a notice.
pub const MAX_REASON_CHARS: usize = 120;

/// Hard cap on any notice this module produces.
pub const MAX_NOTICE_CHARS: usize = 600;

/// The pause notice. `until` is rendered in UTC because the harness has no
/// authority over the reader's zone; the reset instant is unambiguous.
pub fn pause(agent_name: &str, until: DateTime<Utc>, waiting: usize) -> String {
    let name = truncate_chars(agent_name.trim(), MAX_NAME_CHARS);
    let name = if name.is_empty() { "This agent" } else { &name };
    let plural = if waiting == 1 {
        "message is"
    } else {
        "messages are"
    };
    cap(format!(
        "⏸️ {name} is paused until {} (provider capacity limit). {waiting} {plural} saved and \
will be answered in order when I am back. To switch seats now run `cswap switch` and restart \
the Claude agents.",
        until.format("%H:%M UTC on %Y-%m-%d")
    ))
}

/// The park notice, posted once per parked batch.
pub fn parked(reason: &str) -> String {
    cap(format!(
        "⚠️ I could not process the last request after several attempts ({}). It is saved and \
will be retried as soon as I am back. Nothing is lost.",
        truncate_chars(reason.trim(), MAX_REASON_CHARS)
    ))
}

/// The needs-review notice, for a batch that had already started.
pub fn needs_review() -> String {
    cap(
        "⚠️ A request was interrupted after it had started, so it will not run again on its own. \
Devin can retry or discard it from the Agents screen."
            .to_string(),
    )
}

/// The breaker notice, posted once per breaker open.
pub fn breaker(agent_name: &str) -> String {
    let name = truncate_chars(agent_name.trim(), MAX_NAME_CHARS);
    let name = if name.is_empty() {
        "This agent's".to_string()
    } else {
        format!("{name}'s")
    };
    cap(format!(
        "⚠️ {name} provider is returning errors. I will try again every 10 minutes and answer in \
order when it recovers."
    ))
}

/// Told to the operator when state files could not be written.
pub fn state_write_failures(ledger_failures: u64, park_failures: u64) -> String {
    cap(format!(
        "⚠️ I could not write my own reliability state ({ledger_failures} ledger and \
{park_failures} park-file write failures). Saved requests are held in memory only until this \
is fixed."
    ))
}

fn cap(text: String) -> String {
    truncate_chars(&text, MAX_NOTICE_CHARS)
}
