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

/// Maximum number of scopes tracked for consecutive provider failures before
/// pruning.
pub const MAX_CONSECUTIVE_SCOPES: usize = 1_000;

/// Maximum number of scopes with an open breaker tracked at once, mirroring
/// [`MAX_CONSECUTIVE_SCOPES`]. Without this an unbounded number of
/// distinct scopes (one-off channels/threads, each opening a breaker and then
/// going silent) grow the map forever — nothing else prunes it once a scope
/// stops sending failures (T16 delta 1, finding 7).
pub const MAX_OPEN_BREAKERS: usize = 1_000;

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
    /// The scope of the batch actually dispatched as the outstanding pause
    /// probe, if any got that far. Only that scope's turn completion may
    /// release this probe's lease — an unrelated scope finishing a turn
    /// (e.g. an already in-flight breaker probe) must not free it early and
    /// let a second probe batch dispatch while the first is still running.
    probe_scope: Option<SessionScope>,
}

#[derive(Debug, Clone)]
struct Breaker {
    opened_at: DateTime<Utc>,
    next_probe: DateTime<Utc>,
    probe_issued: bool,
    /// Consecutive failures that opened it, for the ledger record.
    consecutive: u32,
    /// Whether the failure notice has been successfully posted.
    notified: bool,
}

