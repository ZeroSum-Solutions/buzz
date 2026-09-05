//! The stored envelope of T11 decisions 2, 5 and 6, and the compare-and-set
//! predicate every write to it passes.
//!
//! One envelope per identity holds the active binding and the pending
//! revocations. Every token, journal and binding write commits inside one
//! [`SecretStore`](crate::secret_store::SecretStore) mutation under a
//! *transition-specific* predicate, evaluated on the freshly-read durable state
//! inside the store's lock:
//!
//! | Transition | Predicate |
//! |---|---|
//! | Connect | no active binding, the identity pubkey unchanged, no retryable journal entry for this `(client_id, sub)`, room in the journal |
//! | Refresh | the active binding is exactly the captured generation |
//! | Disconnect | the active binding is exactly the captured generation, and the journal has room |
//! | Journal progress | the journal holds that generation at the revision the caller captured |
//! | Abandon | the journal holds that generation |
//!
//! One universal predicate could not do this: a cleared binding still has a
//! journal to advance. Check-then-write loses the race the purge exists for,
//! so the check happens inside the mutation, never before it.
//!
//! Nothing here derives `Serialize`: [`Binding`] holds
//! [`Redacted`](super::redact::Redacted) values, which have no serializer, so
//! a token cannot reach a UI payload by accident. Persistence goes through the
//! private wire form below, which is reachable only from this module.

use std::collections::{BTreeMap, HashMap};

use super::redact::Redacted;
use super::revocation::{PendingRevocation, RevocationState, MAX_PENDING_REVOCATIONS};

/// Prefix of the secret-store key holding one identity's envelope.
///
/// Deliberately outside the `mcp:` namespace: the launcher sidecar resolves
/// only `mcp:<agent>:<generation>:<id>` keys, so no agent reference can name
/// this record (T11 decision 9).
pub const ENVELOPE_KEY_PREFIX: &str = "google_calendar:";

/// The secret-store key holding `identity_pubkey_hex`'s envelope.
pub fn envelope_key(identity_pubkey_hex: &str) -> String {
    format!("{ENVELOPE_KEY_PREFIX}{identity_pubkey_hex}")
}

/// One Google account bound to one Buzz identity.
#[derive(Debug, Clone)]
pub struct Binding {
    /// The Buzz identity pubkey, hex.
    pub identity_pubkey_hex: String,
    /// The OAuth client this grant belongs to.
    pub client_id: String,
    /// The Google account's stable `sub`.
    pub sub: String,
    /// The account's email, for display.
    pub email: String,
    /// The scopes Google granted.
    pub scopes: Vec<String>,
    /// CSPRNG generation. Every cache row and journal entry is addressed by it.
    pub generation: u64,
    /// The current access token.
    pub access_token: Redacted<String>,
    /// The refresh token.
    pub refresh_token: Redacted<String>,
    /// When the access token expires, in milliseconds since the Unix epoch.
    pub access_expires_at_ms: i64,
    /// Absolute staleness bound: the last successful authorization refresh plus
    /// 24 hours (T11 decision 6). Never extended by a failure or a restart.
    pub stale_after_ms: i64,
}

/// The whole stored state for one identity.
#[derive(Debug, Clone, Default)]
pub struct CalendarEnvelope {
    /// The active binding, when one exists.
    pub active_binding: Option<Binding>,
    /// Pending revocations, keyed by the generation they revoke.
    pub pending: BTreeMap<u64, PendingRevocation>,
}

impl CalendarEnvelope {
    /// Whether a retryable journal entry blocks Connect for this account.
    pub fn connect_blocked_by(&self, client_id: &str, sub: &str) -> Option<u64> {
        self.pending
            .values()
            .find(|entry| entry.blocks_connect(client_id, sub))
            .map(|entry| entry.generation)
    }
}

/// Why a transition was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionError {
    /// A binding is already active, so Connect would overwrite it.
    AlreadyBound,
    /// The active binding is not the generation the caller captured.
    GenerationChanged,
    /// The identity pubkey the flow started under is no longer active.
    IdentityChanged,
    /// A retryable journal entry for this account must be settled or
    /// abandoned first.
    RevocationPending(u64),
    /// The journal is full; the user must clear an entry.
    JournalFull,
    /// The journal has no entry for that generation.
    NoSuchJournalEntry(u64),
    /// The journal entry moved since the caller read it.
    JournalRevisionChanged {
        /// The revision the caller captured.
        expected: u64,
        /// The revision found.
        found: u64,
    },
}

