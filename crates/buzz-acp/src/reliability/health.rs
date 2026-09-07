//! Agent health summary reducer over harness ledger records.
//!
//! Pure reducer that scans ledger records for an agent and computes windowed
//! activity counters (turns, failed, parked, needs_review, reconnects, last
//! error) and current state (active, paused, breaker, offline).

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::ledger::{LedgerBody, LedgerRecord, TurnOutcome};

/// Per-agent health counters and current state summarized from the ledger.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentHealthRow {
    #[serde(alias = "pubkey")]
    pub agent: String,
    pub state: String,
    #[serde(
        alias = "pausedUntil",
        alias = "latest_paused_until",
        alias = "latestPausedUntil"
    )]
    pub paused_until: Option<DateTime<Utc>>,
    pub turns: u64,
    pub failed: u64,
    pub parked: u64,
    #[serde(alias = "needsReview")]
    pub needs_review: u64,
    #[serde(alias = "reconnects24h")]
    pub reconnects: u64,
    #[serde(
        alias = "lastErrorClass",
        alias = "last_failure_class",
        alias = "lastFailureClass"
    )]
    pub last_error_class: Option<String>,
    #[serde(
        alias = "lastErrorAt",
        alias = "last_failure_at",
        alias = "lastFailureAt"
    )]
    pub last_error_at: Option<DateTime<Utc>>,
    #[serde(alias = "breakerOpen", alias = "latestBreakerOpen")]
    pub breaker_open: bool,
}

impl AgentHealthRow {
    pub fn pubkey(&self) -> &str {
        &self.agent
    }
}

