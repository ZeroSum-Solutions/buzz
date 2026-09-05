//! The `mcp_servers.json` document shape and its bounds.
//!
//! Every bound here caps the quantity that actually costs — entries, argument
//! count, bytes — because this document is operator-editable and reaches the
//! spawn path.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Largest accepted registry document, in bytes.
pub const MAX_DOCUMENT_BYTES: usize = 256 * 1024;

/// Largest accepted single entry, in serialized bytes.
pub const MAX_ENTRY_BYTES: usize = 4 * 1024;

/// Largest accepted number of servers in the whole document.
///
/// The byte cap alone bounds the document, not the entry count a loader has to
/// validate and cross-check; this bounds that count directly.
pub const MAX_DOCUMENT_SERVERS: usize = 256;

/// Largest accepted number of servers one agent may enable.
///
/// Inherited from buzz-acp (`crates/buzz-acp/src/lib.rs:5888`).
pub const MAX_SERVERS_PER_AGENT: usize = 16;

/// Largest accepted server name or id, in bytes.
pub const MAX_NAME_LEN: usize = 64;

/// Largest accepted number of command arguments on one entry.
pub const MAX_ARGS: usize = 64;

/// Largest accepted length of one command argument, in bytes.
pub const MAX_ARG_LEN: usize = 1024;

/// Largest accepted number of `env` entries on one server.
pub const MAX_ENV_ENTRIES: usize = 32;

/// Largest accepted `env` value, in bytes.
pub const MAX_ENV_VALUE_LEN: usize = 4 * 1024;

/// Reserved name prefix; no registry server may use it.
pub const RESERVED_NAME_PREFIX: &str = "buzz-";

/// Built-in server names a registry entry may not shadow.
///
/// buzz-acp resolves a collision by appending a hash suffix
/// (`crates/buzz-acp/src/lib.rs:6168-6173`), which changes the qualified tool
/// name the model calls. That is the right last resort for a parsed env var and
/// the wrong one for a registry an operator edits, so the collision is refused
/// here instead.
pub const BUILTIN_SERVER_NAMES: &[&str] = &["buzz", "buzz-dev-mcp", "developer", "memory"];

/// The document as written on disk.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct RegistryDocument {
    /// Schema version. Read rather than inferred, so a later default change is
    /// a version bump and not a reinterpretation of an old file.
    pub version: u32,
    /// The declared servers, in document order.
    #[serde(default)]
    pub servers: Vec<RegistryEntry>,
}

/// One server as written on disk.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct RegistryEntry {
    /// Stable unique id. Agent records refer to a server by this, never by
    /// name, so renaming a server does not silently disable it.
    pub id: String,
    /// Unique display name. Becomes the key in every generated config, so it
    /// is checked case-insensitively across the whole document.
    pub name: String,
    /// How this server is reached.
    #[serde(flatten)]
    pub transport: RegistryTransport,
    /// Environment declared for this server. Values are either an `mcp:`
    /// reference or a literal the sentinel scan cleared as non-credential.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// The two server classes (memo decision 1).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum RegistryTransport {
    /// A child process speaking MCP over stdio.
    Stdio {
        /// Absolute path of the server executable.
        command: String,
        /// Arguments passed to it.
        #[serde(default)]
        args: Vec<String>,
    },
    /// A Streamable HTTP endpoint, reached through the proxy.
    Http {
        /// The upstream URL.
        url: String,
        /// Credential to attach, when the upstream needs one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auth: Option<HttpAuth>,
    },
}

/// How the proxy authenticates to an HTTP upstream.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct HttpAuth {
    /// One of the pinned schemes: `bearer`, `token`, `basic`, `api-key`.
    pub scheme: String,
    /// The `mcp:` reference naming the credential. Never a value.
    pub secret: String,
}

impl RegistryEntry {
    /// Whether this entry needs an HTTP-capable runtime.
    pub fn is_http(&self) -> bool {
        matches!(self.transport, RegistryTransport::Http { .. })
    }
}