impl std::fmt::Display for TransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransitionError::AlreadyBound => write!(f, "a Google account is already connected"),
            TransitionError::GenerationChanged => {
                write!(f, "the connection changed while this write was in flight")
            }
            TransitionError::IdentityChanged => {
                write!(f, "the Buzz identity changed during the connect flow")
            }
            TransitionError::RevocationPending(generation) => write!(
                f,
                "a disconnect for generation {generation} is still being revoked"
            ),
            TransitionError::JournalFull => write!(
                f,
                "the revocation journal is full; clear an entry in settings first"
            ),
            TransitionError::NoSuchJournalEntry(generation) => {
                write!(f, "no revocation entry for generation {generation}")
            }
            TransitionError::JournalRevisionChanged { expected, found } => write!(
                f,
                "the revocation entry moved from revision {expected} to {found}"
            ),
        }
    }
}

impl std::error::Error for TransitionError {}

/// What a commit reads from the world at the moment the predicate runs.
#[derive(Debug, Clone)]
pub struct CommitContext {
    /// The identity pubkey that is active *now*, re-read inside the mutation.
    /// `import_identity` can replace it mid-flow, so the value captured when
    /// the flow started is not authoritative here.
    pub current_identity_pubkey_hex: Option<String>,
    /// Now, in milliseconds since the Unix epoch.
    pub now_ms: i64,
}

/// How a journal entry advances.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalStep {
    /// The cached rows for this generation are gone.
    PurgeConfirmed,
    /// One call to the revocation endpoint answered with this status, or with
    /// nothing at all.
    RevocationAttempt(Option<u16>),
}

/// One state change, with its own predicate.
///
/// `Clone` so a commit can hand the transition to a store seam that takes a
/// re-callable closure; the clone never leaves the commit.
#[derive(Debug, Clone)]
pub enum Change {
    /// Write the first binding, or the one after a disconnect.
    Connect(Box<Binding>),
    /// Replace the tokens on the active binding.
    Refresh {
        /// The generation the caller captured.
        generation: u64,
        /// The new access token.
        access_token: Redacted<String>,
        /// Its expiry, in milliseconds since the Unix epoch.
        access_expires_at_ms: i64,
        /// The new absolute staleness bound.
        stale_after_ms: i64,
        /// A rotated refresh token, when Google returned one.
        refresh_token: Option<Redacted<String>>,
    },
    /// Clear the binding and open its journal entry, in one commit.
    Disconnect {
        /// The generation the caller captured.
        generation: u64,
    },
    /// Advance one journal entry.
    JournalProgress {
        /// The generation the entry revokes.
        generation: u64,
        /// The revision the caller captured.
        expected_revision: u64,
        /// What happened.
        step: JournalStep,
    },
    /// Abandon one journal entry on the user's instruction.
    AbandonRevocation {
        /// The generation the entry revokes.
        generation: u64,
    },
}

