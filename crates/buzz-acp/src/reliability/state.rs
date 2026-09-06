//! Per-agent pause state and per-scope breakers.
//!
//! Design: `docs/plans/2026-09-06-harness-reliability-design.md`, "State machine".
//!
//! ```text
//! Active --CapacityExhausted--> Paused{until}
//! Paused --timer--> Probing        (first queued batch is the probe)
//! Probing --Ok--> Active           (then replay)
//! Probing --CapacityExhausted--> Paused{new until}  (notice only if until moved > 15 min)
//! Active --3 consecutive ProviderInternal/Unknown on one scope--> BreakerOpen{scope}
//! BreakerOpen --every 10 min--> probe one batch; Ok closes it; open at most 6 h then Park
//! ```

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use crate::scope::SessionScope;

use super::{Action, ErrorClass};

/// A pause never lasts longer than this, however far in the future the provider
/// says its reset is.
pub const MAX_PAUSE_HOURS: i64 = 6;

/// Pause length when the provider named no reset time, or named one we could
/// not parse.
pub const DEFAULT_PAUSE_MINUTES: i64 = 30;

/// Consecutive `ProviderInternal` / `Unknown` failures on one scope that open
/// its breaker.
pub const BREAKER_THRESHOLD: u32 = 3;

/// How often an open breaker lets one batch through as a probe.
pub const BREAKER_PROBE_MINUTES: i64 = 10;

/// A breaker never stays open longer than this; the batch is parked instead.
pub const BREAKER_MAX_OPEN_HOURS: i64 = 6;

/// A re-pause re-notifies a channel only when the new reset time moved by more
/// than this.
pub const PAUSE_RENOTIFY_MINUTES: i64 = 15;

/// Whether the agent may run a turn right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseGate {
    /// No pause is in force.
    Open,
    /// Paused until this instant; nothing runs.
    Held { until: DateTime<Utc> },
    /// The pause expired: exactly one batch may run as the probe.
    Probe,
}

/// Whether an open breaker lets this scope run right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerGate {
    /// No breaker is open for the scope.
    Closed,
    /// Open and inside the probe interval; nothing runs for this scope.
    Held { next_probe: DateTime<Utc> },
    /// Open and due: exactly one batch may run as the probe.
    Probe,
}

/// What a failure during a breaker probe means for the batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerVerdict {
    /// Still inside the six-hour window: try again after the probe interval.
    Reschedule { next_probe: DateTime<Utc> },
    /// The breaker has been open for six hours; park the batch and close it.
    Park,
}

#[derive(Debug, Clone)]
struct Pause {
    until: DateTime<Utc>,
    /// The `until` the channels in `notified_channels` were told about.
    notified_until: DateTime<Utc>,
    notified_channels: HashSet<Uuid>,
    /// Set once the pause expires and a probe has been handed out, so only one
    /// batch probes per expiry.
    probe_issued: bool,
}

#[derive(Debug, Clone)]
struct Breaker {
    opened_at: DateTime<Utc>,
    next_probe: DateTime<Utc>,
    probe_issued: bool,
    /// Consecutive failures that opened it, for the ledger record.
    consecutive: u32,
}

/// Per-agent reliability state: one pause for the whole agent (a capacity
/// limit belongs to the account) and one breaker per scope.
#[derive(Debug, Default)]
pub struct ReliabilityState {
    pause: Option<Pause>,
    breakers: HashMap<SessionScope, Breaker>,
    consecutive: HashMap<SessionScope, u32>,
}

impl ReliabilityState {
    /// Record one failed outcome for `scope` and decide what happens next.
    ///
    /// Retry counts are the queue's business; this returns only the class of
    /// action. `Pause` and `OpenBreaker` both mean "do not spend a retry".
    pub fn on_failure(
        &mut self,
        scope: &SessionScope,
        class: ErrorClass,
        now: DateTime<Utc>,
    ) -> Action {
        match class {
            // A re-login fixes it; a retry does not.
            ErrorClass::Auth => Action::Park,
            ErrorClass::CapacityExhausted { resets_at } => {
                let until = clamp_pause(resets_at, now);
                self.set_pause(until);
                Action::Pause { until }
            }
            ErrorClass::ProviderInternal | ErrorClass::Unknown => {
                self.on_provider_failure(scope, now)
            }
        }
    }

