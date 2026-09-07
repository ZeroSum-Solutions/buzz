//! Harness reliability: error classes, pause, breaker, park and replay.
//!
//! Design: `docs/plans/2026-09-06-harness-reliability-design.md` (T16).
//!
//! The harness never discards a message. A failure that used to dead-letter a
//! batch now **parks** it — writes it to a durable, owner-only park file in the
//! agent's state directory — and either replays it automatically after a
//! successful live turn or hands it to the operator for review.
//!
//! Submodules:
//!
//! - [`error_class`] — pure classification of a provider error, and the
//!   `resets H:MM(am|pm) (IANA zone)` parser.
//! - [`state`] — the per-agent pause and the per-scope breakers.
//! - [`state_dir`] — where the durable state lives and how it is locked down.
//! - [`ledger`] — the append-only `ledger.jsonl`.
//! - [`park`] — the `parked.jsonl` park file.
//! - [`notices`] — the channel notice templates.
//! - [`runtime`] — the glue that owns all of the above and orders the writes.

use chrono::{DateTime, Utc};

pub mod error_class;
pub mod ledger;
pub mod notices;
pub mod park;
pub mod runtime;
pub mod state;
pub mod state_dir;

pub use error_class::{classify_at, sanitize_error_diagnostic};
pub use park::{ParkError, ParkReason, ParkedBatch};
pub use runtime::{DiscardOutcome, ReliabilityRuntime, ReplayPlan};
pub use state::{BreakerGate, BreakerVerdict, PauseGate, ReliabilityState};

/// Longest provider error text the harness inspects, stores or forwards.
///
/// Provider error strings are untrusted input: they are attacker-influenceable
/// through tool output and model output. Everything downstream — the ledger's
/// `raw` field, notice text, log lines — starts from a string cut to this.
pub const MAX_RAW_ERROR_CHARS: usize = 512;

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

impl ErrorClass {
    /// The class name written to the ledger.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::CapacityExhausted { .. } => "capacity_exhausted",
            Self::Auth => "auth",
            Self::ProviderInternal => "provider_internal",
            Self::Unknown => "unknown",
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::AcpError;
    use crate::config::DedupMode;
    use crate::queue::{EventQueue, QueuedEvent, MAX_RETRIES};
    use crate::scope::SessionScope;
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

    #[test]
    fn non_provider_failure_resets_consecutive_streak() {
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
            ErrorClass::Auth,
            now + chrono::Duration::seconds(10),
        );
        let fourth = state.on_failure(
            &scope,
            ErrorClass::ProviderInternal,
            now + chrono::Duration::seconds(15),
        );

        assert_eq!(first, Action::Retry);
        assert_eq!(second, Action::Retry);
        assert_eq!(third, Action::Park);
        assert_eq!(
            fourth,
            Action::Retry,
            "streak must be reset by Auth failure"
        );
    }

    #[test]
    fn test_consecutive_map_stays_bounded_across_many_scopes() {
        let now = log_instant();
        let mut state = ReliabilityState::default();

        for _ in 0..50 {
            let scope = SessionScope::Conversation {
                channel_id: Uuid::new_v4(),
            };
            let first = state.on_failure(&scope, ErrorClass::ProviderInternal, now);
            assert_eq!(first, Action::Retry);
            assert!(state.consecutive_len() > 0);

            let terminal = state.on_failure(&scope, ErrorClass::Auth, now);
            assert_eq!(terminal, Action::Park);
            // After the terminal park, the scope is removed from consecutive.
            assert_eq!(state.consecutive_len(), 0);
        }

        assert_eq!(state.consecutive_len(), 0);
    }
}
