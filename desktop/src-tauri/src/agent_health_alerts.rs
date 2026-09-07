//! Agent health alerts evaluation and rules (T17 Step 10).
//!
//! Evaluates incoming health events and parked batches against the five
//! alert conditions from design §4:
//! 1. Parked batch older than 15 minutes
//! 2. Batch entered needs review
//! 3. Breaker opened
//! 4. Pause longer than 1 hour started
//! 5. Agent process exited with non-zero status

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::agent_health::{HealthEvent, ParkedBatchView};

pub const RULE_PARKED_OLDER_THAN_15_MINUTES: &str = "parked_older_than_15_minutes";
pub const RULE_NEEDS_REVIEW: &str = "needs_review";
pub const RULE_BREAKER_OPENED: &str = "breaker_opened";
pub const RULE_PAUSE_LONGER_THAN_1_HOUR: &str = "pause_longer_than_1_hour";
pub const RULE_NON_ZERO_EXIT: &str = "non_zero_exit";

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertRule {
    ParkedOlderThan15Minutes,
    NeedsReview,
    BreakerOpened,
    PauseLongerThan1Hour,
    NonZeroExit,
}

#[allow(dead_code)]
impl AlertRule {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ParkedOlderThan15Minutes => RULE_PARKED_OLDER_THAN_15_MINUTES,
            Self::NeedsReview => RULE_NEEDS_REVIEW,
            Self::BreakerOpened => RULE_BREAKER_OPENED,
            Self::PauseLongerThan1Hour => RULE_PAUSE_LONGER_THAN_1_HOUR,
            Self::NonZeroExit => RULE_NON_ZERO_EXIT,
        }
    }
}

impl std::fmt::Display for AlertRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Alert {
    pub agent: String,
    pub rule: String,
    pub title: String,
    pub body: String,
}

pub fn normalize_rule(rule: &str) -> &'static str {
    match rule {
        "parked_older_than_15_minutes" | "parked_15m" | "parked_batch_older_than_15_minutes" => {
            RULE_PARKED_OLDER_THAN_15_MINUTES
        }
        "needs_review" | "batch_needs_review" => RULE_NEEDS_REVIEW,
        "breaker_opened" => RULE_BREAKER_OPENED,
        "pause_longer_than_1_hour" | "pause_1h" | "agent_paused" => RULE_PAUSE_LONGER_THAN_1_HOUR,
        "non_zero_exit" | "process_exit" | "agent_exit" => RULE_NON_ZERO_EXIT,
        _ => "unknown",
    }
}

fn should_fire(
    agent: &str,
    rule: &str,
    now: DateTime<Utc>,
    last_fired: &HashMap<(String, String), DateTime<Utc>>,
    fired_this_run: &HashSet<(String, String)>,
) -> bool {
    let norm = normalize_rule(rule);
    if fired_this_run.contains(&(agent.to_string(), norm.to_string())) {
        return false;
    }

    for candidate in [norm, rule] {
        if let Some(fired_at) = last_fired.get(&(agent.to_string(), candidate.to_string())) {
            let elapsed = now.signed_duration_since(*fired_at);
            if elapsed.num_seconds() >= 0 && elapsed.num_seconds() < 3600 {
                return false;
            }
        }
    }

    let aliases: &[&str] = match norm {
        RULE_PARKED_OLDER_THAN_15_MINUTES => &["parked_15m", "parked_batch_older_than_15_minutes"],
        RULE_PAUSE_LONGER_THAN_1_HOUR => &["pause_1h", "agent_paused"],
        RULE_NON_ZERO_EXIT => &["process_exit", "agent_exit"],
        _ => &[],
    };

    for alias in aliases {
        if let Some(fired_at) = last_fired.get(&(agent.to_string(), alias.to_string())) {
            let elapsed = now.signed_duration_since(*fired_at);
            if elapsed.num_seconds() >= 0 && elapsed.num_seconds() < 3600 {
                return false;
            }
        }
    }

    true
}

