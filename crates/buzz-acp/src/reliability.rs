//! Harness reliability: error classes, pause, breaker, park and replay.
//!
//! Design: `docs/plans/2026-09-06-harness-reliability-design.md` (T16).
//!
//! This file currently holds the **fixture tests** for T16 and the smallest
//! stubs that let them compile. Every test is `#[ignore]` with the ticket
//! named in the reason, so the branch stays green while the behaviour is
//! missing. Run them with:
//!
//! ```text
//! cargo test -p buzz-acp reliability -- --ignored
//! ```
//!
//! They fail today. T16 is ready when they pass without `#[ignore]`.

// The stubs below have no caller until T16 wires them into the pool.
#![allow(dead_code)]

use chrono::{DateTime, Utc};

use crate::acp::AcpError;
use crate::scope::SessionScope;

/// Why a turn failed, as far as the harness can tell from the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorClass {
    /// The account is out of capacity for now: session limit, rate limit,
    /// overloaded, quota. `resets_at` is the parsed reset time when the
    /// provider named one.
    CapacityExhausted { resets_at: Option<DateTime<Utc>> },
    /// The provider rejected the credentials. A re-login fixes it; a retry
    /// does not.
    Auth,
    /// The provider is broken for now (plain internal error, 5xx).
    ProviderInternal,
    /// Anything else.
    Unknown,
}

/// What the harness does with the scope after an outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Existing backoff path.
    Retry,
    /// Freeze the whole agent until `until`; retry counts untouched.
    Pause { until: DateTime<Utc> },
    /// Freeze this scope; probe every ten minutes.
    OpenBreaker,
    /// Put the batch in the park file.
    Park,
}

/// Classify a provider error at a known instant. `now` is a parameter so a
/// reset time parsed from "resets 4:20am (America/Los_Angeles)" resolves to
/// the same absolute instant in a test as in production.
///
/// STUB: T16 replaces this body. It exists so the fixtures compile.
pub fn classify_at(_err: &AcpError, _now: DateTime<Utc>) -> ErrorClass {
    ErrorClass::Unknown
}

/// Per-agent reliability state: pause and per-scope breakers.
///
/// STUB: T16 replaces this body.
#[derive(Debug, Default)]
pub struct ReliabilityState {
    /// Placeholder so the stub is not a unit struct; T16 replaces it with
    /// the pause and per-scope breaker fields.
    _pending: (),
}

