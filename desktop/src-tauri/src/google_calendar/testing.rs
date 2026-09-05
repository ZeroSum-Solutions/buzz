//! Fixtures shared by the calendar tests.

use super::binding::{Binding, CommitContext};
use super::oauth::SCOPES;
use super::redact::Redacted;

/// A value no production path may ever render.
pub(super) const SENTINEL: &str = "SENTINEL-CREDENTIAL-VALUE";
/// The Buzz identity the fixtures bind.
pub(super) const PUBKEY: &str = "0123456789abcdef";
/// The installed-app client id the fixtures use.
pub(super) const CLIENT_ID: &str = "buzz-desktop.apps.googleusercontent.com";

/// A binding at `generation`, holding sentinel credentials.
pub(super) fn binding(generation: u64) -> Binding {
    Binding {
        identity_pubkey_hex: PUBKEY.to_string(),
        client_id: CLIENT_ID.to_string(),
        sub: "google-sub-1".to_string(),
        email: "person@example.com".to_string(),
        scopes: SCOPES.iter().map(|scope| (*scope).to_string()).collect(),
        generation,
        access_token: Redacted::new(SENTINEL.to_string()),
        refresh_token: Redacted::new(format!("{SENTINEL}-refresh")),
        access_expires_at_ms: 3_600_000,
        stale_after_ms: 86_400_000,
    }
}

/// A commit context with the fixture identity active at `now_ms`.
pub(super) fn context(now_ms: i64) -> CommitContext {
    CommitContext {
        current_identity_pubkey_hex: Some(PUBKEY.to_string()),
        now_ms,
    }
}
