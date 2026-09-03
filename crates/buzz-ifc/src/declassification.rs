use nostr::secp256k1::schnorr::Signature;
use nostr::secp256k1::Message;
use nostr::{PublicKey, SECP256K1};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::domain::PublicationTarget;
use crate::hash::hash_field;
use crate::label::Principal;

/// Stable nonce identifying one declassification authorization.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct GrantId([u8; 32]);

impl GrantId {
    /// Construct a grant identifier from an unpredictable broker-issued nonce.
    pub fn from_bytes(value: [u8; 32]) -> Self {
        Self(value)
    }

    /// Return the nonce bytes stored by durable replay protection.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Versioned, canonical payload approved by a bot owner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeclassificationGrantPayload {
    approver: Principal,
    grant_id: GrantId,
    operation: String,
    source_domain_id: String,
    destination: PublicationTarget,
    content_digest: [u8; 32],
    expires_at: u64,
}

impl DeclassificationGrantPayload {
    /// Construct every field covered by the owner's signature.
    pub fn new(
        approver: Principal,
        grant_id: GrantId,
        operation: impl Into<String>,
        source_domain_id: impl Into<String>,
        destination: PublicationTarget,
        content_digest: [u8; 32],
        expires_at: u64,
    ) -> Self {
        Self {
            approver,
            grant_id,
            operation: operation.into(),
            source_domain_id: source_domain_id.into(),
            destination,
            content_digest,
            expires_at,
        }
    }

    /// Return the principal whose signature is required.
    pub fn approver(&self) -> &Principal {
        &self.approver
    }

    /// Return the durable replay-protection identifier.
    pub fn grant_id(&self) -> &GrantId {
        &self.grant_id
    }

    /// Return the exact publication operation approved by the owner.
    pub fn operation(&self) -> &str {
        &self.operation
    }

    /// Return the exact execution domain from which the content came.
    pub fn source_domain_id(&self) -> &str {
        &self.source_domain_id
    }

    /// Return the exact destination and membership epoch approved for release.
    pub fn destination(&self) -> &PublicationTarget {
        &self.destination
    }

    /// Return the digest of the exact content approved for release.
    pub fn content_digest(&self) -> &[u8; 32] {
        &self.content_digest
    }

    /// Return the Unix timestamp at which this approval stops being valid.
    pub fn expires_at(&self) -> u64 {
        self.expires_at
    }

    /// Return the BIP-340 message digest covering every grant field.
    ///
    /// Owners sign this digest directly. The domain separator versions the
    /// canonical encoding, and every variable-length field is length-prefixed.
    pub fn signing_digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(b"buzz-ifc-declassification-v2");
        hash_field(&mut hasher, self.approver.as_hex().as_bytes());
        hash_field(&mut hasher, self.grant_id.as_bytes());
        hash_field(&mut hasher, self.operation.as_bytes());
        hash_field(&mut hasher, self.source_domain_id.as_bytes());
        self.destination.stable_hash(&mut hasher);
        hash_field(&mut hasher, &self.content_digest);
        hash_field(&mut hasher, &self.expires_at.to_be_bytes());
        hasher.finalize().into()
    }

    pub(crate) fn matches(
        &self,
        operation: &str,
        source_domain_id: &str,
        destination: &PublicationTarget,
        content_digest: &[u8; 32],
    ) -> bool {
        self.operation == operation
            && self.source_domain_id == source_domain_id
            && &self.destination == destination
            && &self.content_digest == content_digest
    }
}

/// One deliberate, owner-approved release to a broader audience.
///
/// The signed payload binds the approver, an unpredictable nonce, the exact
/// publication operation, the complete source domain, the destination and its
/// membership epoch, the content digest, and an expiry. Verifying the signature
/// inside this crate prevents an adapter from authenticating an envelope while
/// trusting different decoded fields. [`crate::IfcSession::publish`] also
/// requires the approver to be the source domain's owner and rejects a grant
/// that differs from the requested publication in any bound field.
///
/// Durable replay consumption happens at publication commit, immediately
/// before the broker signs or submits the authorized bytes. The approval is
/// therefore exact, expiring, and single-use; it does not grant the worker a
/// general capability to declassify other content or future output.
/// This implements the explicit release described under declassification in
/// [Appendix G of the design
/// paper](../../../docs/practical-information-flow-for-buzz-agents.md#appendix-g-security-labels-as-a-lattice).
pub struct DeclassificationGrant {
    payload: DeclassificationGrantPayload,
    signature: [u8; 64],
}

impl DeclassificationGrant {
    /// Return the signed canonical payload.
    pub fn payload(&self) -> &DeclassificationGrantPayload {
        &self.payload
    }

    /// Construct a grant from its decoded signed payload.
    pub fn new(payload: DeclassificationGrantPayload, signature: [u8; 64]) -> Self {
        Self { payload, signature }
    }

    /// Verify expiration and the approver's BIP-340 signature over canonical
    /// bytes. [`crate::IfcSession::publish`] separately requires the approver
    /// to be the owner of the source execution domain.
    pub fn verify(self, now: u64) -> Result<VerifiedDeclassificationGrant, GrantError> {
        if now >= self.payload.expires_at() {
            return Err(GrantError::Expired);
        }

        let signature =
            Signature::from_slice(&self.signature).map_err(|_| GrantError::InvalidSignature)?;
        let public_key = PublicKey::from_hex(self.payload.approver().as_hex())
            .map_err(|_| GrantError::InvalidSignature)?;
        let xonly = public_key
            .xonly()
            .map_err(|_| GrantError::InvalidSignature)?;
        let message = Message::from_digest(self.payload.signing_digest());
        SECP256K1
            .verify_schnorr(&signature, &message, &xonly)
            .map_err(|_| GrantError::InvalidSignature)?;

        Ok(VerifiedDeclassificationGrant {
            payload: self.payload,
        })
    }
}

/// A declassification grant whose payload signature and expiry were checked.
pub struct VerifiedDeclassificationGrant {
    payload: DeclassificationGrantPayload,
}

impl VerifiedDeclassificationGrant {
    /// Return the authenticated canonical payload.
    pub fn payload(&self) -> &DeclassificationGrantPayload {
        &self.payload
    }

    pub(crate) fn matches(
        &self,
        operation: &str,
        source_domain_id: &str,
        destination: &PublicationTarget,
        content_digest: &[u8; 32],
    ) -> bool {
        self.payload
            .matches(operation, source_domain_id, destination, content_digest)
    }
}

/// Durable, atomic replay protection used when committing declassification.
pub trait GrantReplayStore {
    /// Atomically record a signed grant digest if it has never been consumed.
    ///
    /// The digest binds the nonce, approver, source, operation, destination,
    /// content, and expiry. Return `true` only for the first durable
    /// consumption. Implementations may use `expires_at` to garbage-collect
    /// records after the grant can no longer pass commit-time expiry
    /// validation.
    fn consume_if_unused(&mut self, grant_digest: &[u8; 32], expires_at: u64) -> bool;
}

/// A declassification grant failed authentication.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum GrantError {
    /// The signed grant has reached its expiration time.
    #[error("declassification grant has expired")]
    Expired,
    /// The BIP-340 signature does not cover the canonical grant payload.
    #[error("declassification grant signature is invalid")]
    InvalidSignature,
}
