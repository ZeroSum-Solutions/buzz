//! Tests for the stored envelope, its compare-and-set predicates and the
//! revocation journal (T11 decisions 2, 5, 6 and 9).
//!
//! Each predicate has a test that fails when the predicate is deleted, and the
//! concurrent case is a barrier-held race over the shipped transition code.

use std::collections::HashMap;
use std::sync::{Arc, Barrier, Mutex};

use super::binding::{
    deserialize_envelope, envelope_key, serialize_envelope, CalendarEnvelope, Change,
    CommitContext, CommitError, EnvelopeStore, JournalStep, TransitionError, ENVELOPE_KEY_PREFIX,
};
use super::redact::Redacted;
use super::revocation::{
    AttemptOutcome, PendingRevocation, RevocationState, MAX_PENDING_REVOCATIONS,
    REVOCATION_DEADLINE_MS,
};
use super::testing::{binding, context, CLIENT_ID, PUBKEY, SENTINEL};

// ── The envelope and its predicates (T11 decisions 2, 5 and 6) ────────────

/// An envelope store that serializes commits exactly as the keychain one does:
/// one lock, a fresh read inside it, and no write when the predicate refuses.
#[derive(Default)]
struct MemoryEnvelopes {
    blob: Mutex<HashMap<String, String>>,
}

impl EnvelopeStore for MemoryEnvelopes {
    fn commit(
        &self,
        key: &str,
        change: Change,
        context: &CommitContext,
    ) -> Result<(), CommitError> {
        let mut blob = self.blob.lock().expect("the store lock is not poisoned");
        let mut envelope = match blob.get(key) {
            Some(raw) => deserialize_envelope(raw).map_err(CommitError::Store)?,
            None => CalendarEnvelope::default(),
        };
        change
            .apply(&mut envelope, context)
            .map_err(CommitError::Refused)?;
        blob.insert(
            key.to_string(),
            serialize_envelope(&envelope).map_err(CommitError::Store)?,
        );
        Ok(())
    }

    fn read(&self, key: &str) -> Result<CalendarEnvelope, CommitError> {
        let blob = self.blob.lock().expect("the store lock is not poisoned");
        match blob.get(key) {
            Some(raw) => deserialize_envelope(raw).map_err(CommitError::Store),
            None => Ok(CalendarEnvelope::default()),
        }
    }
}

#[test]
fn google_calendar_connect_refuses_after_an_identity_swap() {
    let store = MemoryEnvelopes::default();
    let key = envelope_key(PUBKEY);
    let swapped = CommitContext {
        current_identity_pubkey_hex: Some("ffff".to_string()),
        now_ms: 0,
    };
    let error = store
        .commit(&key, Change::Connect(Box::new(binding(1))), &swapped)
        .expect_err("a swapped identity refuses the binding");
    assert_eq!(
        error,
        CommitError::Refused(TransitionError::IdentityChanged)
    );
    assert!(
        store
            .read(&key)
            .expect("the envelope reads")
            .active_binding
            .is_none(),
        "a refused commit writes nothing"
    );
    store
        .commit(&key, Change::Connect(Box::new(binding(1))), &context(0))
        .expect("the unchanged identity is accepted");
}

#[test]
fn google_calendar_connect_refuses_a_second_binding() {
    let store = MemoryEnvelopes::default();
    let key = envelope_key(PUBKEY);
    store
        .commit(&key, Change::Connect(Box::new(binding(1))), &context(0))
        .expect("the first binding is written");
    assert_eq!(
        store.commit(&key, Change::Connect(Box::new(binding(2))), &context(0)),
        Err(CommitError::Refused(TransitionError::AlreadyBound))
    );
}

#[test]
fn google_calendar_refresh_requires_the_captured_generation() {
    let store = MemoryEnvelopes::default();
    let key = envelope_key(PUBKEY);
    store
        .commit(&key, Change::Connect(Box::new(binding(7))), &context(0))
        .expect("the binding is written");
    let stale = Change::Refresh {
        generation: 6,
        access_token: Redacted::new("new".to_string()),
        access_expires_at_ms: 1,
        stale_after_ms: 2,
        refresh_token: None,
    };
    assert_eq!(
        store.commit(&key, stale, &context(0)),
        Err(CommitError::Refused(TransitionError::GenerationChanged))
    );
    let fresh = Change::Refresh {
        generation: 7,
        access_token: Redacted::new("new".to_string()),
        access_expires_at_ms: 10,
        stale_after_ms: 20,
        refresh_token: None,
    };
    store
        .commit(&key, fresh, &context(0))
        .expect("the captured generation commits");
    let envelope = store.read(&key).expect("the envelope reads");
    let active = envelope.active_binding.expect("a binding is active");
    assert_eq!(active.access_token.expose(), "new");
    assert_eq!(active.stale_after_ms, 20);
    assert_eq!(
        active.refresh_token.expose(),
        &format!("{SENTINEL}-refresh"),
        "an absent rotated token leaves the stored one alone"
    );
}

