//! Command line for the bundled launcher.
//!
//! Both modes take their configuration in argv, which is readable by `ps` and
//! by any crash dump — so argv carries names, never values: `--secret
//! NAME=mcp:<id>` names a reference, and `--set NAME=VALUE` carries only
//! values the registry's sentinel scan already cleared as non-credential.

use std::collections::BTreeMap;

use buzz_secret_store::McpSecretRef;
use clap::{Args, Parser, Subcommand};

/// Default keychain service name, matching the desktop's.
pub const DEFAULT_KEYCHAIN_SERVICE: &str = "buzz-desktop";

/// Largest number of `--set` and `--secret` pairs one server may declare.
///
/// A generated config is machine-written, but the launcher is a boundary all
/// the same: an unbounded pair count is an unbounded environment.
pub const MAX_ENV_PAIRS: usize = 64;

/// Largest accepted `--set` value, in bytes.
pub const MAX_ENV_VALUE_LEN: usize = 8 * 1024;

/// Largest accepted environment variable name, in bytes.
pub const MAX_ENV_NAME_LEN: usize = 128;

/// `buzz-mcp-launch` — start a registry MCP server, or proxy one over HTTPS.
#[derive(Debug, Parser)]
#[command(name = "buzz-mcp-launch", version, about, long_about = None)]
pub struct Cli {
    /// Keychain service holding the secret blob.
    #[arg(long, default_value = DEFAULT_KEYCHAIN_SERVICE, global = true)]
    pub service: String,

    #[command(subcommand)]
    pub mode: Mode,
}

/// The two modes the one binary runs in.
#[derive(Debug, Subcommand)]
pub enum Mode {
    /// Start a stdio server with an environment built from empty.
    Launch(LaunchArgs),
    /// Serve stdio MCP in front of a Streamable HTTP upstream.
    Proxy(ProxyArgs),
}

/// Launcher-mode arguments.
#[derive(Debug, Args)]
pub struct LaunchArgs {
    /// Registry name of the server, for log lines.
    #[arg(long)]
    pub server: String,

    /// `NAME=VALUE` pair added to the child environment verbatim.
    #[arg(long = "set", value_name = "NAME=VALUE")]
    pub set: Vec<String>,

    /// `NAME=mcp:<id>` pair whose value is resolved from the secret store.
    #[arg(long = "secret", value_name = "NAME=mcp:ID")]
    pub secret: Vec<String>,

    /// Absolute path of the server executable, then its arguments.
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

/// Proxy-mode arguments.
#[derive(Debug, Args)]
pub struct ProxyArgs {
    /// Streamable HTTP upstream. `https` only, except on a loopback host.
    #[arg(long)]
    pub url: String,

    /// Auth scheme: `bearer`, `token`, `basic` or `api-key`.
    #[arg(long)]
    pub auth_scheme: Option<String>,

    /// `mcp:<id>` reference for the credential this upstream needs.
    #[arg(long)]
    pub secret: Option<String>,
}

/// Why the argv could not be turned into a configuration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CliError {
    /// A `--set` or `--secret` argument is not a `NAME=VALUE` pair.
    #[error("`{0}` is not a NAME=VALUE pair")]
    NotAPair(String),
    /// An environment variable name is empty, over the cap, or not
    /// `[A-Za-z_][A-Za-z0-9_]*`.
    #[error("`{0}` is not a valid environment variable name")]
    InvalidName(String),
    /// A value is over [`MAX_ENV_VALUE_LEN`].
    #[error("value for `{name}` is {len} bytes, over the {MAX_ENV_VALUE_LEN}-byte cap")]
    ValueTooLong {
        /// The variable name.
        name: String,
        /// The value length.
        len: usize,
    },
    /// More than [`MAX_ENV_PAIRS`] pairs were declared.
    #[error("{0} environment pairs declared, over the {MAX_ENV_PAIRS} cap")]
    TooManyPairs(usize),
    /// The same name appears twice, so which one wins would be argv order.
    #[error("`{0}` is declared more than once")]
    DuplicateName(String),
    /// A `--secret` value is not a valid `mcp:` reference.
    #[error("secret for `{name}`: {source}")]
    BadReference {
        /// The variable name.
        name: String,
        /// The reference error.
        source: buzz_secret_store::ReferenceError,
    },
}

/// The literal and reference-valued halves of one server's declared `env`.
pub type DeclaredEnv = (BTreeMap<String, String>, BTreeMap<String, McpSecretRef>);

