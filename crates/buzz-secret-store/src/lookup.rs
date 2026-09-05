//! The one typed MCP secret lookup.
//!
//! It takes a [`AgentCapability`] and a [`McpSecretRef`] and can address
//! nothing else: the agent id and the generation that form the blob key come
//! only from the capability, so a reference is a name inside one agent's
//! namespace and never a path out of it. There is no by-name entry point, and
//! no way to ask for `identity` or an `agent:*` key — [`McpSecretRef::parse`]
//! refuses both before a lookup is even constructible.
//!
//! An agent id and a generation are both guessable, so holding the right pair
//! is not authorization. Every resolve first reads the agent's binding record
//! ([`binding_key`]) and compares the capability's nonce against it in
//! constant time; only then is a storage key derived. A `BUZZ_MCP_CAPABILITY`
//! forged with another agent's id and 32 arbitrary hex characters therefore
//! resolves nothing, and fails with the same [`LookupError::NotBound`] a
//! missing secret gives.

use zeroize::Zeroizing;

use crate::blob::{parse_blob, BlobError};
use crate::capability::AgentCapability;
use crate::namespace::{McpSecretRef, MCP_NAMESPACE_PREFIX};

/// Where the raw blob bytes come from.
///
/// Implemented by the keyring backend in production and by
/// [`crate::testing::MemoryBlobSource`] in tests, so a test drives the same
/// lookup code the launcher runs.
pub trait SecretBlobSource {
    /// Read the raw blob. `Ok(None)` when no blob has been written yet.
    ///
    /// # Errors
    /// A backend-specific message when the store is unavailable. Unavailable
    /// must never be reported as "no blob": that would turn a keychain outage
    /// into a silent authorization failure.
    fn read_blob(&self) -> Result<Option<Vec<u8>>, String>;
}

/// A borrowed source is a source, so a caller that keeps ownership of its
/// store — a test asserting the store's contents alongside the lookup, or a
/// process holding one handle for several readers — can still build a lookup
/// without cloning the backend.
impl<T: SecretBlobSource + ?Sized> SecretBlobSource for &T {
    fn read_blob(&self) -> Result<Option<Vec<u8>>, String> {
        (**self).read_blob()
    }
}

/// Why a secret could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LookupError {
    /// The secret store itself could not be read.
    #[error("secret store unavailable: {0}")]
    Unavailable(String),
    /// The store holds no blob at all.
    #[error("secret store holds no secrets")]
    Empty,
    /// The blob could not be parsed or breached a bound.
    #[error("secret store unreadable: {0}")]
    Blob(#[from] BlobError),
    /// No record is bound to this capability under this reference.
    ///
    /// The same variant covers "no such secret" and "that secret belongs to
    /// another agent": a caller must not be able to tell the two apart.
    #[error("no secret `{0}` is bound to this capability")]
    NotBound(String),
}

/// A resolved secret value.
///
/// Wrapped in [`Zeroizing`] so the buffer is cleared when it goes out of scope,
/// and with a redacting `Debug`, so neither a log line nor a panic message can
/// carry the value.
pub struct SecretValue(Zeroizing<String>);

impl SecretValue {
    /// Borrow the value. The only way to read it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretValue(<redacted>)")
    }
}

/// Blob key for one secret, derived from the capability's binding.
///
/// `mcp:<agent id>:<generation>:<reference id>`. `agent_id` and `generation`
/// come from the capability alone; the caller never supplies them.
pub fn storage_key(capability: &AgentCapability, reference: &McpSecretRef) -> String {
    format!(
        "{MCP_NAMESPACE_PREFIX}{}:{}:{}",
        capability.agent_id(),
        capability.generation(),
        reference.id()
    )
}

/// Suffix of the binding record's blob key.
///
/// `#` is outside the reference-id charset (`[a-z0-9_-]`, enforced by
/// [`McpSecretRef::parse`]), so no reference can ever name the binding record
/// and no secret can ever be stored under its key.
pub const BINDING_KEY_SUFFIX: &str = "#binding";

