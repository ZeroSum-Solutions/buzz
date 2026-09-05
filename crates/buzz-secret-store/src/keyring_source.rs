//! The OS keyring blob source.
//!
//! The desktop app owns the write side of this entry; this is the read side the
//! workspace binaries need, because `secret_store` is a private Tauri module a
//! sidecar cannot call. The service name and the blob key are the desktop's, so
//! both processes address one entry and one OS prompt.

use crate::blob::BLOB_KEY;
use crate::lookup::SecretBlobSource;

/// Reads the single blob keychain entry for one service name.
pub struct KeyringBlobSource {
    service: String,
}

impl KeyringBlobSource {
    /// A source over the keychain entry `(service, "secrets")`.
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }
}

/// Read the raw blob bytes for `service` from the OS keyring.
///
/// `Ok(None)` means the entry does not exist yet. Every other backend failure —
/// including "the keyring is unavailable this boot" — is an `Err`, never an
/// empty result: reporting an outage as "no secrets" would silently
/// unauthenticate every server that depends on one.
///
/// # Errors
/// A human-readable message naming the keyring failure.
pub fn read_blob_raw(service: &str) -> Result<Option<Vec<u8>>, String> {
    let entry =
        keyring::Entry::new(service, BLOB_KEY).map_err(|e| format!("keyring entry: {e}"))?;
    match entry.get_password() {
        Ok(value) => Ok(Some(value.into_bytes())),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!("keyring read: {e}")),
    }
}

impl SecretBlobSource for KeyringBlobSource {
    fn read_blob(&self) -> Result<Option<Vec<u8>>, String> {
        read_blob_raw(&self.service)
    }
}