/// Split and validate the `--set` and `--secret` pairs.
///
/// # Errors
/// The [`CliError`] naming the first pair that failed. Both lists share one cap
/// and one namespace, so a name cannot be declared literal in one and secret in
/// the other.
pub fn parse_env_pairs(set: &[String], secret: &[String]) -> Result<DeclaredEnv, CliError> {
    let total = set.len() + secret.len();
    if total > MAX_ENV_PAIRS {
        return Err(CliError::TooManyPairs(total));
    }

    let mut literals = BTreeMap::new();
    let mut references = BTreeMap::new();
    let mut seen = std::collections::BTreeSet::new();

    for raw in set {
        let (name, value) = split_pair(raw)?;
        if value.len() > MAX_ENV_VALUE_LEN {
            return Err(CliError::ValueTooLong {
                name,
                len: value.len(),
            });
        }
        if !seen.insert(name.clone()) {
            return Err(CliError::DuplicateName(name));
        }
        literals.insert(name, value);
    }
    for raw in secret {
        let (name, value) = split_pair(raw)?;
        let reference = McpSecretRef::parse(&value).map_err(|source| CliError::BadReference {
            name: name.clone(),
            source,
        })?;
        if !seen.insert(name.clone()) {
            return Err(CliError::DuplicateName(name));
        }
        references.insert(name, reference);
    }
    Ok((literals, references))
}

fn split_pair(raw: &str) -> Result<(String, String), CliError> {
    let (name, value) = raw
        .split_once('=')
        .ok_or_else(|| CliError::NotAPair(raw.to_string()))?;
    if !is_valid_env_name(name) {
        return Err(CliError::InvalidName(name.to_string()));
    }
    Ok((name.to_string(), value.to_string()))
}

fn is_valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_ENV_NAME_LEN
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn pairs_are_split_and_validated() {
        let (literals, references) = parse_env_pairs(
            &owned(&["GITHUB_HOST=github.example"]),
            &owned(&["GITHUB_TOKEN=mcp:github-token"]),
        )
        .expect("valid pairs");
        assert_eq!(
            literals.get("GITHUB_HOST").map(String::as_str),
            Some("github.example")
        );
        assert_eq!(
            references.get("GITHUB_TOKEN").map(McpSecretRef::id),
            Some("github-token")
        );
    }

    #[test]
    fn every_pair_bound_is_enforced() {
        assert_eq!(
            parse_env_pairs(&owned(&["NOEQUALS"]), &[]),
            Err(CliError::NotAPair("NOEQUALS".to_string()))
        );
        assert_eq!(
            parse_env_pairs(&owned(&["1BAD=x"]), &[]),
            Err(CliError::InvalidName("1BAD".to_string()))
        );
        let long_value = format!("A={}", "v".repeat(MAX_ENV_VALUE_LEN + 1));
        assert_eq!(
            parse_env_pairs(&owned(&[&long_value]), &[]),
            Err(CliError::ValueTooLong {
                name: "A".to_string(),
                len: MAX_ENV_VALUE_LEN + 1,
            })
        );
        let many: Vec<String> = (0..=MAX_ENV_PAIRS).map(|i| format!("A{i}=v")).collect();
        assert_eq!(
            parse_env_pairs(&many, &[]),
            Err(CliError::TooManyPairs(MAX_ENV_PAIRS + 1))
        );
        assert_eq!(
            parse_env_pairs(&owned(&["A=1"]), &owned(&["A=mcp:token"])),
            Err(CliError::DuplicateName("A".to_string()))
        );
        assert!(matches!(
            parse_env_pairs(&[], &owned(&["A=identity"])),
            Err(CliError::BadReference { .. })
        ));
    }

    #[test]
    fn the_parser_accepts_both_modes() {
        use clap::Parser as _;
        let cli = Cli::try_parse_from([
            "buzz-mcp-launch",
            "launch",
            "--server",
            "github",
            "--set",
            "A=1",
            "--",
            "/usr/local/bin/server",
            "--stdio",
        ])
        .expect("launch mode parses");
        assert_eq!(cli.service, DEFAULT_KEYCHAIN_SERVICE);
        let Mode::Launch(args) = cli.mode else {
            panic!("expected launch mode");
        };
        assert_eq!(args.command, owned(&["/usr/local/bin/server", "--stdio"]));

        let cli = Cli::try_parse_from([
            "buzz-mcp-launch",
            "proxy",
            "--url",
            "https://api.example.com/mcp",
            "--auth-scheme",
            "bearer",
            "--secret",
            "mcp:api-token",
        ])
        .expect("proxy mode parses");
        let Mode::Proxy(args) = cli.mode else {
            panic!("expected proxy mode");
        };
        assert_eq!(args.url, "https://api.example.com/mcp");
    }
}
