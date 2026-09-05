//! The reserved `mcp:` secret namespace.
//!
//! Every MCP secret lives under `mcp:` and nowhere else. The blob the desktop
//! keeps also holds the human nsec under `identity` and every agent nsec under
//! `agent:<pubkey>`; a reference that could name either of those would let an
//! untrusted server read a private key, so both are rejected here — at the one
//! place a reference is turned into a typed value — rather than at each caller.

use std::fmt;

/// Prefix every MCP secret reference and every MCP blob key carries.
pub const MCP_NAMESPACE_PREFIX: &str = "mcp:";

/// Blob key holding the human identity nsec. Never addressable from a reference.
pub const RESERVED_IDENTITY_KEY: &str = "identity";

/// Prefix of every per-agent nsec blob key. Never addressable from a reference.
pub const RESERVED_AGENT_KEY_PREFIX: &str = "agent:";

/// Longest accepted reference id, in bytes, excluding the `mcp:` prefix.
///
/// A reference is an operator-visible name that ends up inside a generated
/// config file and a keychain key, so it is capped at the DTO the same way
/// every other user-sourced string is.
pub const MAX_REFERENCE_ID_LEN: usize = 64;

/// Why a string is not a valid MCP secret reference.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReferenceError {
    /// The string does not start with `mcp:`.
    #[error("secret reference must start with `{MCP_NAMESPACE_PREFIX}`")]
    MissingNamespace,
    /// The string names a reserved blob key (`identity` or `agent:*`).
    #[error("secret reference names the reserved key `{0}`")]
    Reserved(String),
    /// The id after `mcp:` is empty.
    #[error("secret reference has an empty id")]
    Empty,
    /// The id after `mcp:` is longer than [`MAX_REFERENCE_ID_LEN`].
    #[error("secret reference id is {0} bytes, over the {MAX_REFERENCE_ID_LEN}-byte cap")]
    TooLong(usize),
    /// The id after `mcp:` uses a character outside `[a-z0-9_-]`.
    #[error("secret reference id may only use lowercase letters, digits, `_` and `-`")]
    InvalidCharacter,
}

/// A validated reference to one secret in the `mcp:` namespace.
///
/// Holding one is proof that the string carried the namespace, named no
/// reserved key, and is within the id cap. It is deliberately *not* an
/// authorization: resolving it still requires a capability bound to the agent
/// that owns it (see [`crate::capability`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct McpSecretRef {
    id: String,
}

impl McpSecretRef {
    /// Parse `raw` (the full `mcp:<id>` spelling) into a validated reference.
    ///
    /// # Errors
    /// Returns [`ReferenceError`] when the namespace is missing, the id names a
    /// reserved key, is empty, is over [`MAX_REFERENCE_ID_LEN`], or uses a
    /// character outside `[a-z0-9_-]`.
    pub fn parse(raw: &str) -> Result<Self, ReferenceError> {
        // Reserved names are checked on the *whole* input, before the prefix
        // strip, so neither `identity` nor `agent:x` can be smuggled in.
        if raw == RESERVED_IDENTITY_KEY || raw.starts_with(RESERVED_AGENT_KEY_PREFIX) {
            return Err(ReferenceError::Reserved(raw.to_string()));
        }
        let id = raw
            .strip_prefix(MCP_NAMESPACE_PREFIX)
            .ok_or(ReferenceError::MissingNamespace)?;
        if id.is_empty() {
            return Err(ReferenceError::Empty);
        }
        if id.len() > MAX_REFERENCE_ID_LEN {
            return Err(ReferenceError::TooLong(id.len()));
        }
        if id == RESERVED_IDENTITY_KEY || id.starts_with(RESERVED_AGENT_KEY_PREFIX) {
            return Err(ReferenceError::Reserved(id.to_string()));
        }
        if !id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        {
            return Err(ReferenceError::InvalidCharacter);
        }
        Ok(Self { id: id.to_string() })
    }

    /// The id after the `mcp:` prefix.
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl fmt::Display for McpSecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{MCP_NAMESPACE_PREFIX}{}", self.id)
    }
}

/// Whether `raw` looks like an MCP secret reference at all.
///
/// Used by config generation to tell a reference-valued `env` entry from a
/// literal one. It is a shape test, not a validation: callers still parse.
pub fn looks_like_reference(raw: &str) -> bool {
    raw.starts_with(MCP_NAMESPACE_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_reference_namespace_is_closed() {
        // The whole point of the namespace: nothing outside `mcp:` parses, and
        // the two reserved key shapes never parse even with the prefix on.
        assert_eq!(
            McpSecretRef::parse("identity"),
            Err(ReferenceError::Reserved("identity".into()))
        );
        assert_eq!(
            McpSecretRef::parse("agent:npub1abc"),
            Err(ReferenceError::Reserved("agent:npub1abc".into()))
        );
        assert_eq!(
            McpSecretRef::parse("mcp:identity"),
            Err(ReferenceError::Reserved("identity".into()))
        );
        assert_eq!(
            McpSecretRef::parse("mcp:agent:npub1abc"),
            Err(ReferenceError::Reserved("agent:npub1abc".into()))
        );
        assert_eq!(
            McpSecretRef::parse("github-token"),
            Err(ReferenceError::MissingNamespace)
        );
        assert_eq!(McpSecretRef::parse("mcp:"), Err(ReferenceError::Empty));
    }

    #[test]
    fn reference_id_is_capped_and_charset_checked() {
        let long = format!("mcp:{}", "a".repeat(MAX_REFERENCE_ID_LEN + 1));
        assert_eq!(
            McpSecretRef::parse(&long),
            Err(ReferenceError::TooLong(MAX_REFERENCE_ID_LEN + 1))
        );
        let at_cap = format!("mcp:{}", "a".repeat(MAX_REFERENCE_ID_LEN));
        assert!(McpSecretRef::parse(&at_cap).is_ok());
        for bad in ["mcp:Upper", "mcp:has space", "mcp:has/slash", "mcp:dot.dot"] {
            assert_eq!(
                McpSecretRef::parse(bad),
                Err(ReferenceError::InvalidCharacter),
                "{bad} must be refused"
            );
        }
    }

    #[test]
    fn display_round_trips_the_namespace() {
        let parsed = McpSecretRef::parse("mcp:github-token").expect("valid");
        assert_eq!(parsed.id(), "github-token");
        assert_eq!(parsed.to_string(), "mcp:github-token");
        assert!(looks_like_reference(&parsed.to_string()));
        assert!(!looks_like_reference("plain-value"));
    }
}
