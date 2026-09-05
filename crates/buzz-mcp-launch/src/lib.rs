//! The bundled launcher and credential-resolving proxy for registry MCP servers.
//!
//! One binary, two modes (`docs/plans/2026-09-04-mcp-registry-design.md`
//! decisions 1, 3 and 4):
//!
//! * **launch** — every generated stdio entry names this binary as its command.
//!   It builds the child environment from empty, resolves `mcp:` references
//!   through the per-agent capability, strips that capability before the server
//!   starts, and stays resident as a supervisor so the whole tree dies with the
//!   adapter.
//! * **proxy** — a local stdio MCP server in front of a Streamable HTTP
//!   upstream. It resolves its one bound secret at first use and attaches it as
//!   a header, so no generated JSON or TOML ever holds a secret value.
//!
//! On Unix the crate takes no `unsafe`; the Windows Job Object calls are the
//! only FFI, each one checked.
#![cfg_attr(not(windows), forbid(unsafe_code))]
#![cfg_attr(windows, deny(unsafe_code))]

pub mod cli;
pub mod env;
pub mod launch;
pub mod proxy;

use std::collections::BTreeMap;

use buzz_secret_store::{AgentCapability, McpSecretLookup, McpSecretRef, SecretBlobSource};

/// Why a `mcp:` reference could not be turned into a value at launch.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// The spawn environment carried no usable capability, but the entry
    /// declares a reference that needs one.
    #[error("cannot resolve `{name}`: {source}")]
    Capability {
        /// The environment variable the reference was declared for.
        name: String,
        /// Why the capability was unusable.
        source: buzz_secret_store::CapabilityError,
    },
    /// The store refused the read.
    #[error("cannot resolve `{name}`: {source}")]
    Lookup {
        /// The environment variable the reference was declared for.
        name: String,
        /// Why the lookup failed.
        source: buzz_secret_store::LookupError,
    },
}

/// Resolve every declared `mcp:` reference into a value.
///
/// The capability is required only when at least one reference is declared, so
/// a server with no secrets starts on a machine with no keychain at all.
///
/// # Errors
/// [`ResolveError`] naming the variable that could not be resolved. A failure
/// is never downgraded to an absent variable: starting a server without the
/// credential it declared would make it fail later, somewhere less legible.
pub fn resolve_references<S: SecretBlobSource>(
    references: &BTreeMap<String, McpSecretRef>,
    capability: Result<AgentCapability, buzz_secret_store::CapabilityError>,
    lookup: &McpSecretLookup<S>,
) -> Result<BTreeMap<String, String>, ResolveError> {
    if references.is_empty() {
        return Ok(BTreeMap::new());
    }
    let capability = match capability {
        Ok(capability) => capability,
        Err(source) => {
            let name = references
                .keys()
                .next()
                .cloned()
                .unwrap_or_else(|| "<unknown>".to_string());
            return Err(ResolveError::Capability { name, source });
        }
    };

    let mut resolved = BTreeMap::new();
    for (name, reference) in references {
        let value =
            lookup
                .resolve(&capability, reference)
                .map_err(|source| ResolveError::Lookup {
                    name: name.clone(),
                    source,
                })?;
        resolved.insert(name.clone(), value.expose().to_string());
    }
    Ok(resolved)
}

/// This process's environment as an owned map.
///
/// Variables with non-UTF-8 names or values are skipped: they cannot be
/// declared in a registry entry and cannot be compared against the allowlist.
/// Enumerated through [`std::env::vars_os`] because [`std::env::vars`] panics
/// on exactly that input, and on Unix an environment entry is a byte string —
/// one invalid value anywhere in the harness's environment would take the
/// launcher down at `main`, before it filters anything or spawns the server.
pub fn inherited_environment() -> BTreeMap<String, String> {
    filter_utf8_environment(std::env::vars_os())
}

/// The UTF-8 entries of `vars`, in an owned map.
///
/// Split from [`inherited_environment`] so the skipping can be driven with a
/// non-UTF-8 entry the process environment cannot portably be given.
fn filter_utf8_environment(
    vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> BTreeMap<String, String> {
    vars.into_iter()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_secret_store::capability::NONCE_LEN;
    use buzz_secret_store::testing::MemoryBlobSource;

    #[test]
    fn a_missing_capability_is_an_error_not_an_absent_variable() {
        let mut references = BTreeMap::new();
        references.insert(
            "TOKEN".to_string(),
            McpSecretRef::parse("mcp:token").expect("valid"),
        );
        let lookup = McpSecretLookup::new(MemoryBlobSource::default());
        let error = resolve_references(
            &references,
            Err(buzz_secret_store::CapabilityError::Absent),
            &lookup,
        )
        .expect_err("must not start a server short a declared credential");
        assert!(
            matches!(error, ResolveError::Capability { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn no_references_needs_no_capability() {
        let lookup = McpSecretLookup::new(MemoryBlobSource::default());
        let resolved = resolve_references(
            &BTreeMap::new(),
            Err(buzz_secret_store::CapabilityError::Absent),
            &lookup,
        )
        .expect("a server with no secrets starts without a keychain");
        assert!(resolved.is_empty());
    }

    #[test]
    fn declared_references_resolve_through_the_capability() {
        let source = MemoryBlobSource::default();
        let bound = AgentCapability::mint("agent-a", 3, [2u8; NONCE_LEN]).expect("valid");
        source.insert(
            &buzz_secret_store::binding_key(&bound),
            bound.binding_value(),
        );
        source.insert("mcp:agent-a:3:token", "A-SECRET");
        let lookup = McpSecretLookup::new(source);
        let mut references = BTreeMap::new();
        references.insert(
            "TOKEN".to_string(),
            McpSecretRef::parse("mcp:token").expect("valid"),
        );
        let capability = AgentCapability::mint("agent-a", 3, [2u8; NONCE_LEN]).expect("valid");
        let resolved = resolve_references(&references, Ok(capability), &lookup).expect("resolves");
        assert_eq!(resolved.get("TOKEN").map(String::as_str), Some("A-SECRET"));
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_environment_entry_is_skipped_rather_than_fatal() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        // 0x80 is a lone continuation byte: a legal Unix environment byte and
        // not valid UTF-8. `std::env::vars` panics on it, which would take the
        // launcher down at startup, before it filters anything or spawns.
        let invalid = || OsString::from_vec(vec![b'\x80']);
        let environment = filter_utf8_environment([
            (OsString::from("PATH"), OsString::from("/usr/bin")),
            (OsString::from("BAD_VALUE"), invalid()),
            (invalid(), OsString::from("value")),
        ]);

        assert_eq!(
            environment.get("PATH").map(String::as_str),
            Some("/usr/bin")
        );
        assert!(
            !environment.contains_key("BAD_VALUE"),
            "a non-UTF-8 value must be skipped, not carried: {environment:?}"
        );
        assert_eq!(
            environment.len(),
            1,
            "a non-UTF-8 name must be skipped too: {environment:?}"
        );
    }
}