impl Change {
    /// Evaluate this change's predicate against `envelope` and apply it.
    ///
    /// The two are one operation on purpose: a caller cannot check and then
    /// write, which is the race the purge exists for.
    ///
    /// # Errors
    /// Returns [`TransitionError`] when the predicate does not hold. The
    /// envelope is left untouched in that case.
    pub fn apply(
        self,
        envelope: &mut CalendarEnvelope,
        context: &CommitContext,
    ) -> Result<(), TransitionError> {
        match self {
            Change::Connect(binding) => {
                if envelope.active_binding.is_some() {
                    return Err(TransitionError::AlreadyBound);
                }
                if context.current_identity_pubkey_hex.as_deref()
                    != Some(binding.identity_pubkey_hex.as_str())
                {
                    return Err(TransitionError::IdentityChanged);
                }
                if let Some(generation) =
                    envelope.connect_blocked_by(&binding.client_id, &binding.sub)
                {
                    return Err(TransitionError::RevocationPending(generation));
                }
                if envelope.pending.len() >= MAX_PENDING_REVOCATIONS {
                    return Err(TransitionError::JournalFull);
                }
                envelope.active_binding = Some(*binding);
                Ok(())
            }
            Change::Refresh {
                generation,
                access_token,
                access_expires_at_ms,
                stale_after_ms,
                refresh_token,
            } => {
                let binding = envelope
                    .active_binding
                    .as_mut()
                    .filter(|binding| binding.generation == generation)
                    .ok_or(TransitionError::GenerationChanged)?;
                binding.access_token = access_token;
                binding.access_expires_at_ms = access_expires_at_ms;
                binding.stale_after_ms = stale_after_ms;
                if let Some(refresh_token) = refresh_token {
                    binding.refresh_token = refresh_token;
                }
                Ok(())
            }
            Change::Disconnect { generation } => {
                let binding = envelope
                    .active_binding
                    .as_ref()
                    .filter(|binding| binding.generation == generation)
                    .ok_or(TransitionError::GenerationChanged)?;
                if envelope.pending.len() >= MAX_PENDING_REVOCATIONS
                    && !envelope.pending.contains_key(&generation)
                {
                    return Err(TransitionError::JournalFull);
                }
                let entry = PendingRevocation::open(
                    generation,
                    binding.client_id.clone(),
                    binding.sub.clone(),
                    binding.refresh_token.clone(),
                    context.now_ms,
                );
                // One mutation writes both halves: the cleared binding and the
                // journal entry that owns the retry. There is no prefix of this
                // commit in which the grant is unreachable and unrevoked.
                envelope.pending.insert(generation, entry);
                envelope.active_binding = None;
                Ok(())
            }
            Change::JournalProgress {
                generation,
                expected_revision,
                step,
            } => {
                let entry = envelope
                    .pending
                    .get_mut(&generation)
                    .ok_or(TransitionError::NoSuchJournalEntry(generation))?;
                if entry.revision != expected_revision {
                    return Err(TransitionError::JournalRevisionChanged {
                        expected: expected_revision,
                        found: entry.revision,
                    });
                }
                match step {
                    JournalStep::PurgeConfirmed => entry.record_purge(),
                    JournalStep::RevocationAttempt(status) => {
                        entry.record_attempt(status, context.now_ms);
                    }
                }
                if entry.is_clearable() {
                    envelope.pending.remove(&generation);
                }
                Ok(())
            }
            Change::AbandonRevocation { generation } => {
                let entry = envelope
                    .pending
                    .get_mut(&generation)
                    .ok_or(TransitionError::NoSuchJournalEntry(generation))?;
                entry.abandon();
                Ok(())
            }
        }
    }
}

/// Why a commit failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitError {
    /// The predicate did not hold.
    Refused(TransitionError),
    /// The stored envelope could not be read or written.
    Store(String),
}

impl std::fmt::Display for CommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommitError::Refused(error) => write!(f, "{error}"),
            CommitError::Store(detail) => write!(f, "calendar secret store: {detail}"),
        }
    }
}

impl std::error::Error for CommitError {}

/// A durable home for envelopes that serializes read-modify-write.
pub trait EnvelopeStore {
    /// Read the envelope at `key`, apply `change` to it under the store's own
    /// lock, and write the result back only if the change succeeded.
    ///
    /// # Errors
    /// Returns [`CommitError::Refused`] when the predicate did not hold and
    /// [`CommitError::Store`] when the backing store failed.
    fn commit(&self, key: &str, change: Change, context: &CommitContext)
        -> Result<(), CommitError>;

    /// Read the envelope at `key`, or the empty envelope when none is stored.
    ///
    /// # Errors
    /// Returns [`CommitError::Store`] when the backing store failed.
    fn read(&self, key: &str) -> Result<CalendarEnvelope, CommitError>;
}

// ── Persistence ───────────────────────────────────────────────────────────

