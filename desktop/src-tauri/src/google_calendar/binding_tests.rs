//! Tests for the stored envelope, its compare-and-set predicates and the
//! revocation journal (T11 decisions 2, 5, 6 and 9).
//!
//! Every commit here runs through the shipped `KeychainEnvelopes::commit` over
//! an in-memory blob that honours the same compare-and-set contract as
//! `SecretStore::mutate_checked`; only the keychain I/O is replaced. So each
//! predicate has a test that fails when the predicate is deleted, the
//! refusal-versus-store error mapping is under test, and the concurrent case is
//! a barrier-held race over the shipped transition code.

use std::collections::HashMap;
use std::sync::{Arc, Barrier, Mutex};

use super::binding::{
    deserialize_envelope, envelope_key, serialize_envelope, BlobMutation, CalendarEnvelope, Change,
    CheckedBlob, CommitContext, CommitError, EnvelopeStore, JournalStep, KeychainEnvelopes,
    TransitionError, ENVELOPE_KEY_PREFIX,
};
use super::redact::Redacted;
use super::revocation::{
    AttemptOutcome, PendingRevocation, RevocationState, MAX_PENDING_REVOCATIONS,
    REVOCATION_DEADLINE_MS,
};
use super::testing::{binding, context, CLIENT_ID, PUBKEY, SENTINEL};

// ── The envelope and its predicates (T11 decisions 2, 5 and 6) ────────────

/// A blob that honours [`CheckedBlob`]'s contract exactly as
/// `SecretStore::mutate_checked` does: one lock held across the whole
/// read-modify-write, the candidate built in a separate allocation, and nothing
/// written when the predicate refuses.
///
/// Only the keychain I/O is replaced. Every test below still commits through
/// the shipped [`KeychainEnvelopes::commit`], so the predicate wiring, the
/// refusal capture and the refusal-versus-store error mapping are production
/// code under test.
#[derive(Default)]
struct MemoryBlob {
    blob: Mutex<HashMap<String, String>>,
    /// When set, the durable write fails after the predicate accepted — the
    /// backend-error path, which is not a refusal.
    write_fails: bool,
}

impl CheckedBlob for MemoryBlob {
    fn mutate_checked(&self, f: BlobMutation<'_>) -> Result<(), String> {
        let mut guard = self.blob.lock().expect("the store lock is not poisoned");
        let mut next = guard.clone();
        f(&mut next)?;
        if self.write_fails {
            return Err("keyring write: the backend is unavailable".to_string());
        }
        *guard = next;
        Ok(())
    }

    fn load_all_readonly(&self) -> Result<Option<HashMap<String, String>>, String> {
        Ok(Some(
            self.blob
                .lock()
                .expect("the store lock is not poisoned")
                .clone(),
        ))
    }
}

/// The shipped envelope store over an in-memory blob.
#[derive(Default)]
struct MemoryEnvelopes {
    backing: MemoryBlob,
}

impl MemoryEnvelopes {
    fn failing_writes() -> Self {
        Self {
            backing: MemoryBlob {
                write_fails: true,
                ..MemoryBlob::default()
            },
        }
    }

    /// Put a raw value in the blob, to exercise an unreadable stored envelope.
    fn seed_raw(&self, key: &str, raw: &str) {
        self.backing
            .blob
            .lock()
            .expect("the store lock is not poisoned")
            .insert(key.to_string(), raw.to_string());
    }
}

impl EnvelopeStore for MemoryEnvelopes {
    fn commit(
        &self,
        key: &str,
        change: Change,
        context: &CommitContext,
    ) -> Result<(), CommitError> {
        KeychainEnvelopes::new(&self.backing).commit(key, change, context)
    }

    fn read(&self, key: &str) -> Result<CalendarEnvelope, CommitError> {
        KeychainEnvelopes::new(&self.backing).read(key)
    }
}

