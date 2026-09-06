//! The per-agent, per-spawn capability that authorizes an MCP secret read.
//!
//! Memo decision 5: the v1 authorization boundary is the agent, not the server.
//! The desktop mints one unguessable capability for the pair (agent id,
//! configuration generation) at spawn and puts it in the spawn environment; the
//! adapter chain forwards it unchanged; `buzz-mcp-launch` strips it from its own
//! environment before it starts a server. A raw `mcp:` id authorizes nothing —
//! the agent and the generation a lookup reads come only from the capability, so
//! no reference can address another agent's key.
//!
//! The agent id and the generation are both guessable, so they cannot be the
//! authorization on their own: the nonce is. The minting side persists it in
//! the agent's binding record ([`crate::lookup::binding_key`]) and
//! [`crate::lookup::McpSecretLookup::resolve`] compares it in constant time
//! before it derives any blob key, so a forged `BUZZ_MCP_CAPABILITY` carrying
//! another agent's id resolves nothing.
//!
//! Nothing in this slice mints a capability: the desktop writes generated
//! configuration but does not yet spawn through it, so no binding record is
//! written either. The check is therefore fail-closed today — every resolve
//! ends in [`crate::LookupError::NotBound`] — and the spawn wiring that mints
//! a capability writes [`Self::binding_value`] to [`crate::binding_key`] in the
//! same commit that stages the generation.

use std::fmt;

/// Environment variable carrying the capability through the spawn chain.
pub const CAPABILITY_ENV_VAR: &str = "BUZZ_MCP_CAPABILITY";

/// Wire prefix, so a future format change is detectable rather than silent.
const CAPABILITY_VERSION: &str = "v1";

/// Nonce length in bytes; rendered as `2 * NONCE_LEN` lowercase hex characters.
pub const NONCE_LEN: usize = 16;

/// Longest accepted agent id, in bytes.
pub const MAX_AGENT_ID_LEN: usize = 128;

/// Longest accepted capability string, in bytes. Bounds what a hostile spawn
/// environment can hand the launcher before any parsing work happens.
pub const MAX_CAPABILITY_LEN: usize = 256;

/// Why a capability string is not usable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CapabilityError {
    /// The string is longer than [`MAX_CAPABILITY_LEN`].
    #[error("capability is {0} bytes, over the {MAX_CAPABILITY_LEN}-byte cap")]
    TooLong(usize),
    /// The string does not have the four `.`-separated fields.
    #[error("capability is malformed")]
    Malformed,
    /// The version field is not one this build understands.
    #[error("capability version `{0}` is not supported")]
    UnsupportedVersion(String),
    /// The agent id is empty, over [`MAX_AGENT_ID_LEN`], or uses a character
    /// outside `[a-z0-9_-]`.
    #[error("capability agent id is invalid")]
    InvalidAgentId,
    /// The generation field is not a base-10 `u64`.
    #[error("capability generation is invalid")]
    InvalidGeneration,
    /// The nonce is not exactly `2 * NONCE_LEN` lowercase hex characters.
    #[error("capability nonce is invalid")]
    InvalidNonce,
    /// [`CAPABILITY_ENV_VAR`] is unset or not valid UTF-8.
    #[error("{CAPABILITY_ENV_VAR} is not set")]
    Absent,
}

/// A per-agent, per-spawn capability for one configuration generation.
///
/// `Debug` and `Display` never render the nonce: the capability is a bearer
/// token, and a log line or a panic message carrying one is a credential leak.
/// Use [`AgentCapability::to_env_value`] for the one place it must be rendered.
#[derive(Clone, PartialEq, Eq)]
pub struct AgentCapability {
    agent_id: String,
    generation: u64,
    nonce: String,
}

