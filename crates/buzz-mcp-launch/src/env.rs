//! The child environment a registry stdio server receives (memo decision 1).
//!
//! The adapter that starts this launcher is spawned with no `env_clear` and
//! inherits the whole harness environment, provider keys included
//! (`crates/buzz-acp/src/acp.rs:478`,
//! `desktop/src-tauri/src/managed_agents/runtime.rs:693-695`). A server started
//! from that environment inherits it too. So the child environment here is
//! built from **empty**: one enumerated list, plus the entry's own approved
//! values, and nothing else.

use std::collections::{BTreeMap, BTreeSet};

use buzz_secret_store::CAPABILITY_ENV_VAR;

/// Platform variables a server may inherit, on every platform.
///
/// `PATH` is here for the child's own subprocesses. It is never used to resolve
/// the server command itself: a registry command must be an absolute path.
pub const BASE_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "TERM",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

/// Windows spellings of `HOME` and `TMPDIR`, plus the system root the CRT needs.
pub const WINDOWS_ALLOWLIST: &[&str] = &["USERPROFILE", "SystemRoot", "TEMP", "TMP"];

/// The eight proxy spellings. Both cases are needed: curl and git read the
/// lowercase ones, most Go and Python tooling the uppercase ones.
///
/// A proxy variable is the one entry on the list that can itself be a
/// credential, so it passes only when its value carries no userinfo.
pub const PROXY_ALLOWLIST: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "all_proxy",
];

/// Whether a proxy variable value carries userinfo (`scheme://user:pass@host`).
///
/// Deliberately conservative: it looks for an `@` in the authority, which is
/// the only place userinfo can appear, and treats an unparseable value as
/// carrying credentials rather than guessing.
pub fn proxy_value_carries_userinfo(value: &str) -> bool {
    let authority = match value.split_once("://") {
        Some((_, rest)) => rest,
        // A bare `host:port` form has no userinfo syntax at all, but an `@`
        // in it is still not something to forward blind.
        None => value,
    };
    let authority = authority.split(['/', '?', '#']).next().unwrap_or(authority);
    authority.contains('@')
}

