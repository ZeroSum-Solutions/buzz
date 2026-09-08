//! The MCP server registry: an operator-editable document, the rules that
//! decide which of its entries an agent may use, and the generation of the
//! three configuration artefacts under a staged, journalled generation.
//!
//! Accepted design: `docs/plans/2026-09-04-mcp-registry-design.md`. The
//! backend core and the spawn wiring; the Settings panel and the per-agent
//! toggles are a separate change.
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
//! * [`paths`] — the roots, and the one place an agent id becomes a path.
//! * [`converge`] — one adopted generation from the document plus each agent's
//!   selection, with each agent's secrets re-keyed onto it before the flip.
//! * [`apply`] — the production caller: a registry edit or a toggle change
//!   becomes one adopted generation, and a start finishes what a crash left.
//! * [`spawn`] — what one spawn takes from the adopted generation: the
//!   artefacts at the placement its runtime names, its own working directory
//!   when that placement needs one, and the capability.

pub mod apply;
pub mod converge;
pub mod generate;
pub mod generation;
pub mod load;
pub mod paths;
pub mod resolve;
pub mod schema;
pub mod spawn;

#[cfg(test)]
mod apply_tests;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod wiring_tests;

/// File name of the registry document inside the app data directory.
pub const REGISTRY_FILE_NAME: &str = "mcp_servers.json";
