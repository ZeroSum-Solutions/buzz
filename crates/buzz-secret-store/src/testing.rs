//! An in-memory [`SecretBlobSource`](crate::lookup::SecretBlobSource).
//!
//! Available in normal builds (not `cfg(test)`) so integration tests in this
//! crate, and the desktop crate's tests, drive the same production lookup with
//! a store they control instead of touching the user's real keychain.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::blob::serialize_blob;
use crate::lookup::SecretBlobSource;

/// A blob source backed by an in-process map.
#[derive(Default)]
pub struct MemoryBlobSource {
    entries: Mutex<Option<HashMap<String, String>>>,
}

impl MemoryBlobSource {
    /// Insert (or replace) one record. Creates the blob if none exists yet.
    pub fn insert(&self, key: &str, value: &str) {
        let mut guard = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .get_or_insert_with(HashMap::new)
            .insert(key.to_string(), value.to_string());
    }

    /// Remove one record, returning whether it was present.
    pub fn remove(&self, key: &str) -> bool {
        let mut guard = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .as_mut()
            .map(|map| map.remove(key).is_some())
            .unwrap_or(false)
    }
}

impl SecretBlobSource for MemoryBlobSource {
    fn read_blob(&self) -> Result<Option<Vec<u8>>, String> {
        let guard = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            None => Ok(None),
            Some(map) => serialize_blob(map)
                .map(|json| Some(json.into_bytes()))
                .map_err(|e| e.to_string()),
        }
    }
}