#[test]
fn google_calendar_disconnect_clears_the_binding_and_opens_the_journal_in_one_commit() {
    let store = MemoryEnvelopes::default();
    let key = envelope_key(PUBKEY);
    store
        .commit(&key, Change::Connect(Box::new(binding(3))), &context(0))
        .expect("the binding is written");
    assert_eq!(
        store.commit(&key, Change::Disconnect { generation: 2 }, &context(0)),
        Err(CommitError::Refused(TransitionError::GenerationChanged))
    );
    store
        .commit(&key, Change::Disconnect { generation: 3 }, &context(1_000))
        .expect("the captured generation disconnects");
    let envelope = store.read(&key).expect("the envelope reads");
    assert!(envelope.active_binding.is_none());
    let entry = envelope.pending.get(&3).expect("the journal entry exists");
    assert!(!entry.purge_confirmed && !entry.revocation_confirmed);
    assert_eq!(
        entry.refresh_token.expose(),
        &format!("{SENTINEL}-refresh"),
        "the entry owns the token the revocation needs"
    );
    assert_eq!(entry.deadline_ms, 1_000 + REVOCATION_DEADLINE_MS);
}

#[test]
fn google_calendar_journal_progress_requires_the_captured_revision() {
    let store = MemoryEnvelopes::default();
    let key = envelope_key(PUBKEY);
    store
        .commit(&key, Change::Connect(Box::new(binding(3))), &context(0))
        .expect("the binding is written");
    store
        .commit(&key, Change::Disconnect { generation: 3 }, &context(0))
        .expect("the disconnect commits");
    let stale = Change::JournalProgress {
        generation: 3,
        expected_revision: 9,
        step: JournalStep::PurgeConfirmed,
    };
    assert!(matches!(
        store.commit(&key, stale, &context(0)),
        Err(CommitError::Refused(
            TransitionError::JournalRevisionChanged { .. }
        ))
    ));
    store
        .commit(
            &key,
            Change::JournalProgress {
                generation: 3,
                expected_revision: 0,
                step: JournalStep::PurgeConfirmed,
            },
            &context(0),
        )
        .expect("the captured revision commits");
    assert!(
        store
            .read(&key)
            .expect("the envelope reads")
            .pending
            .get(&3)
            .expect("the entry is still open")
            .purge_confirmed
    );
}

#[test]
fn google_calendar_entry_clears_only_when_purge_and_revocation_both_confirm() {
    let store = MemoryEnvelopes::default();
    let key = envelope_key(PUBKEY);
    store
        .commit(&key, Change::Connect(Box::new(binding(4))), &context(0))
        .expect("the binding is written");
    store
        .commit(&key, Change::Disconnect { generation: 4 }, &context(0))
        .expect("the disconnect commits");
    store
        .commit(
            &key,
            Change::JournalProgress {
                generation: 4,
                expected_revision: 0,
                step: JournalStep::RevocationAttempt(Some(200)),
            },
            &context(0),
        )
        .expect("the revocation is recorded");
    let envelope = store.read(&key).expect("the envelope reads");
    let entry = envelope
        .pending
        .get(&4)
        .expect("the entry stays until purge");
    assert!(entry.revocation_confirmed && !entry.purge_confirmed);
    store
        .commit(
            &key,
            Change::JournalProgress {
                generation: 4,
                expected_revision: entry.revision,
                step: JournalStep::PurgeConfirmed,
            },
            &context(0),
        )
        .expect("the purge is recorded");
    assert!(
        store
            .read(&key)
            .expect("the envelope reads")
            .pending
            .is_empty(),
        "both predicates held, so the entry cleared"
    );
}

#[test]
fn google_calendar_connect_is_refused_while_a_revocation_is_retryable() {
    let store = MemoryEnvelopes::default();
    let key = envelope_key(PUBKEY);
    store
        .commit(&key, Change::Connect(Box::new(binding(5))), &context(0))
        .expect("the binding is written");
    store
        .commit(&key, Change::Disconnect { generation: 5 }, &context(0))
        .expect("the disconnect commits");
    assert_eq!(
        store.commit(&key, Change::Connect(Box::new(binding(6))), &context(0)),
        Err(CommitError::Refused(TransitionError::RevocationPending(5))),
        "a late 200 for the old generation would kill the new grant"
    );
}

#[test]
fn google_calendar_abandoned_entry_never_retries_and_leaves_the_new_binding() {
    let store = MemoryEnvelopes::default();
    let key = envelope_key(PUBKEY);
    store
        .commit(&key, Change::Connect(Box::new(binding(5))), &context(0))
        .expect("the binding is written");
    store
        .commit(&key, Change::Disconnect { generation: 5 }, &context(0))
        .expect("the disconnect commits");
    store
        .commit(
            &key,
            Change::AbandonRevocation { generation: 5 },
            &context(0),
        )
        .expect("the user abandons the entry");
    store
        .commit(&key, Change::Connect(Box::new(binding(6))), &context(0))
        .expect("a new binding is issued after the abandon");

    let envelope = store.read(&key).expect("the envelope reads");
    let mut abandoned = envelope.pending.get(&5).expect("the entry stays").clone();
    assert_eq!(abandoned.state, RevocationState::Abandoned);
    assert_eq!(
        abandoned.record_attempt(Some(200), 10),
        AttemptOutcome::NotRetryable,
        "an abandoned entry never retries, so a delayed 200 cannot arrive"
    );
    assert_eq!(
        envelope
            .active_binding
            .expect("the new binding is active")
            .generation,
        6
    );
}