#[test]
fn google_calendar_a_backend_failure_is_never_reported_as_a_refused_predicate() {
    // The two error arms of the shipped commit: a refusal names the transition
    // the predicate rejected, a store error carries the backend's message and
    // names no transition. Swapping them would tell a caller a keychain outage
    // was a lost compare-and-set, and the caller would stop retrying.
    let store = MemoryEnvelopes::failing_writes();
    let key = envelope_key(PUBKEY);
    let error = store
        .commit(&key, Change::Connect(Box::new(binding(1))), &context(0))
        .expect_err("a failing durable write is surfaced");
    match error {
        CommitError::Store(detail) => assert!(
            detail.contains("backend is unavailable"),
            "the backend's own message must survive: {detail}"
        ),
        other => panic!("a backend failure is not a refusal, got {other:?}"),
    }
    assert!(
        store
            .read(&key)
            .expect("the envelope reads")
            .active_binding
            .is_none(),
        "a failed write leaves nothing durable"
    );
}

#[test]
fn google_calendar_an_unreadable_stored_envelope_refuses_the_commit() {
    // An envelope this build cannot read is never replaced with the default:
    // that would drop a live journal entry. It is a store error, not a refusal.
    let store = MemoryEnvelopes::default();
    let key = envelope_key(PUBKEY);
    store.seed_raw(
        &key,
        "{\"version\":999,\"active_binding\":null,\"pending\":[]}",
    );
    let error = store
        .commit(&key, Change::Connect(Box::new(binding(1))), &context(0))
        .expect_err("an unreadable envelope is not silently replaced");
    assert!(
        matches!(error, CommitError::Store(_)),
        "an unreadable envelope is a store error, got {error:?}"
    );
    assert!(
        store.read(&key).is_err(),
        "the unreadable value is still there, not overwritten"
    );
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

#[test]
fn google_calendar_no_agent_capability_resolves_the_calendar_credential() {
    // T11 decision 9: an agent has no authority over the calendar credential.
    // The string check above is necessary and not sufficient — this drives the
    // shipped `McpSecretLookup::resolve`, the one seam an agent's tools read
    // secrets through, over a blob that really holds the envelope, with a
    // capability that really resolves that agent's own secrets.
    use buzz_secret_store_pkg::testing::MemoryBlobSource;
    use buzz_secret_store_pkg::{AgentCapability, McpSecretLookup, McpSecretRef};

    let key = envelope_key(PUBKEY);
    let store = MemoryEnvelopes::default();
    store
        .commit(&key, Change::Connect(Box::new(binding(1))), &context(0))
        .expect("the binding commits");
    let envelope_blob = store
        .backing
        .blob
        .lock()
        .expect("the store lock is not poisoned")
        .clone();
    assert!(
        envelope_blob
            .get(&key)
            .is_some_and(|raw| raw.contains(SENTINEL)),
        "the fixture must really hold the credential, or this test proves nothing"
    );

    let source = MemoryBlobSource::default();
    for (record, value) in &envelope_blob {
        source.insert(record, value);
    }
    let capability = AgentCapability::mint("agent-one", 1, [7u8; 16]).expect("a valid agent id");
    source.insert(
        &buzz_secret_store_pkg::binding_key(&capability),
        capability.binding_value(),
    );
    // The agent's own secret resolves, so the lookup below is refusing the
    // calendar record specifically and not failing for an unrelated reason.
    let own = McpSecretRef::parse("mcp:api-token").expect("a valid reference");
    source.insert(
        &buzz_secret_store_pkg::storage_key(&capability, &own),
        "the agent's own secret",
    );
    let lookup = McpSecretLookup::new(source);
    assert!(
        lookup.resolve(&capability, &own).is_ok(),
        "the control must resolve, or a refusal below means nothing"
    );

    // Everything an agent could write in a reference position to try to name
    // the calendar record.
    for attempt in [
        key.clone(),
        format!("mcp:{key}"),
        format!("mcp:{ENVELOPE_KEY_PREFIX}"),
        format!("mcp:../{key}"),
        format!("mcp:{PUBKEY}"),
        "mcp:identity".to_string(),
    ] {
        match McpSecretRef::parse(&attempt) {
            Err(_) => {}
            Ok(reference) => {
                let resolved = lookup.resolve(&capability, &reference);
                assert!(
                    resolved.is_err(),
                    "`{attempt}` resolved to a value; no agent reference may reach the calendar \
                     credential"
                );
            }
        }
    }
}