impl AgentCapability {
    /// Mint a capability for `agent_id` at `generation` from caller-supplied
    /// entropy.
    ///
    /// The randomness is a parameter rather than an internal RNG so this crate
    /// stays dependency-light and the caller keeps one audited entropy source.
    ///
    /// # Errors
    /// Returns [`CapabilityError::InvalidAgentId`] when `agent_id` is empty,
    /// over [`MAX_AGENT_ID_LEN`], or uses a character outside `[a-z0-9_-]`.
    pub fn mint(
        agent_id: &str,
        generation: u64,
        nonce: [u8; NONCE_LEN],
    ) -> Result<Self, CapabilityError> {
        if !is_valid_agent_id(agent_id) {
            return Err(CapabilityError::InvalidAgentId);
        }
        let mut hex = String::with_capacity(NONCE_LEN * 2);
        for byte in nonce {
            use fmt::Write as _;
            // Writing to a String is infallible; the Result is discarded on
            // purpose rather than unwrapped (no `unwrap` in production paths).
            let _ = write!(hex, "{byte:02x}");
        }
        Ok(Self {
            agent_id: agent_id.to_string(),
            generation,
            nonce: hex,
        })
    }

    /// Rebuild a capability from a stored binding record.
    ///
    /// The spawn side reads the nonce [`Self::binding_value`] wrote and needs
    /// the capability back; going through the wire spelling would put the
    /// format in two places. `nonce` is validated exactly as [`Self::parse`]
    /// validates it, so a corrupt record is refused rather than minted into a
    /// capability nothing can resolve.
    ///
    /// # Errors
    /// [`CapabilityError::InvalidAgentId`] or [`CapabilityError::InvalidNonce`].
    pub fn bind(agent_id: &str, generation: u64, nonce: &str) -> Result<Self, CapabilityError> {
        if !is_valid_agent_id(agent_id) {
            return Err(CapabilityError::InvalidAgentId);
        }
        if !is_valid_nonce(nonce) {
            return Err(CapabilityError::InvalidNonce);
        }
        Ok(Self {
            agent_id: agent_id.to_string(),
            generation,
            nonce: nonce.to_string(),
        })
    }

    /// Parse the wire spelling `v1.<agent>.<generation>.<nonce-hex>`.
    ///
    /// # Errors
    /// Returns the [`CapabilityError`] describing the first field that failed.
    pub fn parse(raw: &str) -> Result<Self, CapabilityError> {
        if raw.len() > MAX_CAPABILITY_LEN {
            return Err(CapabilityError::TooLong(raw.len()));
        }
        let mut fields = raw.split('.');
        let (Some(version), Some(agent_id), Some(generation), Some(nonce), None) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            return Err(CapabilityError::Malformed);
        };
        if version != CAPABILITY_VERSION {
            return Err(CapabilityError::UnsupportedVersion(version.to_string()));
        }
        if !is_valid_agent_id(agent_id) {
            return Err(CapabilityError::InvalidAgentId);
        }
        let generation: u64 = generation
            .parse()
            .map_err(|_| CapabilityError::InvalidGeneration)?;
        if !is_valid_nonce(nonce) {
            return Err(CapabilityError::InvalidNonce);
        }
        Ok(Self {
            agent_id: agent_id.to_string(),
            generation,
            nonce: nonce.to_string(),
        })
    }

    /// Read the capability from [`CAPABILITY_ENV_VAR`] in `environment`.
    ///
    /// # Errors
    /// [`CapabilityError::Absent`] when the variable is unset, otherwise the
    /// parse error.
    pub fn from_env<F>(lookup: F) -> Result<Self, CapabilityError>
    where
        F: FnOnce(&str) -> Option<String>,
    {
        let raw = lookup(CAPABILITY_ENV_VAR).ok_or(CapabilityError::Absent)?;
        Self::parse(&raw)
    }

    /// The agent this capability is bound to.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// The configuration generation this capability is bound to.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The nonce, for the one caller that must persist it: the minting side
    /// writes it into the agent's binding record so a lookup can verify it.
    ///
    /// This is the secret half of the capability. Write it to the secret store
    /// and nowhere else — never to a log line, a generated config, or argv.
    pub fn binding_value(&self) -> &str {
        &self.nonce
    }

    /// Whether `stored` is this capability's nonce, compared in constant time.
    ///
    /// A forged capability carries the right agent id and generation (both are
    /// guessable) and a wrong nonce, so this comparison is the whole
    /// authorization check. It runs in time independent of how many leading
    /// characters match, so a caller cannot search the nonce one byte at a
    /// time.
    pub fn nonce_matches(&self, stored: &str) -> bool {
        let mine = self.nonce.as_bytes();
        let theirs = stored.as_bytes();
        // The length is not a secret (it is a compile-time constant), so
        // comparing it up front leaks nothing; the byte fold below is what
        // must not short-circuit.
        if mine.len() != theirs.len() {
            return false;
        }
        let mut difference = 0u8;
        for (a, b) in mine.iter().zip(theirs) {
            difference |= a ^ b;
        }
        difference == 0
    }

    /// Render the wire spelling. The only place the nonce is emitted.
    pub fn to_env_value(&self) -> String {
        format!(
            "{CAPABILITY_VERSION}.{}.{}.{}",
            self.agent_id, self.generation, self.nonce
        )
    }
}

