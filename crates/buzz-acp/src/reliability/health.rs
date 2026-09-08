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
/// `turns`, `failed`, and `reconnects` count events occurring at or after
/// `now - since` — true activity totals for the window.
///
/// `parked` and `needs_review` are NOT windowed counts of park/review
/// events: they are the number of batches CURRENTLY sitting in each state,
/// tracked by batch id across the full record set and reconciled by
/// `batch_replayed`/`batch_discarded`. A batch parked eight days ago with no
/// resolution is still outstanding and must still show up — counting raw
/// `batch_parked`/`batch_needs_review` events inside the window would both
/// hide that old unresolved batch and keep counting a batch that was
/// resolved (replayed or discarded) minutes after entering the window.
///
/// Current `state`, `paused_until`, and `breaker_open` reflect the agent's latest
/// known state as of `now`.
pub fn summarize(records: &[LedgerRecord], since: Duration, now: DateTime<Utc>) -> AgentHealthRow {
    let cutoff = now - since;
    let agent = records.first().map(|r| r.agent.clone()).unwrap_or_default();

    let mut turns = 0u64;
    let mut failed = 0u64;
    let mut reconnects = 0u64;
    let mut last_error_class: Option<String> = None;
    let mut last_error_at: Option<DateTime<Utc>> = None;

    let mut latest_pause_or_resume: Option<(DateTime<Utc>, Option<DateTime<Utc>>)> = None;
    let mut open_breakers: std::collections::HashMap<String, (DateTime<Utc>, bool)> =
        std::collections::HashMap::new();
    let mut active_parked: std::collections::HashSet<uuid::Uuid> = std::collections::HashSet::new();
    let mut active_needs_review: std::collections::HashSet<uuid::Uuid> =
        std::collections::HashSet::new();

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
            LedgerBody::BreakerOpened(r)
                if open_breakers
                    .get(&r.scope)
                    .is_none_or(|(at, _)| record.at >= *at) =>
            {
                open_breakers.insert(r.scope.clone(), (record.at, true));
            }
            LedgerBody::BreakerClosed(r)
                if open_breakers
                    .get(&r.scope)
                    .is_none_or(|(at, _)| record.at >= *at) =>
            {
                open_breakers.insert(r.scope.clone(), (record.at, false));
            }
            LedgerBody::BatchParked(r) => {
                active_parked.insert(r.batch_id);
            }
            LedgerBody::BatchNeedsReview(r) => {
                active_needs_review.insert(r.batch_id);
            }
            LedgerBody::BatchReplayed(r) => {
                active_parked.remove(&r.batch_id);
                active_needs_review.remove(&r.batch_id);
            }
            LedgerBody::BatchDiscarded(r) => {
                active_parked.remove(&r.batch_id);
                active_needs_review.remove(&r.batch_id);
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
                LedgerBody::RelayReconnected(_) => {
                    reconnects += 1;
                }
                _ => {}
            }
        }
    }

    let raw_paused_until = latest_pause_or_resume.and_then(|(_, until)| until);
    let breaker_open = open_breakers.values().any(|(_, open)| *open);
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
        parked: active_parked.len() as u64,
        needs_review: active_needs_review.len() as u64,
        reconnects,
        last_error_class,
        last_error_at,
        breaker_open,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reliability::ledger::{
        BatchDiscarded, BatchNeedsReview, BatchParked, BatchReplayed, TurnFinished, TurnOutcome,
    };
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
                                   // `parked`/`needs_review` are current outstanding batch counts, not
                                   // windowed event counts: batch_3 (parked 49h ago, never resolved) and
                                   // batch_4 (needs-review 50h ago, never resolved) are both still
                                   // outstanding even though their events fall outside the 24h window.
        assert_eq!(row.parked, 2); // batch_1 and batch_3, still unresolved
        assert_eq!(row.needs_review, 2); // batch_1 and batch_4, still unresolved
        assert_eq!(row.last_error_class.as_deref(), Some("rate_limit"));
        assert_eq!(row.state, "active");
    }

    #[test]
    fn parked_and_needs_review_are_cleared_by_replay_or_discard() {
        let now = Utc::now();
        let since = Duration::hours(24);
        let agent = "test_agent_pubkey".to_string();
        let channel_id = Uuid::new_v4();
        let batch_replayed = Uuid::new_v4();
        let batch_discarded = Uuid::new_v4();
        let batch_still_open = Uuid::new_v4();

        let park = |batch_id: Uuid, at: DateTime<Utc>| LedgerRecord {
            at,
            agent: agent.clone(),
            body: LedgerBody::BatchParked(BatchParked {
                batch_id,
                channel_id,
                reason: "retries_exhausted".to_string(),
                started: true,
                events: 1,
            }),
        };

        let records = vec![
            park(batch_replayed, now - Duration::hours(2)),
            LedgerRecord {
                at: now - Duration::hours(1),
                agent: agent.clone(),
                body: LedgerBody::BatchReplayed(BatchReplayed {
                    batch_id: batch_replayed,
                    channel_id,
                    replay_of: Uuid::new_v4(),
                }),
            },
            park(batch_discarded, now - Duration::hours(2)),
            LedgerRecord {
                at: now - Duration::hours(1),
                agent: agent.clone(),
                body: LedgerBody::BatchDiscarded(BatchDiscarded {
                    batch_id: batch_discarded,
                    channel_id,
                    by: "operator".to_string(),
                }),
            },
            park(batch_still_open, now - Duration::hours(2)),
            // Duplicate needs-review event for the still-open batch must not
            // double count (HashSet insert is idempotent).
            LedgerRecord {
                at: now - Duration::hours(2),
                agent: agent.clone(),
                body: LedgerBody::BatchNeedsReview(BatchNeedsReview {
                    batch_id: batch_still_open,
                    channel_id,
                    reason: "decision_required".to_string(),
                }),
            },
            LedgerRecord {
                at: now - Duration::hours(1),
                agent: agent.clone(),
                body: LedgerBody::BatchNeedsReview(BatchNeedsReview {
                    batch_id: batch_still_open,
                    channel_id,
                    reason: "decision_required".to_string(),
                }),
            },
        ];

        let row = summarize(&records, since, now);
        assert_eq!(row.parked, 1, "only batch_still_open remains parked");
        assert_eq!(
            row.needs_review, 1,
            "duplicate needs_review events for one batch must not double count"
        );
    }

    #[test]
    fn breaker_tracks_per_scope() {
        let now = Utc::now();
        let agent = "test_agent".to_string();
        let records = vec![
            LedgerRecord {
                at: now - Duration::hours(3),
                agent: agent.clone(),
                body: LedgerBody::BreakerOpened(crate::reliability::ledger::BreakerOpened {
                    scope: "scope_a".to_string(),
                    consecutive: 3,
                }),
            },
            LedgerRecord {
                at: now - Duration::hours(2),
                agent: agent.clone(),
                body: LedgerBody::BreakerOpened(crate::reliability::ledger::BreakerOpened {
                    scope: "scope_b".to_string(),
                    consecutive: 3,
                }),
            },
            LedgerRecord {
                at: now - Duration::hours(1),
                agent: agent.clone(),
                body: LedgerBody::BreakerClosed(crate::reliability::ledger::BreakerClosed {
                    scope: "scope_a".to_string(),
                }),
            },
        ];

        let row = summarize(&records, Duration::hours(24), now);
        assert!(
            row.breaker_open,
            "scope_b is still open, breaker_open must be true"
        );
    }
}
