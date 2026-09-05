//! The revocation journal of T11 decision 5.
//!
//! Disconnect is one mutation: it clears the binding and writes a journal
//! entry (refresh token, key, generation, purge predicate, deadline) in the
//! same commit. Only then does the purge run, and only then the call to
//! Google's revocation endpoint. An entry clears when — and only when —
//! `purge_confirmed` and `revocation_confirmed` both hold; only HTTP 200 sets
//! the second.
//!
//! Anything else backs off to a seven-day ceiling and then becomes the terminal
//! `revocation_unconfirmed` state settings names, so the journal converges
//! instead of retrying forever. Terminal and abandoned entries stay in the map
//! and count toward the cap; only the user clears them.

use super::redact::Redacted;

/// Largest number of journal entries held per identity (T11 decision 5).
pub const MAX_PENDING_REVOCATIONS: usize = 8;

/// First backoff interval after a failed revocation attempt.
pub const FIRST_BACKOFF_MS: i64 = 60 * 1000;

/// Longest interval between two revocation attempts.
pub const MAX_BACKOFF_MS: i64 = 6 * 60 * 60 * 1000;

/// How long an entry stays retryable before it becomes terminal.
pub const REVOCATION_DEADLINE_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// Where one journal entry stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevocationState {
    /// Still inside the seven-day ceiling; another attempt is due.
    Retryable,
    /// The ceiling passed with no HTTP 200. Settings names this state; only
    /// the user clears it.
    Unconfirmed,
    /// The user abandoned it. It never retries again.
    Abandoned,
}

/// What one attempt achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// Google confirmed the revocation and the purge is confirmed too: the
    /// caller clears the entry.
    Clearable,
    /// Progress was recorded; the entry stays.
    Recorded,
    /// The entry is no longer retryable, so nothing was attempted.
    NotRetryable,
}

/// One pending revocation.
#[derive(Debug, Clone)]
pub struct PendingRevocation {
    /// The generation this entry revokes.
    pub generation: u64,
    /// The OAuth client the grant belongs to.
    pub client_id: String,
    /// The Google account the grant belongs to.
    pub sub: String,
    /// The refresh token to present to the revocation endpoint.
    pub refresh_token: Redacted<String>,
    /// Whether the cached rows for this generation are gone.
    pub purge_confirmed: bool,
    /// Whether Google answered the revocation with HTTP 200.
    pub revocation_confirmed: bool,
    /// Attempts made so far.
    pub attempts: u32,
    /// When the next attempt is due, in milliseconds since the Unix epoch.
    pub next_attempt_at_ms: i64,
    /// When the entry stops being retryable.
    pub deadline_ms: i64,
    /// Where the entry stands.
    pub state: RevocationState,
    /// Bumped on every change. The journal-progress predicate (T11 decision 6)
    /// requires the revision it captured, so two writers cannot both advance
    /// the same entry.
    pub revision: u64,
}

impl PendingRevocation {
    /// Open an entry for `generation` at `now_ms`.
    pub fn open(
        generation: u64,
        client_id: impl Into<String>,
        sub: impl Into<String>,
        refresh_token: Redacted<String>,
        now_ms: i64,
    ) -> Self {
        Self {
            generation,
            client_id: client_id.into(),
            sub: sub.into(),
            refresh_token,
            purge_confirmed: false,
            revocation_confirmed: false,
            attempts: 0,
            next_attempt_at_ms: now_ms,
            deadline_ms: now_ms.saturating_add(REVOCATION_DEADLINE_MS),
            state: RevocationState::Retryable,
            revision: 0,
        }
    }

    /// Whether both predicates hold, so the entry may be cleared.
    pub fn is_clearable(&self) -> bool {
        self.purge_confirmed && self.revocation_confirmed
    }

    /// Whether this entry blocks a fresh Connect for its `(client_id, sub)`.
    ///
    /// Only a retryable entry does: revocation is Cloud-project-wide, so a late
    /// 200 for this generation would kill the grant a reconnect just issued.
    /// A terminal or abandoned entry never retries, so it cannot.
    pub fn blocks_connect(&self, client_id: &str, sub: &str) -> bool {
        self.state == RevocationState::Retryable && self.client_id == client_id && self.sub == sub
    }

    /// Record that the cached rows for this generation are gone.
    pub fn record_purge(&mut self) {
        if !self.purge_confirmed {
            self.purge_confirmed = true;
            self.revision = self.revision.saturating_add(1);
        }
    }

    /// Record the result of one call to the revocation endpoint.
    ///
    /// `status` is `None` when no response arrived. Only HTTP 200 sets
    /// `revocation_confirmed`; every other answer is a failure that backs off,
    /// and the seven-day ceiling turns into the terminal state rather than
    /// retrying forever.
    pub fn record_attempt(&mut self, status: Option<u16>, now_ms: i64) -> AttemptOutcome {
        if self.state != RevocationState::Retryable {
            return AttemptOutcome::NotRetryable;
        }
        self.attempts = self.attempts.saturating_add(1);
        self.revision = self.revision.saturating_add(1);
        if status == Some(200) {
            self.revocation_confirmed = true;
            return if self.is_clearable() {
                AttemptOutcome::Clearable
            } else {
                AttemptOutcome::Recorded
            };
        }
        if now_ms >= self.deadline_ms {
            self.state = RevocationState::Unconfirmed;
            return AttemptOutcome::Recorded;
        }
        self.next_attempt_at_ms = now_ms.saturating_add(backoff_ms(self.attempts));
        AttemptOutcome::Recorded
    }

    /// Abandon the entry on the user's explicit instruction.
    pub fn abandon(&mut self) {
        self.state = RevocationState::Abandoned;
        self.revision = self.revision.saturating_add(1);
    }
}

/// The interval before attempt number `attempts`, doubling to
/// [`MAX_BACKOFF_MS`].
pub fn backoff_ms(attempts: u32) -> i64 {
    let shift = attempts.saturating_sub(1).min(20);
    FIRST_BACKOFF_MS
        .saturating_mul(1i64 << shift)
        .min(MAX_BACKOFF_MS)
}