    fn on_provider_failure(&mut self, scope: &SessionScope, now: DateTime<Utc>) -> Action {
        if let Some(breaker) = self.breakers.get_mut(scope) {
            if now - breaker.opened_at >= Duration::hours(BREAKER_MAX_OPEN_HOURS) {
                self.breakers.remove(scope);
                self.consecutive.remove(scope);
                return Action::Park;
            }
            breaker.next_probe = now + Duration::minutes(BREAKER_PROBE_MINUTES);
            breaker.probe_issued = false;
            breaker.consecutive = breaker.consecutive.saturating_add(1);
            return Action::OpenBreaker;
        }

        let count = self.consecutive.entry(scope.clone()).or_insert(0);
        *count = count.saturating_add(1);
        if *count < BREAKER_THRESHOLD {
            return Action::Retry;
        }
        let consecutive = *count;
        self.consecutive.remove(scope);
        self.breakers.insert(
            scope.clone(),
            Breaker {
                opened_at: now,
                next_probe: now + Duration::minutes(BREAKER_PROBE_MINUTES),
                probe_issued: false,
                consecutive,
            },
        );
        Action::OpenBreaker
    }

    /// Record a successful live turn for `scope`: the pause lifts, the scope's
    /// breaker closes, and its consecutive-failure count resets.
    ///
    /// Returns `(pause_lifted, breaker_closed)` so the caller can write the
    /// `agent_resumed` and `breaker_closed` ledger records.
    pub fn on_success(&mut self, scope: &SessionScope) -> (bool, bool) {
        let pause_lifted = self.pause.take().is_some();
        let breaker_closed = self.breakers.remove(scope).is_some();
        self.consecutive.remove(scope);
        (pause_lifted, breaker_closed)
    }

    /// Whether the agent may run a turn now, and if not, until when.
    ///
    /// Calling this hands out at most one probe per pause expiry: the first
    /// call after `until` returns [`PauseGate::Probe`], later calls return
    /// [`PauseGate::Held`] again until the probe's outcome resolves the pause.
    pub fn pause_gate(&mut self, now: DateTime<Utc>) -> PauseGate {
        let Some(pause) = self.pause.as_mut() else {
            return PauseGate::Open;
        };
        if now < pause.until {
            return PauseGate::Held { until: pause.until };
        }
        if pause.probe_issued {
            return PauseGate::Held { until: pause.until };
        }
        pause.probe_issued = true;
        PauseGate::Probe
    }

    /// Read the pause without handing out a probe.
    pub fn paused_until(&self) -> Option<DateTime<Utc>> {
        self.pause.as_ref().map(|p| p.until)
    }

    /// Whether `scope` may run a turn now, and if not, until when. Hands out at
    /// most one probe per probe interval, like [`pause_gate`](Self::pause_gate).
    pub fn breaker_gate(&mut self, scope: &SessionScope, now: DateTime<Utc>) -> BreakerGate {
        let Some(breaker) = self.breakers.get_mut(scope) else {
            return BreakerGate::Closed;
        };
        if now < breaker.next_probe {
            return BreakerGate::Held {
                next_probe: breaker.next_probe,
            };
        }
        if breaker.probe_issued {
            return BreakerGate::Held {
                next_probe: breaker.next_probe,
            };
        }
        breaker.probe_issued = true;
        BreakerGate::Probe
    }