/// Per-agent reliability state: one pause for the whole agent (a capacity
/// limit belongs to the account) and one breaker per scope.
#[derive(Debug, Default)]
pub struct ReliabilityState {
    pause: Option<Pause>,
    breakers: HashMap<SessionScope, Breaker>,
    consecutive: HashMap<SessionScope, u32>,
    generation: u64,
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
            ErrorClass::Auth => {
                self.consecutive.remove(scope);
                Action::Park
            }
            ErrorClass::CapacityExhausted { resets_at } => {
                self.consecutive.remove(scope);
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
                self.generation = self.generation.wrapping_add(1);
                return Action::Park;
            }
            breaker.next_probe = now + Duration::minutes(BREAKER_PROBE_MINUTES);
            breaker.probe_issued = false;
            breaker.consecutive = breaker.consecutive.saturating_add(1);
            self.generation = self.generation.wrapping_add(1);
            return Action::OpenBreaker;
        }

        if self.consecutive.len() >= MAX_CONSECUTIVE_SCOPES && !self.consecutive.contains_key(scope)
        {
            if let Some(oldest_scope) = self.consecutive.keys().next().cloned() {
                self.consecutive.remove(&oldest_scope);
            }
        }
        let count = self.consecutive.entry(scope.clone()).or_insert(0);
        *count = count.saturating_add(1);
        if *count < BREAKER_THRESHOLD {
            return Action::Retry;
        }
        let consecutive = *count;
        self.consecutive.remove(scope);
        if self.breakers.len() >= MAX_OPEN_BREAKERS && !self.breakers.contains_key(scope) {
            // Evict the longest-open breaker to admit this one rather than
            // growing without bound. This is a bounded-memory safety valve,
            // not a substitute for `sweep_breakers` actually expiring stale
            // entries — normal operation should never reach this cap.
            if let Some(oldest_scope) = self
                .breakers
                .iter()
                .min_by_key(|(_, b)| b.opened_at)
                .map(|(s, _)| s.clone())
            {
                self.breakers.remove(&oldest_scope);
            }
        }
        self.breakers.insert(
            scope.clone(),
            Breaker {
                opened_at: now,
                next_probe: now + Duration::minutes(BREAKER_PROBE_MINUTES),
                probe_issued: false,
                consecutive,
                notified: false,
            },
        );
        self.generation = self.generation.wrapping_add(1);
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
        if pause_lifted || breaker_closed {
            self.generation = self.generation.wrapping_add(1);
        }
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
        self.generation = self.generation.wrapping_add(1);
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
        self.generation = self.generation.wrapping_add(1);
        BreakerGate::Probe
    }

    /// Release an unconsumed pause probe permit so a subsequent dispatch may probe.
    pub fn release_pause_probe(&mut self) {
        if let Some(pause) = self.pause.as_mut() {
            if pause.probe_issued {
                pause.probe_issued = false;
                pause.probe_scope = None;
                self.generation = self.generation.wrapping_add(1);
            }
        }
    }

    /// Record which scope's batch was actually dispatched as the outstanding
    /// pause probe. Called once dispatch has genuinely claimed a worker for
    /// it — not merely selected it — so [`release_pause_probe_for`] can later
    /// tell "this turn was the probe" from "this is an unrelated scope's turn
    /// completing while the probe is still in flight".
    pub fn set_pause_probe_scope(&mut self, scope: SessionScope) {
        if let Some(pause) = self.pause.as_mut() {
            pause.probe_scope = Some(scope);
        }
    }

    /// Release the pause probe lease, but only if `scope` is the exact scope
    /// that was dispatched as the probe.
    ///
    /// Safety net for every terminal outcome of a dispatched batch (success,
    /// retry, park, panic) — not just the paths that already know they held
    /// a probe. A stray call for an unrelated scope (e.g. a breaker probe for
    /// a different scope completing while a pause probe is still in flight)
    /// is a no-op, so calling this unconditionally at every completion point
    /// is safe (T16 delta 1, finding 9 / prior #5).
    pub fn release_pause_probe_for(&mut self, scope: &SessionScope) {
        if let Some(pause) = self.pause.as_mut() {
            if pause.probe_issued && pause.probe_scope.as_ref() == Some(scope) {
                pause.probe_issued = false;
                pause.probe_scope = None;
                self.generation = self.generation.wrapping_add(1);
            }
        }
    }

    /// A pause probe was selected but dispatch never actually ran it (no
    /// worker claimed it, or the owner was busy) — reschedule the deadline
    /// forward rather than reverting to "eligible right now".
    ///
    /// Reverting to eligible-now would make the very next `pause_gate` call
    /// hand out another probe immediately, and if nothing is ever available
    /// to dispatch (the queue is genuinely empty because the batch that
    /// triggered the pause is sitting in the park file, not the live queue)
    /// that repeats forever with the deadline pinned in the past — a busy
    /// spin that burns CPU indefinitely. Advancing the deadline the same way
    /// an open breaker already does between probes closes it (T16 delta 1,
    /// finding 2).
    /// A no-op if the probe was already released by another path (a held
    /// batch, a busy session owner, pool exhaustion) — those call
    /// [`release_pause_probe`](Self::release_pause_probe) inline and must
    /// stay immediately eligible again, not pushed 10 minutes out. This only
    /// actually reschedules when `probe_issued` is *still* true, meaning
    /// dispatch never found anything to even attempt.
    pub fn reschedule_pause_probe(&mut self, now: DateTime<Utc>) {
        if let Some(pause) = self.pause.as_mut() {
            if pause.probe_issued {
                pause.until = now + Duration::minutes(BREAKER_PROBE_MINUTES);
                pause.probe_issued = false;
                pause.probe_scope = None;
                self.generation = self.generation.wrapping_add(1);
            }
        }
    }

    /// Expire breakers that have been open for the full
    /// [`BREAKER_MAX_OPEN_HOURS`] window, and reschedule any breaker whose
    /// probe deadline is due but was never actually issued this cycle
    /// (because the live queue had nothing queued for that scope, so
    /// `breaker_gate` never ran for it).
    ///
    /// Call this once per probe-timer wake, AFTER giving `dispatch_pending`
    /// its chance to run — a breaker whose probe genuinely got issued this
    /// cycle already has `probe_issued == true` by then and is left alone.
    /// Without this sweep, a scope that opens a breaker and then sends
    /// nothing else ever again keeps its breaker (and the per-scope
    /// `consecutive` residue) forever, and its stale due-in-the-past deadline
    /// re-triggers the timer on every loop iteration (T16 delta 1, finding 7,
    /// and the breaker half of finding 2).
    ///
    /// Returns the scopes whose breaker expired (closed via timeout, not a
    /// successful probe) so the caller can write the ledger record.
    pub fn sweep_breakers(&mut self, now: DateTime<Utc>) -> Vec<SessionScope> {
        let mut expired = Vec::new();
        let mut changed = false;
        self.breakers.retain(|scope, breaker| {
            if now - breaker.opened_at >= Duration::hours(BREAKER_MAX_OPEN_HOURS) {
                expired.push(scope.clone());
                changed = true;
                false
            } else if now >= breaker.next_probe && !breaker.probe_issued {
                breaker.next_probe = now + Duration::minutes(BREAKER_PROBE_MINUTES);
                changed = true;
                true
            } else {
                true
            }
        });
        for scope in &expired {
            self.consecutive.remove(scope);
        }
        if changed {
            self.generation = self.generation.wrapping_add(1);
        }
        expired
    }

    /// Release an unconsumed breaker probe permit so a subsequent dispatch may probe.
    pub fn release_breaker_probe(&mut self, scope: &SessionScope) {
        if let Some(breaker) = self.breakers.get_mut(scope) {
            if breaker.probe_issued {
                breaker.probe_issued = false;
                self.generation = self.generation.wrapping_add(1);
            }
        }
    }

    /// Release any unconsumed probe permits for pause and scope breaker.
    pub fn release_probe(&mut self, scope: Option<&SessionScope>) {
        self.release_pause_probe();
        if let Some(scope) = scope {
            self.release_breaker_probe(scope);
        }
    }

    /// Reissue probe permit alias for `release_probe`.
    pub fn reissue_probe(&mut self, scope: Option<&SessionScope>) {
        self.release_probe(scope);
    }

    /// Monotonically increasing generation bumped on every pause or breaker state change.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The earliest deadline among an active pause and all open breakers that
    /// have not yet issued a probe permit.
    pub fn earliest_probe_deadline(&self) -> Option<DateTime<Utc>> {
        let pause_until = match &self.pause {
            Some(p) if !p.probe_issued => Some(p.until),
            _ => None,
        };
        let breaker_until = self
            .breakers
            .values()
            .filter(|b| !b.probe_issued)
            .map(|b| b.next_probe)
            .min();
        match (pause_until, breaker_until) {
            (Some(p), Some(b)) => Some(p.min(b)),
            (Some(p), None) => Some(p),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    /// Number of scopes currently tracked for consecutive failures.
    pub fn consecutive_len(&self) -> usize {
        self.consecutive.len()
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
            self.generation = self.generation.wrapping_add(1);
            return BreakerVerdict::Park;
        }
        breaker.next_probe = now + Duration::minutes(BREAKER_PROBE_MINUTES);
        breaker.probe_issued = false;
        self.generation = self.generation.wrapping_add(1);
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

    /// Whether this channel still needs a pause notice for the current pause.
    pub fn pause_needs_notice(&self, channel_id: Uuid) -> bool {
        self.pause
            .as_ref()
            .is_some_and(|p| !p.notified_channels.contains(&channel_id))
    }

    /// Mark the pause notice for `channel_id` as consumed after post succeeds.
    pub fn mark_pause_notice_consumed(&mut self, channel_id: Uuid) {
        if let Some(pause) = self.pause.as_mut() {
            pause.notified_channels.insert(channel_id);
        }
    }

    /// Whether this scope's open breaker has not yet successfully posted a notice.
    pub fn breaker_needs_notice(&self, scope: &SessionScope) -> bool {
        self.breakers.get(scope).is_some_and(|b| !b.notified)
    }

    /// Mark the open breaker's notice as consumed after post succeeds.
    pub fn mark_breaker_notice_consumed(&mut self, scope: &SessionScope) {
        if let Some(breaker) = self.breakers.get_mut(scope) {
            breaker.notified = true;
        }
    }

    /// Operator control frame `resume_now`: leave Paused and BreakerOpen and
    /// probe immediately. Returns `true` when something was actually lifted.
    pub fn resume_now(&mut self) -> bool {
        let had_pause = self.pause.take().is_some();
        let had_breakers = !self.breakers.is_empty();
        self.breakers.clear();
        self.consecutive.clear();
        if had_pause || had_breakers {
            self.generation = self.generation.wrapping_add(1);
        }
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
        self.generation = self.generation.wrapping_add(1);
        match self.pause.as_mut() {
            Some(pause) => {
                let moved = (until - pause.notified_until).num_minutes().abs();
                pause.until = until;
                pause.probe_issued = false;
                pause.probe_scope = None;
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
                    probe_scope: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reliability::ErrorClass;
    use uuid::Uuid;

    fn scope() -> SessionScope {
        SessionScope::Conversation {
            channel_id: Uuid::new_v4(),
        }
    }

    fn open_breaker(state: &mut ReliabilityState, scope: &SessionScope, now: DateTime<Utc>) {
        for _ in 0..BREAKER_THRESHOLD {
            state.on_failure(scope, ErrorClass::ProviderInternal, now);
        }
    }

    // T16 delta 1, finding 7: a breaker whose scope never sends anything
    // again must still eventually close — nothing but `sweep_breakers`
    // touches it once the scope goes silent.
    #[test]
    fn sweep_breakers_expires_a_breaker_whose_scope_went_silent() {
        let mut state = ReliabilityState::default();
        let s = scope();
        let now = Utc::now();
        open_breaker(&mut state, &s, now);
        assert!(
            state.breaker_opened_at(&s).is_some(),
            "breaker must be open"
        );

        // Well short of the 6h expiry: nothing changes.
        let before_expiry = now + Duration::hours(BREAKER_MAX_OPEN_HOURS - 1);
        let expired = state.sweep_breakers(before_expiry);
        assert!(expired.is_empty());
        assert!(state.breaker_opened_at(&s).is_some());

        // Past the 6h expiry with no new traffic on this scope at all: the
        // sweep — not a failure on the scope — must close it.
        let after_expiry = now + Duration::hours(BREAKER_MAX_OPEN_HOURS) + Duration::minutes(1);
        let expired = state.sweep_breakers(after_expiry);
        assert_eq!(expired, vec![s.clone()]);
        assert!(
            state.breaker_opened_at(&s).is_none(),
            "the breaker must actually be gone after the sweep expires it"
        );
    }

    // The busy-spin half of finding 2: a breaker whose probe deadline is due
    // but that no live dispatch ever touched this cycle (its scope had
    // nothing queued) must not keep reporting the same past deadline.
    #[test]
    fn sweep_breakers_reschedules_a_due_but_unconsumed_probe() {
        let mut state = ReliabilityState::default();
        let s = scope();
        let now = Utc::now();
        open_breaker(&mut state, &s, now);

        let due = now + Duration::minutes(BREAKER_PROBE_MINUTES) + Duration::seconds(1);
        assert_eq!(
            state.earliest_probe_deadline(),
            Some(now + Duration::minutes(BREAKER_PROBE_MINUTES))
        );

        let expired = state.sweep_breakers(due);
        assert!(expired.is_empty(), "6h expiry has not been reached");
        let next = state
            .earliest_probe_deadline()
            .expect("a rescheduled breaker still has a future deadline");
        assert!(
            next > due,
            "the deadline must move into the future, not stay stuck at `due`"
        );
    }

    #[test]
    fn breakers_map_is_bounded_across_many_distinct_scopes() {
        let mut state = ReliabilityState::default();
        let now = Utc::now();
        for _ in 0..(MAX_OPEN_BREAKERS + 50) {
            open_breaker(&mut state, &scope(), now);
        }
        // Each `scope()` call is a brand-new SessionScope, so without a
        // bound this would grow to MAX_OPEN_BREAKERS + 50 entries. `breakers`
        // is a private field, visible here as a descendant module of
        // `reliability::state` — there is no public len() accessor and
        // adding one only for this test isn't worth the API surface.
        assert!(
            state.breakers.len() <= MAX_OPEN_BREAKERS,
            "breakers map must not grow past MAX_OPEN_BREAKERS: got {}",
            state.breakers.len()
        );
    }

    // Finding 9 / prior #5: a pause probe that was actually dispatched (its
    // scope recorded) must only release for that exact scope — an unrelated
    // scope's turn completing (e.g. a breaker probe running concurrently)
    // must not free the pause lease early and let a second probe dispatch
    // while the first is still in flight.
    #[test]
    fn release_pause_probe_for_only_releases_the_dispatched_scope() {
        let mut state = ReliabilityState::default();
        let probe_scope = scope();
        let other_scope = scope();
        let now = Utc::now();
        state.set_pause(now); // already due at `now`
        assert_eq!(state.pause_gate(now), PauseGate::Probe);
        state.set_pause_probe_scope(probe_scope.clone());

        state.release_pause_probe_for(&other_scope);
        assert_eq!(
            state.pause_gate(now),
            PauseGate::Held {
                until: state.paused_until().unwrap()
            },
            "an unrelated scope must not release the probe lease"
        );

        state.release_pause_probe_for(&probe_scope);
        assert_eq!(
            state.pause_gate(now),
            PauseGate::Probe,
            "the exact dispatched scope must release the lease"
        );
    }

    // Finding 2: a pause probe selected but never actually dispatched must
    // reschedule forward, not revert to "eligible right now" — reverting
    // would make the very next call hand out another probe immediately,
    // spinning forever when nothing is ever available to dispatch.
    #[test]
    fn reschedule_pause_probe_moves_the_deadline_forward() {
        let mut state = ReliabilityState::default();
        let now = Utc::now();
        state.set_pause(now); // already due at `now`
        assert_eq!(state.pause_gate(now), PauseGate::Probe);

        state.reschedule_pause_probe(now);
        match state.pause_gate(now) {
            PauseGate::Held { until } => assert!(until > now),
            other => panic!("expected Held after reschedule, got {other:?}"),
        }
    }

    // A batch WAS found and held (busy owner / pool exhausted) — those
    // paths already call the bare, non-rescheduling `release_pause_probe`
    // inline. A caller that then also calls `reschedule_pause_probe` as a
    // blanket "nothing dispatched" cleanup must not clobber that decision
    // and push the deadline out — the probe must stay immediately eligible.
    #[test]
    fn reschedule_pause_probe_is_a_no_op_after_an_inline_release() {
        let mut state = ReliabilityState::default();
        let now = Utc::now();
        state.set_pause(now);
        assert_eq!(state.pause_gate(now), PauseGate::Probe);

        state.release_pause_probe(); // simulates the held/pool-exhausted path
        state.reschedule_pause_probe(now); // the blanket post-loop cleanup

        assert_eq!(
            state.pause_gate(now),
            PauseGate::Probe,
            "an already-released probe must remain immediately eligible, not \
             be pushed 10 minutes out by a later reschedule call"
        );
    }
}
