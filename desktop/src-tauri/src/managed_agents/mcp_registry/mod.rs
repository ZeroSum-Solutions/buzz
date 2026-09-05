//! The MCP server registry: an operator-editable document, the rules that
//! decide which of its entries an agent may use, and the generation of the
//! three configuration artefacts under a staged, journalled generation.
//!
//! Accepted design: `docs/plans/2026-09-04-mcp-registry-design.md`. This module
//! is the backend core only — nothing here is wired to the spawn path yet, and
//! the Settings panel and the per-agent toggles are separate changes.
//!
//! The pieces, and the memo decision each answers:
//!
//! * [`schema`] — the document shape and every bound on it.
//! * [`load`] — bounded, no-follow reading and validation, with whole-document
//!   rejection kept to the three cases decision 7 names and everything else
//!   per entry with a status string.
//! * [`resolve`] — one agent's toggles against a loaded registry, refusing the
//!   spawn rather than starting an agent short a server it was told to have.
//! * [`generate`] — the buzz-acp registry file, Claude's project `.mcp.json`
//!   and Codex's `config.toml`, all naming the bundled launcher by absolute
//!   path and carrying references rather than values.
//! * [`generation`] — the staging tree, the single pointer rename, and the
//!   `PREPARED`/`FLIPPED`/`CLEANED` journal.

pub mod generate;
pub mod generation;
pub mod load;
pub mod resolve;
pub mod schema;

#[cfg(test)]
mod tests;

/// File name of the registry document inside the app data directory.
pub const REGISTRY_FILE_NAME: &str = "mcp_servers.json";