/// Blob key of the record holding one capability's nonce.
///
/// `mcp:<agent id>:<generation>:#binding`. The minting side writes
/// [`AgentCapability::binding_value`] here when it stages a generation; every
/// resolve for that agent and generation verifies against it.
pub fn binding_key(capability: &AgentCapability) -> String {
    binding_key_for(capability.agent_id(), capability.generation())
}

/// Blob key of the binding record for `agent_id` at `generation`.
///
/// The spawn side needs this key *before* it holds a capability — the nonce it
/// reads here is what makes one — so the key is derivable from the pair alone.
/// Knowing the key authorizes nothing: reading the record is how a caller
/// proves it may act for that agent, and the record is only readable by a
/// process that can already read the whole store.
pub fn binding_key_for(agent_id: &str, generation: u64) -> String {
    format!("{MCP_NAMESPACE_PREFIX}{agent_id}:{generation}:{BINDING_KEY_SUFFIX}")
}

/// The typed, MCP-only read side of the secret store.
pub struct McpSecretLookup<S: SecretBlobSource> {
    source: S,
}

impl<S: SecretBlobSource> McpSecretLookup<S> {
    /// Build a lookup over `source`.
    pub fn new(source: S) -> Self {
        Self { source }
    }

    /// Resolve the secret `reference` names for the agent `capability` binds.
    ///
    /// # Errors
    /// [`LookupError::Unavailable`] when the store could not be read,
    /// [`LookupError::Empty`] when nothing is stored, [`LookupError::Blob`]
    /// when the blob is unreadable or over a bound, and
    /// [`LookupError::NotBound`] when this capability has no such secret —
    /// including the case where another agent owns one by that name, and the
    /// case where the capability's nonce does not match the agent's binding
    /// record.
    pub fn resolve(
        &self,
        capability: &AgentCapability,
        reference: &McpSecretRef,
    ) -> Result<SecretValue, LookupError> {
        let raw = self
            .source
            .read_blob()
            .map_err(LookupError::Unavailable)?
            .ok_or(LookupError::Empty)?;
        let map = parse_blob(&raw)?;

        // The nonce is the authorization. It is verified before a storage key
        // is derived, so a capability that is not the one this agent and
        // generation were minted with never addresses anything. An absent
        // binding record and a wrong nonce are the same answer as a missing
        // secret: a caller must not be able to tell them apart.
        let unbound = || LookupError::NotBound(reference.to_string());
        let stored = map.get(&binding_key(capability)).ok_or_else(unbound)?;
        if !capability.nonce_matches(stored) {
            return Err(unbound());
        }

        let key = storage_key(capability, reference);
        map.get(&key)
            .map(|value| SecretValue(Zeroizing::new(value.clone())))
            .ok_or_else(unbound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::NONCE_LEN;
    use crate::testing::MemoryBlobSource;

    fn capability(agent: &str, generation: u64) -> AgentCapability {
        AgentCapability::mint(agent, generation, [1u8; NONCE_LEN]).expect("valid agent id")
    }

    /// A capability for the same agent and generation with different entropy —
    /// what a forger can build from public facts alone.
    fn forged(agent: &str, generation: u64) -> AgentCapability {
        AgentCapability::mint(agent, generation, [0xABu8; NONCE_LEN]).expect("valid agent id")
    }

    fn reference(id: &str) -> McpSecretRef {
        McpSecretRef::parse(&format!("mcp:{id}")).expect("valid reference")
    }

    fn populated() -> MemoryBlobSource {
        let source = MemoryBlobSource::default();
        for agent in ["agent-a", "agent-b"] {
            let capability = capability(agent, 1);
            source.insert(&binding_key(&capability), capability.binding_value());
        }
        source.insert("mcp:agent-a:1:token", "A-SECRET");
        source.insert("mcp:agent-b:1:token", "B-SECRET");
        source.insert("identity", "HUMAN-NSEC");
        source.insert("agent:npub1abc", "AGENT-NSEC");
        source
    }

    #[test]
    fn capability_cannot_cross_agents() {
        let lookup = McpSecretLookup::new(populated());
        let agent_a = capability("agent-a", 1);
        let agent_b = capability("agent-b", 1);

        assert_eq!(
            lookup
                .resolve(&agent_a, &reference("token"))
                .expect("own secret resolves")
                .expose(),
            "A-SECRET"
        );
        assert_eq!(
            lookup
                .resolve(&agent_b, &reference("token"))
                .expect("own secret resolves")
                .expose(),
            "B-SECRET"
        );

        // Agent A's capability resolves only agent A's namespace. There is no
        // spelling of a reference that reaches agent B's record, `identity`, or
        // an `agent:*` key: the first two are refused at parse, and the third
        // cannot be expressed because the key prefix comes from the capability.
        assert!(McpSecretRef::parse("identity").is_err());
        assert!(McpSecretRef::parse("mcp:agent:npub1abc").is_err());
        assert!(McpSecretRef::parse("mcp:agent-b:1:token").is_err());
        assert_eq!(
            lookup
                .resolve(&agent_a, &reference("nothing-here"))
                .expect_err("must not resolve"),
            LookupError::NotBound("mcp:nothing-here".to_string())
        );
    }

    #[test]
    fn a_forged_nonce_resolves_nothing() {
        // The agent id and the generation are guessable, so this is exactly
        // what a hostile local process can build: the right agent, the right
        // generation, and 32 hex characters of its own. Removing the binding
        // check in `resolve` makes this test fail.
        let lookup = McpSecretLookup::new(populated());
        let forged = forged("agent-a", 1);
        assert_eq!(forged.agent_id(), capability("agent-a", 1).agent_id());
        assert_eq!(forged.generation(), capability("agent-a", 1).generation());
        assert_eq!(
            lookup
                .resolve(&forged, &reference("token"))
                .expect_err("a forged capability must resolve nothing"),
            LookupError::NotBound("mcp:token".to_string())
        );

        // And a store with no binding record at all resolves nothing either:
        // the check fails closed, not open.
        let unbound = MemoryBlobSource::default();
        unbound.insert("mcp:agent-a:1:token", "A-SECRET");
        assert_eq!(
            McpSecretLookup::new(unbound)
                .resolve(&capability("agent-a", 1), &reference("token"))
                .expect_err("an unbound generation must resolve nothing"),
            LookupError::NotBound("mcp:token".to_string())
        );
    }

    #[test]
    fn the_binding_record_cannot_be_named_by_a_reference() {
        // `#` is outside the reference charset, so no reference spells the
        // binding key and no secret can be stored under it.
        assert!(McpSecretRef::parse(&format!("mcp:{BINDING_KEY_SUFFIX}")).is_err());
        assert!(McpSecretRef::parse("mcp:agent-a:1:#binding").is_err());
    }

    #[test]
    fn a_capability_for_another_generation_resolves_nothing() {
        let lookup = McpSecretLookup::new(populated());
        assert_eq!(
            lookup
                .resolve(&capability("agent-a", 2), &reference("token"))
                .expect_err("another generation must not resolve"),
            LookupError::NotBound("mcp:token".to_string())
        );
    }

    #[test]
    fn store_failures_are_surfaced_not_swallowed() {
        struct Failing;
        impl SecretBlobSource for Failing {
            fn read_blob(&self) -> Result<Option<Vec<u8>>, String> {
                Err("keyring unavailable".to_string())
            }
        }
        assert_eq!(
            McpSecretLookup::new(Failing)
                .resolve(&capability("agent-a", 1), &reference("token"))
                .expect_err("an unavailable store must not read as empty"),
            LookupError::Unavailable("keyring unavailable".to_string())
        );

        struct Absent;
        impl SecretBlobSource for Absent {
            fn read_blob(&self) -> Result<Option<Vec<u8>>, String> {
                Ok(None)
            }
        }
        assert_eq!(
            McpSecretLookup::new(Absent)
                .resolve(&capability("agent-a", 1), &reference("token"))
                .expect_err("an absent blob must not resolve"),
            LookupError::Empty
        );
    }

    #[test]
    fn resolved_values_redact_in_debug() {
        let lookup = McpSecretLookup::new(populated());
        let value = lookup
            .resolve(&capability("agent-a", 1), &reference("token"))
            .expect("resolves");
        assert_eq!(format!("{value:?}"), "SecretValue(<redacted>)");
    }
}
