//! Read side of the Buzz desktop keychain blob, plus the MCP secret namespace
//! and the per-agent capability that authorizes a read.
//!
//! `desktop/src-tauri/src/secret_store.rs` is a private Tauri module, so a
//! workspace binary such as `buzz-mcp-launch` cannot call it. This crate
//! carries the parts a sidecar needs — the blob format and its bounds, the
//! reserved `mcp:` namespace, the capability, and the one typed lookup — while
//! the desktop keeps the write side and delegates its raw read here so the two
//! processes can never drift on the format.
//!
//! See `docs/plans/2026-09-04-mcp-registry-design.md` decision 5.

#![forbid(unsafe_code)]

pub mod blob;
pub mod capability;
pub mod lookup;
pub mod namespace;
pub mod sentinel;
pub mod testing;

#[cfg(feature = "system-keyring")]
pub mod keyring_source;

pub use blob::{parse_blob, serialize_blob, BlobError, BLOB_KEY};
pub use capability::{AgentCapability, CapabilityError, CAPABILITY_ENV_VAR};
pub use lookup::{
    binding_key, storage_key, LookupError, McpSecretLookup, SecretBlobSource, SecretValue,
};
pub use namespace::{looks_like_reference, McpSecretRef, ReferenceError};
pub use sentinel::{scan_argv, scan_query_pairs, scan_value, SentinelHit};
