//! Credential-shaped string detection.
//!
//! Memo decisions 5 and 7: an entry's argv and its URL query string are scanned
//! for sentinel credential patterns and the entry is refused when one matches,
//! so no credential is written to a generated file, into a command line `ps`
//! can read, or into a crash dump. It lives beside the secret vocabulary
//! because the registry loader and the HTTP proxy must agree on exactly one
//! definition of "this looks like a credential".

use crate::namespace::MCP_NAMESPACE_PREFIX;

/// Vendor key prefixes that are credentials by construction.
const KEY_PREFIXES: &[&str] = &[
    "sk-",
    "sk_live_",
    "sk_test_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xapp-",
    "AKIA",
    "ASIA",
    "glpat-",
    "AIza",
    "ya29.",
    "hf_",
    "npm_",
    "dop_v1_",
    "sq0atp-",
    "sq0csp-",
    "nsec1",
    "eyJ",
];

/// Parameter-name stems that mark the value beside them as a credential.
const CREDENTIAL_NAMES: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "apikey",
    "api_key",
    "api-key",
    "accesskey",
    "access_key",
    "access-key",
    "credential",
    "auth",
    "bearer",
    "session",
    "cookie",
    "privatekey",
    "private_key",
];

/// Why a string was judged credential-shaped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentinelHit {
    /// A short, operator-facing reason, safe to render in a status string.
    /// It names the pattern, never the matched value.
    pub reason: String,
}

/// Whether `name` reads as the name of a credential.
pub fn is_credential_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    CREDENTIAL_NAMES.iter().any(|stem| lower.contains(stem))
}

/// Scan one free-form string (an argv element, a query value) for a credential.
///
/// Returns `None` for an `mcp:` reference: a reference is a name, which is
/// exactly what an operator is supposed to write instead of a value.
pub fn scan_value(value: &str) -> Option<SentinelHit> {
    if value.starts_with(MCP_NAMESPACE_PREFIX) {
        return None;
    }
    // Match a vendor prefix at the START of a token, never anywhere inside the
    // string: `value.contains("sk-")` would refuse `--disk-cache`, and an entry
    // refused for a false positive is a feature the operator cannot use.
    for token in value.split([' ', '\t', '=', ':', ',', ';', '"', '\'']) {
        if let Some(prefix) = KEY_PREFIXES
            .iter()
            .find(|prefix| token.starts_with(*prefix) && token.len() > prefix.len())
        {
            return Some(SentinelHit {
                reason: format!("carries a credential with the `{prefix}` prefix"),
            });
        }
    }
    // `--token=abc`, `TOKEN=abc`, `password:abc` inside one argv element. Only
    // the FIRST separator splits: `--token=mcp:ref` must read as the name
    // `--token` with a reference value, not as the name `--token=mcp`.
    if let Some((name, rest)) = value.split_once(['=', ':']) {
        if !rest.is_empty() && !rest.starts_with(MCP_NAMESPACE_PREFIX) && is_credential_name(name) {
            let stem = name.trim_start_matches('-');
            return Some(SentinelHit {
                reason: format!("carries an inline value for `{stem}`"),
            });
        }
    }
    None
}

/// Scan an argv vector, returning the first hit and the index it was found at.
pub fn scan_argv(argv: &[String]) -> Option<(usize, SentinelHit)> {
    for (index, arg) in argv.iter().enumerate() {
        if let Some(hit) = scan_value(arg) {
            return Some((index, hit));
        }
        // `--token abc` splits the name and the value across two elements.
        if is_credential_name(arg.trim_start_matches('-')) && arg.starts_with('-') {
            if let Some(next) = argv.get(index + 1) {
                if !next.is_empty()
                    && !next.starts_with('-')
                    && !next.starts_with(MCP_NAMESPACE_PREFIX)
                {
                    return Some((
                        index,
                        SentinelHit {
                            reason: format!(
                                "carries a value for `{}` in its arguments",
                                arg.trim_start_matches('-')
                            ),
                        },
                    ));
                }
            }
        }
    }
    None
}

/// Scan the `key=value` pairs of a URL query string.
pub fn scan_query_pairs<'a, I>(pairs: I) -> Option<SentinelHit>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    for (name, value) in pairs {
        if value.is_empty() || value.starts_with(MCP_NAMESPACE_PREFIX) {
            continue;
        }
        if is_credential_name(name) {
            return Some(SentinelHit {
                reason: format!("carries a credential in the `{name}` query parameter"),
            });
        }
        if let Some(hit) = scan_value(value) {
            return Some(hit);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn vendor_key_shapes_are_caught_in_argv() {
        for value in [
            "sk-ant-api03-abcdef",
            "ghp_0123456789abcdef",
            "xoxb-1-2-3",
            "AKIAIOSFODNN7EXAMPLE",
            "nsec1qqqqqq",
            "eyJhbGciOiJIUzI1NiJ9.payload.signature",
            "Bearer sk-ant-api03-abcdef",
        ] {
            assert!(scan_value(value).is_some(), "{value} must be refused");
        }
    }

    #[test]
    fn a_vendor_prefix_inside_an_ordinary_word_is_not_a_credential() {
        // The guard matches a prefix at the start of a token only. Without that
        // rule `--disk-cache` (which contains `sk-`) would refuse a legitimate
        // server, and a refusal an operator cannot work around is a defect.
        for value in ["--disk-cache", "/opt/tasks-runner", "--no-ya29-mode"] {
            assert!(scan_value(value).is_none(), "{value} must pass");
        }
    }

    #[test]
    fn inline_and_split_credential_arguments_are_caught() {
        assert!(scan_value("--token=abc123").is_some());
        assert!(scan_value("API_KEY=abc123").is_some());
        assert!(scan_argv(&argv(["--token", "abc123"].as_slice())).is_some());
        assert!(scan_argv(&argv(["--password", "hunter2"].as_slice())).is_some());
    }

    #[test]
    fn references_and_ordinary_arguments_pass() {
        assert!(scan_value("mcp:github-token").is_none());
        assert!(scan_value("--token=mcp:github-token").is_none());
        assert!(scan_argv(&argv(["--token", "mcp:github-token"].as_slice())).is_none());
        assert!(scan_argv(&argv(["--stdio", "--project", "/srv/repo"].as_slice())).is_none());
        assert!(scan_argv(&argv(["--verbose"].as_slice())).is_none());
    }

    #[test]
    fn query_parameters_are_scanned_by_name_and_by_value() {
        assert!(scan_query_pairs([("access_token", "abc")]).is_some());
        assert!(scan_query_pairs([("q", "ghp_0123456789abcdef")]).is_some());
        assert!(scan_query_pairs([("q", "search"), ("page", "2")]).is_none());
        assert!(scan_query_pairs([("token", "mcp:github-token")]).is_none());
    }
}