impl fmt::Debug for AgentCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentCapability")
            .field("agent_id", &self.agent_id)
            .field("generation", &self.generation)
            .field("nonce", &"<redacted>")
            .finish()
    }
}

/// Whether `nonce` is exactly `2 * NONCE_LEN` lowercase hex characters.
fn is_valid_nonce(nonce: &str) -> bool {
    nonce.len() == NONCE_LEN * 2
        && nonce
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_valid_agent_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_AGENT_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(agent: &str, generation: u64) -> AgentCapability {
        AgentCapability::mint(agent, generation, [7u8; NONCE_LEN]).expect("valid agent id")
    }

    #[test]
    fn wire_form_round_trips() {
        let minted = cap("agent-a", 4);
        let parsed = AgentCapability::parse(&minted.to_env_value()).expect("round trip");
        assert_eq!(parsed, minted);
        assert_eq!(parsed.agent_id(), "agent-a");
        assert_eq!(parsed.generation(), 4);
    }

    #[test]
    fn debug_never_renders_the_nonce() {
        let rendered = format!("{:?}", cap("agent-a", 1));
        assert!(rendered.contains("<redacted>"), "{rendered}");
        assert!(!rendered.contains("0707"), "{rendered}");
    }

    #[test]
    fn malformed_capabilities_are_refused() {
        assert_eq!(
            AgentCapability::parse(&"v".repeat(MAX_CAPABILITY_LEN + 1)),
            Err(CapabilityError::TooLong(MAX_CAPABILITY_LEN + 1))
        );
        assert_eq!(
            AgentCapability::parse("v1.agent-a.4"),
            Err(CapabilityError::Malformed)
        );
        assert_eq!(
            AgentCapability::parse("v1.agent-a.4.0707.extra"),
            Err(CapabilityError::Malformed)
        );
        assert_eq!(
            AgentCapability::parse("v2.agent-a.4.07070707070707070707070707070707"),
            Err(CapabilityError::UnsupportedVersion("v2".into()))
        );
        assert_eq!(
            AgentCapability::parse("v1.Agent.4.07070707070707070707070707070707"),
            Err(CapabilityError::InvalidAgentId)
        );
        assert_eq!(
            AgentCapability::parse("v1.agent-a.x.07070707070707070707070707070707"),
            Err(CapabilityError::InvalidGeneration)
        );
        assert_eq!(
            AgentCapability::parse("v1.agent-a.4.ABCD"),
            Err(CapabilityError::InvalidNonce)
        );
        assert!(AgentCapability::mint("", 1, [0u8; NONCE_LEN]).is_err());
        assert!(
            AgentCapability::mint(&"a".repeat(MAX_AGENT_ID_LEN + 1), 1, [0u8; NONCE_LEN]).is_err()
        );
    }

    #[test]
    fn nonce_matching_is_exact_and_length_checked() {
        let minted = cap("agent-a", 1);
        assert!(minted.nonce_matches(minted.binding_value()));
        // One character different, and every length that is not the nonce's.
        let mut forged = minted.binding_value().to_string();
        forged.replace_range(0..1, "8");
        assert!(!minted.nonce_matches(&forged));
        assert!(!minted.nonce_matches(""));
        assert!(!minted.nonce_matches(&"07".repeat(NONCE_LEN + 1)));
    }

    #[test]
    fn from_env_reports_absence_distinctly() {
        assert_eq!(
            AgentCapability::from_env(|_| None),
            Err(CapabilityError::Absent)
        );
        let value = cap("agent-a", 2).to_env_value();
        let read = AgentCapability::from_env(|name| {
            assert_eq!(name, CAPABILITY_ENV_VAR);
            Some(value.clone())
        })
        .expect("present");
        assert_eq!(read.agent_id(), "agent-a");
    }
}