/// Evaluates health events and parked batches against alert rules with a 1-hour rate limit.
pub fn evaluate(
    events: &[HealthEvent],
    parked: &[ParkedBatchView],
    now: DateTime<Utc>,
    last_fired: &HashMap<(String, String), DateTime<Utc>>,
) -> Vec<Alert> {
    let mut alerts = Vec::new();
    let mut fired_this_run: HashSet<(String, String)> = HashSet::new();

    // 1. Evaluate parked batches (older than 15 minutes, or needs_review)
    for batch in parked {
        let agent = batch
            .agent
            .as_deref()
            .or_else(|| events.first().map(|e| e.agent.as_str()))
            .unwrap_or("agent");

        if let Ok(parked_at) = DateTime::parse_from_rfc3339(&batch.parked_at) {
            let parked_at_utc = parked_at.with_timezone(&Utc);
            let elapsed = now.signed_duration_since(parked_at_utc);
            let minutes = elapsed.num_minutes();
            if minutes >= 15
                && should_fire(
                    agent,
                    RULE_PARKED_OLDER_THAN_15_MINUTES,
                    now,
                    last_fired,
                    &fired_this_run,
                )
            {
                fired_this_run.insert((
                    agent.to_string(),
                    RULE_PARKED_OLDER_THAN_15_MINUTES.to_string(),
                ));
                let body = if batch.events == 1 {
                    format!("{agent} has 1 saved message waiting for {minutes} minutes")
                } else {
                    format!(
                        "{agent} has {} saved messages waiting for {minutes} minutes",
                        batch.events
                    )
                };
                alerts.push(Alert {
                    agent: agent.to_string(),
                    rule: RULE_PARKED_OLDER_THAN_15_MINUTES.to_string(),
                    title: agent.to_string(),
                    body,
                });
            }
        }

        if batch.needs_review
            && should_fire(agent, RULE_NEEDS_REVIEW, now, last_fired, &fired_this_run)
        {
            fired_this_run.insert((agent.to_string(), RULE_NEEDS_REVIEW.to_string()));
            alerts.push(Alert {
                agent: agent.to_string(),
                rule: RULE_NEEDS_REVIEW.to_string(),
                title: agent.to_string(),
                body: format!("A {agent} request needs your decision"),
            });
        }
    }

    // 2. Evaluate events (needs_review, breaker_opened, pause > 1h, non-zero exit)
    for event in events {
        let agent = &event.agent;
        match event.kind.as_str() {
            "batch_needs_review" => {
                if should_fire(agent, RULE_NEEDS_REVIEW, now, last_fired, &fired_this_run) {
                    fired_this_run.insert((agent.to_string(), RULE_NEEDS_REVIEW.to_string()));
                    alerts.push(Alert {
                        agent: agent.to_string(),
                        rule: RULE_NEEDS_REVIEW.to_string(),
                        title: agent.to_string(),
                        body: format!("A {agent} request needs your decision"),
                    });
                }
            }
            "breaker_opened" => {
                if should_fire(agent, RULE_BREAKER_OPENED, now, last_fired, &fired_this_run) {
                    fired_this_run.insert((agent.to_string(), RULE_BREAKER_OPENED.to_string()));
                    alerts.push(Alert {
                        agent: agent.to_string(),
                        rule: RULE_BREAKER_OPENED.to_string(),
                        title: agent.to_string(),
                        body: format!("{agent}'s provider is failing; probing every 10 min"),
                    });
                }
            }
            "agent_paused" => {
                let payload_val = event
                    .payload
                    .as_deref()
                    .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok());
                let until_str = payload_val
                    .as_ref()
                    .and_then(|v| v.get("until").and_then(|u| u.as_str()));

                if let Some(until_raw) = until_str {
                    if let Ok(until_parsed) = DateTime::parse_from_rfc3339(until_raw) {
                        let until_utc = until_parsed.with_timezone(&Utc);
                        let event_start = DateTime::from_timestamp(event.at, 0).unwrap_or(now);
                        let pause_duration = until_utc.signed_duration_since(event_start);
                        if pause_duration.num_seconds() >= 3600
                            && should_fire(
                                agent,
                                RULE_PAUSE_LONGER_THAN_1_HOUR,
                                now,
                                last_fired,
                                &fired_this_run,
                            )
                        {
                            fired_this_run.insert((
                                agent.to_string(),
                                RULE_PAUSE_LONGER_THAN_1_HOUR.to_string(),
                            ));
                            let time_formatted = until_utc.format("%-I:%M %p").to_string();
                            alerts.push(Alert {
                                agent: agent.to_string(),
                                rule: RULE_PAUSE_LONGER_THAN_1_HOUR.to_string(),
                                title: agent.to_string(),
                                body: format!("{agent} is paused until {time_formatted}"),
                            });
                        }
                    }
                }
            }
            "process_exit" | "non_zero_exit" | "agent_exit" => {
                let payload_val = event
                    .payload
                    .as_deref()
                    .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok());
                let code = payload_val
                    .as_ref()
                    .and_then(|v| {
                        v.get("code")
                            .or_else(|| v.get("lastErrorCode"))
                            .or_else(|| v.get("lastExitCode"))
                    })
                    .and_then(|c| c.as_i64());
                let err_msg = event.class.as_deref().or_else(|| {
                    payload_val.as_ref().and_then(|v| {
                        v.get("lastError")
                            .or_else(|| v.get("message"))
                            .and_then(|m| m.as_str())
                    })
                });

                let is_error = code.map(|c| c != 0).unwrap_or(true);
                if is_error
                    && should_fire(agent, RULE_NON_ZERO_EXIT, now, last_fired, &fired_this_run)
                {
                    fired_this_run.insert((agent.to_string(), RULE_NON_ZERO_EXIT.to_string()));
                    let body = if let Some(msg) = err_msg {
                        format!("{agent} exited: {msg}")
                    } else if let Some(c) = code {
                        format!("{agent} exited with error code {c}")
                    } else {
                        format!("{agent} process exited with non-zero status")
                    };
                    alerts.push(Alert {
                        agent: agent.to_string(),
                        rule: RULE_NON_ZERO_EXIT.to_string(),
                        title: agent.to_string(),
                        body,
                    });
                }
            }
            _ => {
                // If a batch_parked event has needs_review payload
                let payload_val = event
                    .payload
                    .as_deref()
                    .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok());
                let needs_review = payload_val
                    .as_ref()
                    .and_then(|v| {
                        v.get("needsReview")
                            .or_else(|| v.get("needs_review"))
                            .and_then(|nr| nr.as_bool())
                    })
                    .unwrap_or(false);
                if needs_review
                    && should_fire(agent, RULE_NEEDS_REVIEW, now, last_fired, &fired_this_run)
                {
                    fired_this_run.insert((agent.to_string(), RULE_NEEDS_REVIEW.to_string()));
                    alerts.push(Alert {
                        agent: agent.to_string(),
                        rule: RULE_NEEDS_REVIEW.to_string(),
                        title: agent.to_string(),
                        body: format!("A {agent} request needs your decision"),
                    });
                }
            }
        }
    }

    alerts
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn each_rule_fires_once_per_hour() {
        let now = Utc::now();

        let parked = vec![ParkedBatchView {
            agent: Some("PM".to_string()),
            batch_id: "batch-1".to_string(),
            channel_id: "channel-1".to_string(),
            reason: "retries_exhausted".to_string(),
            started: true,
            needs_review: false,
            parked_at: (now - Duration::minutes(20)).to_rfc3339(),
            events: 3,
            excerpt: "saved message".to_string(),
        }];

        let events = vec![
            HealthEvent {
                agent: "Critic".to_string(),
                at: now.timestamp(),
                kind: "batch_needs_review".to_string(),
                event_key: "k1".to_string(),
                batch_id: Some("batch-2".to_string()),
                channel_id: None,
                class: None,
                payload: None,
            },
            HealthEvent {
                agent: "Critic".to_string(),
                at: now.timestamp(),
                kind: "breaker_opened".to_string(),
                event_key: "k2".to_string(),
                batch_id: None,
                channel_id: None,
                class: None,
                payload: Some("{\"scope\":\"project-1\"}".to_string()),
            },
            HealthEvent {
                agent: "PM".to_string(),
                at: now.timestamp(),
                kind: "agent_paused".to_string(),
                event_key: "k3".to_string(),
                batch_id: None,
                channel_id: None,
                class: None,
                payload: Some(
                    serde_json::json!({
                        "until": (now + Duration::hours(2)).to_rfc3339(),
                        "waiting": 5
                    })
                    .to_string(),
                ),
            },
            HealthEvent {
                agent: "Sentinel".to_string(),
                at: now.timestamp(),
                kind: "process_exit".to_string(),
                event_key: "k4".to_string(),
                batch_id: None,
                channel_id: None,
                class: Some("provider failure".to_string()),
                payload: Some("{\"code\":1}".to_string()),
            },
        ];

        let mut last_fired = HashMap::new();

        // 1. Initial evaluation: all five rules should fire
        let alerts = evaluate(&events, &parked, now, &last_fired);
        assert_eq!(alerts.len(), 5, "all 5 distinct rules must fire");

        let rules: Vec<&str> = alerts.iter().map(|a| a.rule.as_str()).collect();
        assert!(rules.contains(&RULE_PARKED_OLDER_THAN_15_MINUTES));
        assert!(rules.contains(&RULE_NEEDS_REVIEW));
        assert!(rules.contains(&RULE_BREAKER_OPENED));
        assert!(rules.contains(&RULE_PAUSE_LONGER_THAN_1_HOUR));
        assert!(rules.contains(&RULE_NON_ZERO_EXIT));

        // Record fired alerts
        for alert in &alerts {
            last_fired.insert((alert.agent.clone(), alert.rule.clone()), now);
        }

        // 2. Evaluation at 30 minutes later: zero alerts must fire
        let now_30m = now + Duration::minutes(30);
        let alerts_30m = evaluate(&events, &parked, now_30m, &last_fired);
        assert_eq!(
            alerts_30m.len(),
            0,
            "no alerts should fire within the 1-hour suppression window"
        );

        // 3. Evaluation at 61 minutes later: all 5 rules fire again
        let now_61m = now + Duration::minutes(61);
        let alerts_61m = evaluate(&events, &parked, now_61m, &last_fired);
        assert_eq!(
            alerts_61m.len(),
            5,
            "all 5 rules must fire again after 1 hour has passed"
        );
    }

    #[test]
    fn parked_batch_older_than_15_minutes_alerts() {
        let now = Utc::now();
        let last_fired = HashMap::new();

        // 1. Parked 10 minutes ago: should NOT alert
        let parked_10m = vec![ParkedBatchView {
            agent: Some("PM".to_string()),
            batch_id: "b-recent".to_string(),
            channel_id: "c-1".to_string(),
            reason: "retries_exhausted".to_string(),
            started: false,
            needs_review: false,
            parked_at: (now - Duration::minutes(10)).to_rfc3339(),
            events: 1,
            excerpt: "msg".to_string(),
        }];
        let alerts_recent = evaluate(&[], &parked_10m, now, &last_fired);
        assert!(
            alerts_recent.is_empty(),
            "batch parked for 10 min should not trigger >15min alert"
        );

        // 2. Parked 20 minutes ago: SHOULD alert
        let parked_20m = vec![ParkedBatchView {
            agent: Some("PM".to_string()),
            batch_id: "b-old".to_string(),
            channel_id: "c-1".to_string(),
            reason: "retries_exhausted".to_string(),
            started: true,
            needs_review: false,
            parked_at: (now - Duration::minutes(20)).to_rfc3339(),
            events: 3,
            excerpt: "msg".to_string(),
        }];
        let alerts_old = evaluate(&[], &parked_20m, now, &last_fired);
        assert_eq!(alerts_old.len(), 1);
        assert_eq!(alerts_old[0].agent, "PM");
        assert_eq!(alerts_old[0].rule, RULE_PARKED_OLDER_THAN_15_MINUTES);
        assert!(alerts_old[0].body.contains("PM"));
        assert!(alerts_old[0].body.contains("3 saved messages"));
        assert!(alerts_old[0].body.contains("20 minutes"));
    }

    #[test]
    fn needs_review_alert_names_the_agent() {
        let now = Utc::now();
        let last_fired = HashMap::new();

        let event_critic = HealthEvent {
            agent: "Critic".to_string(),
            at: now.timestamp(),
            kind: "batch_needs_review".to_string(),
            event_key: "nr-1".to_string(),
            batch_id: Some("b-critic".to_string()),
            channel_id: Some("c-critic".to_string()),
            class: None,
            payload: None,
        };

        let alerts = evaluate(&[event_critic], &[], now, &last_fired);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].agent, "Critic");
        assert_eq!(alerts[0].rule, RULE_NEEDS_REVIEW);
        assert!(alerts[0].title.contains("Critic"));
        assert!(alerts[0].body.contains("Critic"));
        assert_eq!(alerts[0].body, "A Critic request needs your decision");

        // Another agent name
        let event_pm = HealthEvent {
            agent: "PM".to_string(),
            at: now.timestamp(),
            kind: "batch_needs_review".to_string(),
            event_key: "nr-2".to_string(),
            batch_id: Some("b-pm".to_string()),
            channel_id: Some("c-pm".to_string()),
            class: None,
            payload: None,
        };
        let alerts_pm = evaluate(&[event_pm], &[], now, &last_fired);
        assert_eq!(alerts_pm.len(), 1);
        assert_eq!(alerts_pm[0].agent, "PM");
        assert!(alerts_pm[0].body.contains("PM"));
        assert_eq!(alerts_pm[0].body, "A PM request needs your decision");
    }
}
