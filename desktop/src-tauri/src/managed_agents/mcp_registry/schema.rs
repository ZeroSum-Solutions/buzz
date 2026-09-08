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

/// Largest accepted registry id, in bytes.
///
/// An id never leaves the desktop: it keys the agent record's enabled list and
/// nothing downstream reads it, so it keeps the wider bound.
pub const MAX_ID_LEN: usize = 64;

/// Largest accepted server name, in bytes.
///
/// Unified with the consumer (Sol W4). A name is the one operator string that
/// crosses into `buzz-acp`, which caps a generated name at
/// `MAX_MCP_NAME_LEN` — 32 bytes, budgeted against `buzz-agent`'s 64-byte
/// qualified `<server>__<tool>` name — and refuses the **whole** handover
/// document past it. A desktop that accepted 64 would write a document every
/// registry-enabled agent then failed to start on.
/// `mcp_registry_name_bounds_match_the_consumer` pins the two together.
pub const MAX_NAME_LEN: usize = 32;

/// Largest accepted number of command arguments on one entry.
pub const MAX_ARGS: usize = 64;

/// Largest accepted length of one command argument, in bytes.
///
/// Unified with `buzz_acp::mcp_registry::MAX_REGISTRY_ARG_LEN`: every one of
/// these strings is copied verbatim into the generated launcher argv, which
/// the consumer bounds at the same number.
pub const MAX_ARG_LEN: usize = 1024;

/// Largest accepted number of `env` entries on one server.
pub const MAX_ENV_ENTRIES: usize = 32;

/// Largest accepted `env` variable name, in bytes.
///
/// The launcher's own bound (`buzz_mcp_launch::cli::MAX_ENV_NAME_LEN`); a
/// longer name would be refused by the process the desktop generated the
/// argument for.
pub const MAX_ENV_NAME_LEN: usize = 128;

/// Largest accepted `env` value, in bytes.
///
/// Derived, not chosen. Each declared variable is generated as one
/// `NAME=VALUE` launcher argument, and `buzz-acp` refuses the whole handover
/// document when any argument passes [`MAX_ARG_LEN`]. Deriving the value cap
/// from the name cap and the argument cap makes the worst case fit by
/// construction rather than by a number somebody has to keep in step.
pub const MAX_ENV_VALUE_LEN: usize = MAX_ARG_LEN - MAX_ENV_NAME_LEN - 1;

/// Largest accepted number of arguments on one **generated** launcher command
/// line.
///
/// The bound that actually costs is the generated argv, not the entry's own
/// `args`: the generator prepends the service flags, the mode, the server name
/// and two arguments per declared variable, and `buzz-acp` bounds the sum.
/// Bounding `args` alone let an entry with 64 arguments and 32 variables
/// generate 135 and take every registry-enabled agent down at startup.
pub const MAX_GENERATED_ARGS: usize = 64;

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