/// Build the child environment for one stdio server.
///
/// `inherited` is this process's own environment. `approved_literals` are the
/// entry's non-secret `env` values and `resolved_secrets` the values resolved
/// from `mcp:` references; both win over the inherited allowlist on a name
/// clash, and both are applied after it.
///
/// The capability variable is stripped last, unconditionally: it authorizes
/// every secret bound to this agent, and the server must never see it.
pub fn build_child_env(
    inherited: &BTreeMap<String, String>,
    approved_literals: &BTreeMap<String, String>,
    resolved_secrets: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut allowed: BTreeSet<&str> = BASE_ALLOWLIST.iter().copied().collect();
    if cfg!(windows) {
        allowed.extend(WINDOWS_ALLOWLIST.iter().copied());
    }

    let mut child = BTreeMap::new();
    for name in allowed {
        if let Some(value) = inherited.get(name) {
            child.insert(name.to_string(), value.clone());
        }
    }
    for name in PROXY_ALLOWLIST {
        let Some(value) = inherited.get(*name) else {
            continue;
        };
        if proxy_value_carries_userinfo(value) {
            tracing::warn!(
                variable = name,
                "dropping proxy variable: its value carries userinfo; declare it as an approved `env` reference on the server entry to pass one"
            );
            continue;
        }
        child.insert((*name).to_string(), value.clone());
    }

    for (name, value) in approved_literals {
        child.insert(name.clone(), value.clone());
    }
    for (name, value) in resolved_secrets {
        child.insert(name.clone(), value.clone());
    }

    // Unconditional, and last: an approved entry cannot re-add it either.
    child.remove(CAPABILITY_ENV_VAR);
    child
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn child_environment_is_built_from_empty() {
        let inherited = env(&[
            ("PATH", "/usr/bin"),
            ("HOME", "/home/u"),
            ("ANTHROPIC_API_KEY", "sk-live-should-not-leak"),
            ("SSH_AUTH_SOCK", "/tmp/agent.sock"),
            ("GIT_ASKPASS", "/usr/bin/askpass"),
            ("XDG_CONFIG_HOME", "/home/u/.config"),
            ("BUZZ_PRIVATE_KEY", "nsec-should-not-leak"),
            ("NOSTR_PRIVATE_KEY", "nsec-should-not-leak"),
        ]);
        let child = build_child_env(&inherited, &BTreeMap::new(), &BTreeMap::new());
        assert_eq!(child.get("PATH").map(String::as_str), Some("/usr/bin"));
        assert_eq!(child.get("HOME").map(String::as_str), Some("/home/u"));
        for withheld in [
            "ANTHROPIC_API_KEY",
            "SSH_AUTH_SOCK",
            "GIT_ASKPASS",
            "XDG_CONFIG_HOME",
            "BUZZ_PRIVATE_KEY",
            "NOSTR_PRIVATE_KEY",
        ] {
            assert!(!child.contains_key(withheld), "{withheld} must not pass");
        }
    }

    #[test]
    fn authenticated_proxy_values_are_dropped_and_clean_ones_pass() {
        let inherited = env(&[
            ("HTTPS_PROXY", "http://employee:token@proxy.example"),
            ("https_proxy", "http://employee:token@proxy.example"),
            ("HTTP_PROXY", "http://proxy.example:3128"),
            ("NO_PROXY", "localhost,127.0.0.1"),
        ]);
        let child = build_child_env(&inherited, &BTreeMap::new(), &BTreeMap::new());
        assert!(!child.contains_key("HTTPS_PROXY"));
        assert!(!child.contains_key("https_proxy"));
        assert_eq!(
            child.get("HTTP_PROXY").map(String::as_str),
            Some("http://proxy.example:3128")
        );
        assert_eq!(
            child.get("NO_PROXY").map(String::as_str),
            Some("localhost,127.0.0.1")
        );
    }

    #[test]
    fn approved_values_win_and_secrets_win_over_those() {
        let inherited = env(&[("PATH", "/usr/bin"), ("HTTP_PROXY", "http://inherited")]);
        let approved = env(&[("PATH", "/opt/bin"), ("HTTP_PROXY", "http://approved")]);
        let secrets = env(&[("TOKEN", "resolved")]);
        let child = build_child_env(&inherited, &approved, &secrets);
        assert_eq!(child.get("PATH").map(String::as_str), Some("/opt/bin"));
        assert_eq!(
            child.get("HTTP_PROXY").map(String::as_str),
            Some("http://approved")
        );
        assert_eq!(child.get("TOKEN").map(String::as_str), Some("resolved"));
    }

    #[test]
    fn the_capability_is_stripped_even_when_re_declared() {
        let inherited = env(&[(CAPABILITY_ENV_VAR, "v1.agent-a.1.00")]);
        let approved = env(&[(CAPABILITY_ENV_VAR, "v1.agent-a.1.00")]);
        let secrets = env(&[(CAPABILITY_ENV_VAR, "v1.agent-a.1.00")]);
        let child = build_child_env(&inherited, &approved, &secrets);
        assert!(!child.contains_key(CAPABILITY_ENV_VAR));
    }

    #[test]
    fn userinfo_detection_covers_the_shapes_that_reach_it() {
        assert!(proxy_value_carries_userinfo("http://u:p@host:3128"));
        assert!(proxy_value_carries_userinfo("http://u@host"));
        assert!(proxy_value_carries_userinfo("u:p@host:3128"));
        assert!(!proxy_value_carries_userinfo("http://host:3128"));
        assert!(!proxy_value_carries_userinfo(
            "http://host/path@notuserinfo"
        ));
        assert!(!proxy_value_carries_userinfo("localhost,127.0.0.1"));
    }
}