impl ReliabilityState {
    /// Record one failed outcome for `scope` and decide what happens next.
    pub fn on_failure(
        &mut self,
        _scope: &SessionScope,
        _class: ErrorClass,
        _now: DateTime<Utc>,
    ) -> Action {
        Action::Retry
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DedupMode;
    use crate::queue::{EventQueue, QueuedEvent, MAX_RETRIES};
    use chrono::TimeZone;
    use nostr::{EventBuilder, Keys, Kind};
    use std::time::Instant;
    use uuid::Uuid;

    /// The four provider error lines from the agent logs of 2026-09-02/03.
    const SESSION_LIMIT_0420: &str =
        "Internal error: You've hit your session limit · resets 4:20am (America/Los_Angeles)";
    const SESSION_LIMIT_0040: &str =
        "Internal error: You've hit your session limit · resets 12:40am (America/Los_Angeles)";
    const PLAIN_INTERNAL: &str = "Internal error";
    const UNRELATED: &str = "Tool 'read_file' returned no content";

    fn agent_error(message: &str) -> AcpError {
        AcpError::AgentError {
            code: -32603,
            message: message.to_string(),
        }
    }

    /// 2026-09-02T09:57:24Z, the timestamp of the first dead-letter line.
    fn log_instant() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 2, 9, 57, 24).unwrap()
    }

    fn make_queued(channel_id: Uuid, content: &str) -> QueuedEvent {
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::Custom(9), content)
            .tags([])
            .sign_with_keys(&keys)
            .unwrap();
        QueuedEvent {
            channel_id,
            scope: SessionScope::Conversation { channel_id },
            event,
            received_at: Instant::now(),
            prompt_tag: "test".into(),
        }
    }

    // 1. Error classes on the real log lines.

    #[test]
    #[ignore = "T16 fixture: fails until classify_at parses the Claude session-limit line"]
    fn a_session_limit_with_a_morning_reset_is_capacity_exhausted_until_that_time() {
        // 04:20 America/Los_Angeles on 2026-09-02 is PDT, so 11:20 UTC, later
        // the same day as the log line.
        let expected = Utc.with_ymd_and_hms(2026, 9, 2, 11, 20, 0).unwrap();
        assert_eq!(
            classify_at(&agent_error(SESSION_LIMIT_0420), log_instant()),
            ErrorClass::CapacityExhausted {
                resets_at: Some(expected)
            }
        );
    }

    #[test]
    #[ignore = "T16 fixture: fails until classify_at parses the Claude session-limit line"]
    fn a_session_limit_whose_reset_already_passed_today_resolves_to_tomorrow() {
        // 00:40 America/Los_Angeles is 07:40 UTC. At 09:57 UTC that is already
        // past, so the next occurrence is 2026-09-03T07:40Z.
        let expected = Utc.with_ymd_and_hms(2026, 9, 3, 7, 40, 0).unwrap();
        assert_eq!(
            classify_at(&agent_error(SESSION_LIMIT_0040), log_instant()),
            ErrorClass::CapacityExhausted {
                resets_at: Some(expected)
            }
        );
    }

    #[test]
    #[ignore = "T16 fixture: fails until classify_at recognises a bare internal error"]
    fn a_plain_internal_error_is_provider_internal() {
        assert_eq!(
            classify_at(&agent_error(PLAIN_INTERNAL), log_instant()),
            ErrorClass::ProviderInternal
        );
    }

    #[test]
    fn an_unrelated_agent_error_is_unknown() {
        // Passes today by construction; kept so the class set is complete.
        assert_eq!(
            classify_at(&agent_error(UNRELATED), log_instant()),
            ErrorClass::Unknown
        );
    }

    // 2. The queue never hands a batch back to be discarded.

    #[test]
    #[ignore = "T16 fixture: fails until retry exhaustion parks the batch instead of returning it"]
    fn retry_exhaustion_parks_the_batch_and_returns_nothing_to_discard() {
        let channel_id = Uuid::new_v4();
        let mut queue = EventQueue::new(DedupMode::Queue);
        assert!(queue.push(make_queued(channel_id, "please review section 20")));
        let batch = queue.flush_next().expect("one batch");
        // mark_complete clears a scope's retry count when no backoff is
        // active, so the count is set after it, as the queue's own tests do.
        queue.mark_complete(SessionScope::Conversation { channel_id });
        queue.set_retry_count_for_test(SessionScope::Conversation { channel_id }, MAX_RETRIES);

        let returned = queue.requeue(batch);

        assert!(
            returned.is_none(),
            "after {} retries the batch must be parked inside the queue, never returned for discard",
            MAX_RETRIES
        );
    }

    // 3. Capacity exhaustion pauses the agent and spends no retries.

    #[test]
    #[ignore = "T16 fixture: fails until ReliabilityState pauses on CapacityExhausted"]
    fn capacity_exhausted_pauses_until_the_reset_time() {
        let channel_id = Uuid::new_v4();
        let scope = SessionScope::Conversation { channel_id };
        let resets_at = Utc.with_ymd_and_hms(2026, 9, 2, 11, 20, 0).unwrap();
        let mut state = ReliabilityState::default();

        let action = state.on_failure(
            &scope,
            ErrorClass::CapacityExhausted {
                resets_at: Some(resets_at),
            },
            log_instant(),
        );

        assert_eq!(action, Action::Pause { until: resets_at });
    }

    #[test]
    #[ignore = "T16 fixture: fails until an unparseable reset time pauses for 30 minutes"]
    fn capacity_exhausted_without_a_reset_time_pauses_thirty_minutes() {
        let channel_id = Uuid::new_v4();
        let scope = SessionScope::Conversation { channel_id };
        let now = log_instant();
        let mut state = ReliabilityState::default();

        let action = state.on_failure(
            &scope,
            ErrorClass::CapacityExhausted { resets_at: None },
            now,
        );

        assert_eq!(
            action,
            Action::Pause {
                until: now + chrono::Duration::minutes(30)
            }
        );
    }

    // 4. Three consecutive provider errors open the breaker for that scope.

    #[test]
    #[ignore = "T16 fixture: fails until three consecutive ProviderInternal failures open the breaker"]
    fn three_consecutive_provider_errors_open_the_breaker() {
        let channel_id = Uuid::new_v4();
        let scope = SessionScope::Conversation { channel_id };
        let now = log_instant();
        let mut state = ReliabilityState::default();

        let first = state.on_failure(&scope, ErrorClass::ProviderInternal, now);
        let second = state.on_failure(
            &scope,
            ErrorClass::ProviderInternal,
            now + chrono::Duration::seconds(5),
        );
        let third = state.on_failure(
            &scope,
            ErrorClass::ProviderInternal,
            now + chrono::Duration::seconds(15),
        );

        assert_eq!(first, Action::Retry);
        assert_eq!(second, Action::Retry);
        assert_eq!(third, Action::OpenBreaker);
    }
}