#[test]
fn google_calendar_journal_is_capped_and_connect_names_the_full_state() {
    let mut envelope = CalendarEnvelope::default();
    for generation in 0..MAX_PENDING_REVOCATIONS as u64 {
        let mut entry = PendingRevocation::open(
            generation,
            CLIENT_ID,
            format!("sub-{generation}"),
            Redacted::new("token".to_string()),
            0,
        );
        entry.state = RevocationState::Unconfirmed;
        envelope.pending.insert(generation, entry);
    }
    assert_eq!(
        Change::Connect(Box::new(binding(99))).apply(&mut envelope, &context(0)),
        Err(TransitionError::JournalFull),
        "a ninth entry is refused rather than retiring one"
    );
    assert_eq!(envelope.pending.len(), MAX_PENDING_REVOCATIONS);
}

#[test]
fn google_calendar_only_http_200_confirms_a_revocation_and_the_ceiling_is_terminal() {
    let mut entry =
        PendingRevocation::open(1, CLIENT_ID, "sub", Redacted::new("token".to_string()), 0);
    entry.record_purge();
    for status in [None, Some(500), Some(429), Some(204)] {
        assert_eq!(
            entry.record_attempt(status, 1_000),
            AttemptOutcome::Recorded
        );
        assert!(
            !entry.revocation_confirmed,
            "{status:?} must not confirm a revocation"
        );
        assert!(entry.next_attempt_at_ms > 1_000, "a failure backs off");
    }
    assert_eq!(
        entry.record_attempt(Some(500), REVOCATION_DEADLINE_MS + 1),
        AttemptOutcome::Recorded
    );
    assert_eq!(
        entry.state,
        RevocationState::Unconfirmed,
        "the seven-day ceiling converges on a terminal state"
    );
    assert_eq!(
        entry.record_attempt(Some(200), REVOCATION_DEADLINE_MS + 2),
        AttemptOutcome::NotRetryable
    );

    let mut fresh =
        PendingRevocation::open(2, CLIENT_ID, "sub", Redacted::new("token".to_string()), 0);
    fresh.record_purge();
    assert_eq!(
        fresh.record_attempt(Some(200), 10),
        AttemptOutcome::Clearable
    );
}

#[test]
fn google_calendar_concurrent_commits_serialize_and_the_loser_is_refused() {
    let store = Arc::new(MemoryEnvelopes::default());
    let key = envelope_key(PUBKEY);
    store
        .commit(&key, Change::Connect(Box::new(binding(11))), &context(0))
        .expect("the binding is written");

    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            let key = key.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.commit(&key, Change::Disconnect { generation: 11 }, &context(0))
            })
        })
        .collect();
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("the committing thread finishes"))
        .collect();
    assert_eq!(
        results.iter().filter(|result| result.is_ok()).count(),
        1,
        "exactly one disconnect commits"
    );
    assert!(results.iter().any(|result| {
        matches!(
            result,
            Err(CommitError::Refused(TransitionError::GenerationChanged))
        )
    }));
    let envelope = store.read(&key).expect("the envelope reads");
    assert!(envelope.active_binding.is_none());
    assert_eq!(envelope.pending.len(), 1, "no torn or duplicated state");
}

#[test]
fn google_calendar_envelope_round_trips_and_refuses_an_unknown_version() {
    let mut envelope = CalendarEnvelope {
        active_binding: Some(binding(12)),
        pending: Default::default(),
    };
    envelope.pending.insert(
        11,
        PendingRevocation::open(11, CLIENT_ID, "sub", Redacted::new("t".to_string()), 5),
    );
    let raw = serialize_envelope(&envelope).expect("the envelope serializes");
    let restored = deserialize_envelope(&raw).expect("the envelope restores");
    assert_eq!(
        restored
            .active_binding
            .expect("the binding restores")
            .access_token
            .expose(),
        SENTINEL
    );
    assert_eq!(restored.pending.len(), 1);

    let future = raw.replace("\"version\":1", "\"version\":2");
    assert!(
        deserialize_envelope(&future).is_err(),
        "an unreadable envelope is an error, never a silent empty one"
    );
}

#[test]
fn google_calendar_envelope_key_is_outside_the_agent_namespace() {
    let key = envelope_key(PUBKEY);
    assert!(key.starts_with(ENVELOPE_KEY_PREFIX));
    assert!(
        buzz_secret_store_pkg::McpSecretRef::parse(&key).is_err(),
        "no agent reference can name the calendar credential"
    );
    assert!(!buzz_secret_store_pkg::looks_like_reference(&key));
}