/// Summarize ledger records for an agent within the `since` duration relative to `now`.
///
/// Activity counters (`turns`, `failed`, `parked`, `needs_review`, `reconnects`)
/// count events occurring at or after `now - since`.
/// Current `state`, `paused_until`, and `breaker_open` reflect the agent's latest
/// known state as of `now`.
pub fn summarize(records: &[LedgerRecord], since: Duration, now: DateTime<Utc>) -> AgentHealthRow {
    let cutoff = now - since;
    let agent = records.first().map(|r| r.agent.clone()).unwrap_or_default();

    let mut turns = 0u64;
    let mut failed = 0u64;
    let mut parked = 0u64;
    let mut needs_review = 0u64;
    let mut reconnects = 0u64;
    let mut last_error_class: Option<String> = None;
    let mut last_error_at: Option<DateTime<Utc>> = None;

    let mut latest_pause_or_resume: Option<(DateTime<Utc>, Option<DateTime<Utc>>)> = None;
    let mut latest_breaker: Option<(DateTime<Utc>, bool)> = None;

    for record in records {
        match &record.body {
            LedgerBody::AgentPaused(r)
                if latest_pause_or_resume
                    .as_ref()
                    .is_none_or(|(at, _)| record.at >= *at) =>
            {
                latest_pause_or_resume = Some((record.at, Some(r.until)));
            }
            LedgerBody::AgentResumed(_)
                if latest_pause_or_resume
                    .as_ref()
                    .is_none_or(|(at, _)| record.at >= *at) =>
            {
                latest_pause_or_resume = Some((record.at, None));
            }
            LedgerBody::BreakerOpened(_)
                if latest_breaker
                    .as_ref()
                    .is_none_or(|(at, _)| record.at >= *at) =>
            {
                latest_breaker = Some((record.at, true));
            }
            LedgerBody::BreakerClosed(_)
                if latest_breaker
                    .as_ref()
                    .is_none_or(|(at, _)| record.at >= *at) =>
            {
                latest_breaker = Some((record.at, false));
            }
            _ => {}
        }

        if record.at >= cutoff {
            match &record.body {
                LedgerBody::TurnFinished(r) => {
                    turns += 1;
                    if let TurnOutcome::Error { class, .. } = &r.outcome {
                        failed += 1;
                        if last_error_at.as_ref().is_none_or(|at| record.at >= *at) {
                            last_error_class = Some(class.clone());
                            last_error_at = Some(record.at);
                        }
                    }
                }
                LedgerBody::BatchParked(_) => {
                    parked += 1;
                }
                LedgerBody::BatchNeedsReview(_) => {
                    needs_review += 1;
                }
                LedgerBody::RelayReconnected(_) => {
                    reconnects += 1;
                }
                _ => {}
            }
        }
    }

    let raw_paused_until = latest_pause_or_resume.and_then(|(_, until)| until);
    let breaker_open = latest_breaker.is_some_and(|(_, open)| open);
    let is_paused = raw_paused_until.is_some_and(|until| until > now);

    let state = if breaker_open {
        "breaker".to_string()
    } else if is_paused {
        "paused".to_string()
    } else if records.is_empty() {
        "offline".to_string()
    } else {
        "active".to_string()
    };

    AgentHealthRow {
        agent,
        state,
        paused_until: raw_paused_until,
        turns,
        failed,
        parked,
        needs_review,
        reconnects,
        last_error_class,
        last_error_at,
        breaker_open,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reliability::ledger::{BatchNeedsReview, BatchParked, TurnFinished, TurnOutcome};
    use uuid::Uuid;

    #[test]
    fn summarize_counts_failed_parked_needs_review_within_window() {
        let now = Utc::now();
        let since = Duration::hours(24);
        let agent = "test_agent_pubkey".to_string();

        let batch_1 = Uuid::new_v4();
        let batch_2 = Uuid::new_v4();
        let batch_3 = Uuid::new_v4();
        let batch_4 = Uuid::new_v4();
        let channel_id = Uuid::new_v4();

        let records = vec![
            // Inside window:
            LedgerRecord {
                at: now - Duration::hours(2),
                agent: agent.clone(),
                body: LedgerBody::TurnFinished(TurnFinished {
                    batch_id: batch_1,
                    channel_id,
                    outcome: TurnOutcome::error("rate_limit", "429 Too Many Requests"),
                }),
            },
            LedgerRecord {
                at: now - Duration::hours(3),
                agent: agent.clone(),
                body: LedgerBody::TurnFinished(TurnFinished {
                    batch_id: batch_2,
                    channel_id,
                    outcome: TurnOutcome::Ok,
                }),
            },
            LedgerRecord {
                at: now - Duration::hours(4),
                agent: agent.clone(),
                body: LedgerBody::BatchParked(BatchParked {
                    batch_id: batch_1,
                    channel_id,
                    reason: "retries_exhausted".to_string(),
                    started: true,
                    events: 1,
                }),
            },
            LedgerRecord {
                at: now - Duration::hours(5),
                agent: agent.clone(),
                body: LedgerBody::BatchNeedsReview(BatchNeedsReview {
                    batch_id: batch_1,
                    channel_id,
                    reason: "decision_required".to_string(),
                }),
            },
            // Outside window (older than 24h):
            LedgerRecord {
                at: now - Duration::hours(48),
                agent: agent.clone(),
                body: LedgerBody::TurnFinished(TurnFinished {
                    batch_id: batch_3,
                    channel_id,
                    outcome: TurnOutcome::error("auth", "invalid key"),
                }),
            },
            LedgerRecord {
                at: now - Duration::hours(49),
                agent: agent.clone(),
                body: LedgerBody::BatchParked(BatchParked {
                    batch_id: batch_3,
                    channel_id,
                    reason: "retries_exhausted".to_string(),
                    started: true,
                    events: 1,
                }),
            },
            LedgerRecord {
                at: now - Duration::hours(50),
                agent: agent.clone(),
                body: LedgerBody::BatchNeedsReview(BatchNeedsReview {
                    batch_id: batch_4,
                    channel_id,
                    reason: "decision_required".to_string(),
                }),
            },
        ];

        let row = summarize(&records, since, now);

        assert_eq!(row.agent, agent);
        assert_eq!(row.turns, 2); // 1 error + 1 ok inside window
        assert_eq!(row.failed, 1); // 1 error inside window
        assert_eq!(row.parked, 1); // 1 parked inside window
        assert_eq!(row.needs_review, 1); // 1 needs review inside window
        assert_eq!(row.last_error_class.as_deref(), Some("rate_limit"));
        assert_eq!(row.state, "active");
    }
}
