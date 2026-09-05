//! A wrapper that keeps a credential out of every rendering path.
//!
//! T11 decision 2 requires access, refresh and ID tokens, authorization codes,
//! PKCE verifiers and callback URLs to be "wrapped in a redacting type" so a
//! log line, a command error, a panic message or a `Debug` dump cannot carry
//! the value. [`Redacted`] is that type.
//!
//! Two properties make it a guard rather than a convention:
//!
//! * `Debug` and `Display` print a fixed marker, so the value cannot reach a
//!   formatter by accident.
//! * There is no `Serialize` implementation, so a struct holding one cannot be
//!   serialized into a UI payload at all — the persistence path converts to an
//!   explicit wire form instead (see [`super::binding`]).

use std::fmt;

/// What every formatter prints in place of the value.
pub const REDACTED_MARKER: &str = "<redacted>";

/// A value that must never be printed, logged or serialized.
///
/// [`Redacted::expose`] is the only read path, so every use of the real value
/// is greppable.
#[derive(Clone, Default)]
pub struct Redacted<T = String> {
    inner: T,
}

impl<T> Redacted<T> {
    /// Wrap `inner` so it cannot reach a formatter.
    pub fn new(inner: T) -> Self {
        Self { inner }
    }

    /// Borrow the wrapped value. The only read path.
    pub fn expose(&self) -> &T {
        &self.inner
    }

    /// Take the wrapped value back out.
    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T> fmt::Debug for Redacted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED_MARKER)
    }
}

impl<T> fmt::Display for Redacted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED_MARKER)
    }
}

impl From<String> for Redacted<String> {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for Redacted<String> {
    fn from(value: &str) -> Self {
        Self::new(value.to_string())
    }
}

/// Compare two secrets without an early return.
///
/// The OAuth `state` check (T11 decision 1) compares a value an attacker
/// supplies against one we generated; a byte-by-byte comparison that stops at
/// the first difference leaks the shared prefix length. Length is not secret,
/// so an unequal length short-circuits.
pub fn constant_time_eq(left: &str, right: &str) -> bool {
    let (left, right) = (left.as_bytes(), right.as_bytes());
    if left.len() != right.len() {
        return false;
    }
    let mut difference: u8 = 0;
    for (a, b) in left.iter().zip(right.iter()) {
        difference |= a ^ b;
    }
    difference == 0
}