/// The serialized form. Private to this module: it is the one place a token is
/// a plain `String`, and nothing outside can build or read it.
mod wire {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize)]
    pub(super) struct Envelope {
        pub version: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub active_binding: Option<Binding>,
        #[serde(default)]
        pub pending: Vec<Pending>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub(super) struct Binding {
        pub identity_pubkey_hex: String,
        pub client_id: String,
        pub sub: String,
        pub email: String,
        pub scopes: Vec<String>,
        pub generation: u64,
        pub access_token: String,
        pub refresh_token: String,
        pub access_expires_at_ms: i64,
        pub stale_after_ms: i64,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub(super) struct Pending {
        pub generation: u64,
        pub client_id: String,
        pub sub: String,
        pub refresh_token: String,
        pub purge_confirmed: bool,
        pub revocation_confirmed: bool,
        pub attempts: u32,
        pub next_attempt_at_ms: i64,
        pub deadline_ms: i64,
        pub state: String,
        pub revision: u64,
    }
}

/// Schema version of the stored envelope.
pub const ENVELOPE_VERSION: u32 = 1;

/// Serialize an envelope for storage.
///
/// # Errors
/// Returns the serializer's message when the envelope cannot be encoded.
pub fn serialize_envelope(envelope: &CalendarEnvelope) -> Result<String, String> {
    let wire = wire::Envelope {
        version: ENVELOPE_VERSION,
        active_binding: envelope
            .active_binding
            .as_ref()
            .map(|binding| wire::Binding {
                identity_pubkey_hex: binding.identity_pubkey_hex.clone(),
                client_id: binding.client_id.clone(),
                sub: binding.sub.clone(),
                email: binding.email.clone(),
                scopes: binding.scopes.clone(),
                generation: binding.generation,
                access_token: binding.access_token.expose().clone(),
                refresh_token: binding.refresh_token.expose().clone(),
                access_expires_at_ms: binding.access_expires_at_ms,
                stale_after_ms: binding.stale_after_ms,
            }),
        pending: envelope
            .pending
            .values()
            .map(|entry| wire::Pending {
                generation: entry.generation,
                client_id: entry.client_id.clone(),
                sub: entry.sub.clone(),
                refresh_token: entry.refresh_token.expose().clone(),
                purge_confirmed: entry.purge_confirmed,
                revocation_confirmed: entry.revocation_confirmed,
                attempts: entry.attempts,
                next_attempt_at_ms: entry.next_attempt_at_ms,
                deadline_ms: entry.deadline_ms,
                state: match entry.state {
                    RevocationState::Retryable => "retryable".to_string(),
                    RevocationState::Unconfirmed => "revocation_unconfirmed".to_string(),
                    RevocationState::Abandoned => "abandoned".to_string(),
                },
                revision: entry.revision,
            })
            .collect(),
    };
    serde_json::to_string(&wire).map_err(|error| error.to_string())
}

/// Read an envelope back.
///
/// # Errors
/// Returns a message when the stored value is not an envelope of a version
/// this build understands. A stored value is never silently replaced with the
/// empty envelope: that would drop a live journal entry.
pub fn deserialize_envelope(raw: &str) -> Result<CalendarEnvelope, String> {
    let wire: wire::Envelope = serde_json::from_str(raw).map_err(|error| error.to_string())?;
    if wire.version != ENVELOPE_VERSION {
        return Err(format!(
            "stored calendar envelope is version {}, expected {ENVELOPE_VERSION}",
            wire.version
        ));
    }
    let mut pending = BTreeMap::new();
    for entry in wire.pending {
        let state = match entry.state.as_str() {
            "retryable" => RevocationState::Retryable,
            "revocation_unconfirmed" => RevocationState::Unconfirmed,
            "abandoned" => RevocationState::Abandoned,
            other => return Err(format!("unknown revocation state `{other}`")),
        };
        pending.insert(
            entry.generation,
            PendingRevocation {
                generation: entry.generation,
                client_id: entry.client_id,
                sub: entry.sub,
                refresh_token: Redacted::new(entry.refresh_token),
                purge_confirmed: entry.purge_confirmed,
                revocation_confirmed: entry.revocation_confirmed,
                attempts: entry.attempts,
                next_attempt_at_ms: entry.next_attempt_at_ms,
                deadline_ms: entry.deadline_ms,
                state,
                revision: entry.revision,
            },
        );
    }
    Ok(CalendarEnvelope {
        active_binding: wire.active_binding.map(|binding| Binding {
            identity_pubkey_hex: binding.identity_pubkey_hex,
            client_id: binding.client_id,
            sub: binding.sub,
            email: binding.email,
            scopes: binding.scopes,
            generation: binding.generation,
            access_token: Redacted::new(binding.access_token),
            refresh_token: Redacted::new(binding.refresh_token),
            access_expires_at_ms: binding.access_expires_at_ms,
            stale_after_ms: binding.stale_after_ms,
        }),
        pending,
    })
}

/// One read-modify-write of the whole blob. Refusing means writing nothing.
pub type BlobMutation<'f> = &'f mut dyn FnMut(&mut HashMap<String, String>) -> Result<(), String>;

/// The store-wide compare-and-set primitive an envelope commit runs through.
///
/// A seam over
/// [`SecretStore::mutate_checked`](crate::secret_store::SecretStore::mutate_checked)
/// so the shipped [`KeychainEnvelopes::commit`] — the predicate wiring, the
/// refusal capture and the error mapping — is the code a test drives. The
/// implementation must hold one lock across the whole read-modify-write, build
/// the candidate separately from the current state, and write nothing at all
/// when `f` returns an error.
pub trait CheckedBlob {
    /// Read-modify-write the whole blob under one lock, refusing on `f`'s error.
    ///
    /// # Errors
    /// Returns `f`'s error when the mutation refuses, or the backend's error
    /// when the store is unavailable or the write fails.
    fn mutate_checked(&self, f: BlobMutation<'_>) -> Result<(), String>;

    /// Read the whole blob without migrating anything.
    ///
    /// # Errors
    /// Returns the backend's error when the store is unavailable.
    fn load_all_readonly(&self) -> Result<Option<HashMap<String, String>>, String>;
}

impl CheckedBlob for crate::secret_store::SecretStore {
    fn mutate_checked(&self, f: BlobMutation<'_>) -> Result<(), String> {
        crate::secret_store::SecretStore::mutate_checked(self, |map| f(map))
    }

    fn load_all_readonly(&self) -> Result<Option<HashMap<String, String>>, String> {
        crate::secret_store::SecretStore::load_all_readonly(self)
    }
}

/// The [`SecretStore`](crate::secret_store::SecretStore)-backed envelope store.
///
/// Every commit runs inside `SecretStore::mutate_checked`, which holds the
/// interprocess advisory lock and re-reads the durable blob before the
/// predicate sees it.
pub struct KeychainEnvelopes<'a, S: CheckedBlob + ?Sized = crate::secret_store::SecretStore> {
    store: &'a S,
}

impl<'a, S: CheckedBlob + ?Sized> KeychainEnvelopes<'a, S> {
    /// Wrap `store`.
    pub fn new(store: &'a S) -> Self {
        Self { store }
    }
}

impl<S: CheckedBlob + ?Sized> EnvelopeStore for KeychainEnvelopes<'_, S> {
    fn commit(
        &self,
        key: &str,
        change: Change,
        context: &CommitContext,
    ) -> Result<(), CommitError> {
        let mut refusal: Option<TransitionError> = None;
        let outcome = {
            let refusal = &mut refusal;
            let mut apply = move |map: &mut HashMap<String, String>| -> Result<(), String> {
                let mut envelope = match map.get(key) {
                    Some(raw) => deserialize_envelope(raw)?,
                    None => CalendarEnvelope::default(),
                };
                if let Err(error) = change.clone().apply(&mut envelope, context) {
                    *refusal = Some(error.clone());
                    return Err(error.to_string());
                }
                map.insert(key.to_string(), serialize_envelope(&envelope)?);
                Ok(())
            };
            self.store.mutate_checked(&mut apply)
        };
        match (outcome, refusal) {
            (Ok(()), _) => Ok(()),
            // A refusal is the predicate's verdict and names the transition; a
            // store error is the backend's and never does. Collapsing the two
            // would tell a caller that a keychain outage was a refused
            // compare-and-set, or the reverse.
            (Err(_), Some(refusal)) => Err(CommitError::Refused(refusal)),
            (Err(detail), None) => Err(CommitError::Store(detail)),
        }
    }

    fn read(&self, key: &str) -> Result<CalendarEnvelope, CommitError> {
        // `load_all_readonly` rather than `load`: the calendar record has no
        // legacy per-key form, so the migration path `load` would run for a
        // missing key is pure keychain work with nothing to find.
        let blob = self.store.load_all_readonly().map_err(CommitError::Store)?;
        match blob.as_ref().and_then(|blob| blob.get(key)) {
            None => Ok(CalendarEnvelope::default()),
            Some(raw) => deserialize_envelope(raw).map_err(CommitError::Store),
        }
    }
}
