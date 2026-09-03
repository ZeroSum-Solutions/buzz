pub use ifc_core::LabelError;
use nostr::PublicKey;
use serde::Serialize;
use sha2::{Digest, Sha256};
use url::Url;

use crate::hash::hash_field;

/// A Buzz principal represented by a validated, normalized Nostr public key.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Principal(pub(crate) String);

impl Principal {
    /// Parse and normalize a hexadecimal Nostr public key.
    pub fn from_hex(value: &str) -> Result<Self, PrincipalError> {
        let key = PublicKey::from_hex(value).map_err(|_| PrincipalError::InvalidPublicKey)?;
        Self::from_public_key(&key)
    }

    /// Validate and convert a Nostr public key.
    ///
    /// PublicKey can hold any 32-byte value, including values that are not
    /// valid x-only secp256k1 points. IFC identities must reject those values
    /// before they enter reader sets or domain keys.
    pub fn from_public_key(value: &PublicKey) -> Result<Self, PrincipalError> {
        value
            .xonly()
            .map_err(|_| PrincipalError::InvalidPublicKey)?;
        Ok(Self(value.to_hex().to_ascii_lowercase()))
    }

    /// Return the normalized hexadecimal public key.
    pub fn as_hex(&self) -> &str {
        &self.0
    }
}

/// A principal could not be constructed from the supplied key.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PrincipalError {
    /// The value is not a valid Nostr public key.
    #[error("invalid Nostr public key")]
    InvalidPublicKey,
}

/// A confidentiality universe derived from one canonical Buzz relay URL.
///
/// Public data in one community is not public in another, so labels from
/// different realms never flow to one another.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct RealmId(pub(crate) [u8; 32]);

impl RealmId {
    /// Derive a realm identifier from the canonical form of a Buzz relay URL.
    ///
    /// Scheme and host case, a default port, and a root trailing slash do not
    /// create distinct realms. Credentials, query parameters, and fragments
    /// are rejected because they are connection details, not community
    /// identity.
    pub fn from_relay_url(relay_url: &str) -> Result<Self, RealmError> {
        let parsed = Url::parse(relay_url).map_err(|_| RealmError::InvalidUrl)?;
        if !matches!(parsed.scheme(), "ws" | "wss") {
            return Err(RealmError::UnsupportedScheme);
        }
        if parsed.host().is_none() {
            return Err(RealmError::MissingHost);
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(RealmError::Credentials);
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            return Err(RealmError::QueryOrFragment);
        }

        let mut canonical = parsed.origin().ascii_serialization();
        let path = parsed.path().trim_end_matches('/');
        if !path.is_empty() {
            canonical.push_str(path);
        }
        Ok(Self(Sha256::digest(canonical.as_bytes()).into()))
    }

    /// Return a short identifier suitable for structured logs.
    pub fn fingerprint(&self) -> String {
        hex::encode(&self.0[..6])
    }

    pub(crate) fn stable_hash(&self, hasher: &mut Sha256) {
        hasher.update(self.0);
    }
}

/// A relay URL cannot safely identify a Buzz confidentiality realm.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RealmError {
    /// The value is not an absolute URL.
    #[error("invalid Buzz relay URL")]
    InvalidUrl,
    /// Buzz relay realms use WebSocket URLs.
    #[error("Buzz relay URL must use ws or wss")]
    UnsupportedScheme,
    /// A realm URL must identify a host.
    #[error("Buzz relay URL has no host")]
    MissingHost,
    /// Credentials are connection state and cannot participate in realm identity.
    #[error("Buzz relay URL must not contain credentials")]
    Credentials,
    /// Query parameters and fragments are not stable community identity.
    #[error("Buzz relay URL must not contain a query or fragment")]
    QueryOrFragment,
}

/// A reader-set confidentiality label within one Buzz community.
pub type ConfidentialityLabel = ifc_core::ConfidentialityLabel<RealmId, Principal>;

pub(crate) type ReaderSet = ifc_core::ReaderSet<Principal>;

pub(crate) fn stable_hash_label(label: &ConfidentialityLabel, hasher: &mut Sha256) {
    label.universe().stable_hash(hasher);
    stable_hash_readers(label.reader_set(), hasher);
}

pub(crate) fn stable_hash_readers(readers: &ReaderSet, hasher: &mut Sha256) {
    match readers {
        ReaderSet::Everyone => hash_field(hasher, b"everyone"),
        ReaderSet::Only(readers) => {
            hash_field(hasher, b"only");
            hash_field(hasher, &(readers.len() as u64).to_be_bytes());
            for reader in readers {
                hash_field(hasher, reader.0.as_bytes());
            }
        }
    }
}