    /// A probe on an open breaker failed: reschedule, or park once the breaker
    /// has been open for [`BREAKER_MAX_OPEN_HOURS`].
    pub fn on_breaker_probe_failure(
        &mut self,
        scope: &SessionScope,
        now: DateTime<Utc>,
    ) -> BreakerVerdict {
        let Some(breaker) = self.breakers.get_mut(scope) else {
            return BreakerVerdict::Reschedule {
                next_probe: now + Duration::minutes(BREAKER_PROBE_MINUTES),
            };
        };
        if now - breaker.opened_at >= Duration::hours(BREAKER_MAX_OPEN_HOURS) {
            self.breakers.remove(scope);
            return BreakerVerdict::Park;
        }
        breaker.next_probe = now + Duration::minutes(BREAKER_PROBE_MINUTES);
        breaker.probe_issued = false;
        BreakerVerdict::Reschedule {
            next_probe: breaker.next_probe,
        }
    }

    /// How long a breaker for `scope` has been open, if one is.
    pub fn breaker_opened_at(&self, scope: &SessionScope) -> Option<DateTime<Utc>> {
        self.breakers.get(scope).map(|b| b.opened_at)
    }

    /// Consecutive failures recorded against the scope's open breaker.
    pub fn breaker_consecutive(&self, scope: &SessionScope) -> Option<u32> {
        self.breakers.get(scope).map(|b| b.consecutive)
    }

    /// Whether `channel_id` still needs the pause notice.
    ///
    /// At most one notice per pause per channel. A re-pause re-notifies only
    /// when the reset time moved by more than [`PAUSE_RENOTIFY_MINUTES`].
    pub fn claim_pause_notice(&mut self, channel_id: Uuid) -> bool {
        match self.pause.as_mut() {
            Some(pause) => pause.notified_channels.insert(channel_id),
            None => false,
        }
    }

    /// Operator control frame `resume_now`: leave Paused and BreakerOpen and
    /// probe immediately. Returns `true` when something was actually lifted.
    pub fn resume_now(&mut self) -> bool {
        let had_pause = self.pause.take().is_some();
        let had_breakers = !self.breakers.is_empty();
        self.breakers.clear();
        self.consecutive.clear();
        had_pause || had_breakers
    }

    /// Operator control frame `keep_paused { until }`: extend a pause. The
    /// pause is only ever extended, never shortened, and never past the
    /// six-hour cap measured from `now`.
    pub fn keep_paused(&mut self, until: DateTime<Utc>, now: DateTime<Utc>) -> DateTime<Utc> {
        let capped = clamp_pause(Some(until), now);
        let target = match self.pause.as_ref() {
            Some(existing) if existing.until > capped => existing.until,
            _ => capped,
        };
        self.set_pause(target);
        target
    }

    fn set_pause(&mut self, until: DateTime<Utc>) {
        match self.pause.as_mut() {
            Some(pause) => {
                let moved = (until - pause.notified_until).num_minutes().abs();
                pause.until = until;
                pause.probe_issued = false;
                if moved > PAUSE_RENOTIFY_MINUTES {
                    pause.notified_channels.clear();
                    pause.notified_until = until;
                }
            }
            None => {
                self.pause = Some(Pause {
                    until,
                    notified_until: until,
                    notified_channels: HashSet::new(),
                    probe_issued: false,
                });
            }
        }
    }
}

/// The pause instant for a parsed (or absent) reset time: the default 30
/// minutes when the provider named none, never more than six hours out, and
/// never in the past.
pub fn clamp_pause(resets_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> DateTime<Utc> {
    let cap = now + Duration::hours(MAX_PAUSE_HOURS);
    let Some(resets_at) = resets_at else {
        return now + Duration::minutes(DEFAULT_PAUSE_MINUTES);
    };
    if resets_at > cap {
        tracing::warn!(
            resets_at = %resets_at,
            cap = %cap,
            "provider reset time is more than {MAX_PAUSE_HOURS}h out — clamping the pause"
        );
        return cap;
    }
    if resets_at <= now {
        return now + Duration::minutes(DEFAULT_PAUSE_MINUTES);
    }
    resets_at
}
