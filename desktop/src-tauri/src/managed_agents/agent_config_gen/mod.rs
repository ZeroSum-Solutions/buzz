//! Generation of a managed agent's extra MCP servers and pinned skills as
//! runtime configuration files.
//!
//! This is a *pure generator*: it validates a caller-supplied spec, renders it
//! to the file format each runtime reads, and writes it under a caller-supplied
//! root. It is deliberately not wired into the spawn path.
//!
//! # Where the files go
//!
//! * **Claude** reads a project `.mcp.json` from the directory the harness was
//!   spawned in. Managed agents are spawned with
//!   `current_dir(default_agent_workdir())` (`managed_agents/runtime.rs:564`).
//!   That function falls back to `$HOME` when the nest is missing;
//!   [`claude_project_config_root`] deliberately does **not** — it returns the
//!   Buzz-owned nest (`~/.buzz`) only when it exists as a real directory, and
//!   `None` otherwise, so a missing or symlinked nest can never turn the
//!   operator's home directory into this generator's write root.
//! * **Codex** reads `<CODEX_HOME>/config.toml`
//!   (`config_bridge::codex::codex_config_path`).
//! * **Skills** are discovered per runtime under the catalog's `skill_dir`
//!   (`.claude/skills`, `.codex/skills`; `discovery/catalog.rs`), relative to
//!   the same spawned working directory — the same placement `nest.rs` already
//!   uses for the bundled `buzz-cli` skill.
//!
//! # What this module will not do
//!
//! * It never resolves a path from the environment. Every write goes under a
//!   root the caller passes in, so the operator's own `~/.claude.json` and
//!   `~/.codex` are unreachable from here. [`plan_claude_paths`] and
//!   [`plan_codex_paths`] report the exact set a write would touch.
//! * It never follows a symlink into a directory it did not create, and it
//!   never changes the permissions of a directory that already existed — see
//!   the "Symlinks and permissions" section of [`mod@write`].
//! * It never writes a literal environment value. A server that needs a
//!   credential names the variable it wants in
//!   [`McpTransport::Stdio::env_passthrough`], and each runtime is given the
//!   form *it* forwards by: Claude gets a `"${NAME}"` placeholder it expands at
//!   spawn time, Codex gets `env_vars = ["NAME"]`, its list of names to forward
//!   from its own environment. Codex reads `env` values literally and
//!   interpolates nothing, so a placeholder written there would reach the
//!   server verbatim. Either way no secret reaches the file: every value in a
//!   generated file is either operator-chosen structure (a command, an
//!   argument, a URL), a placeholder, or a variable name. The agent's own identity variables are
//!   refused outright ([`DENIED_ENV_NAMES`]): forwarding one would hand an
//!   arbitrary MCP child the key the agent signs relay events with.
//! * A custom `CLAUDE_CONFIG_DIR` / `CODEX_HOME` starts a fresh keychain
//!   namespace and leaves the CLI logged out unless
//!   `CLAUDE_SECURESTORAGE_CONFIG_DIR` is managed too
//!   (`config_bridge/types.rs`). That is why the Claude side targets a project
//!   file in the working directory rather than a private config dir, and why
//!   the Codex side is a generator only until a per-agent `CODEX_HOME` has been
//!   shown to keep a Dock-launched agent logged in.

mod emit;
mod write;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

pub use emit::{render_claude_mcp_json, render_codex_config_toml};
pub use write::{
    plan_claude_paths, plan_codex_paths, write_claude_project_config, write_codex_config,
};

use std::collections::BTreeMap;

/// Largest number of MCP servers one generated config may declare.
///
/// Bounds the quantity that costs: each entry is one child process the runtime
/// spawns. Matches `buzz-agent`'s own 16-server ceiling (`crates/buzz-acp/README.md`).
pub const MAX_SERVERS: usize = 16;
/// Largest number of pinned skills one generated config may install.
///
/// Bounds directory entries created under the runtime's skill directory.
pub const MAX_SKILLS: usize = 32;
/// Largest number of command-line arguments one server entry may carry.
pub const MAX_ARGS: usize = 32;
/// Largest number of environment variable names one server entry may forward.
pub const MAX_ENV_PASSTHROUGH: usize = 32;

/// Byte ceiling on a server or skill name.
///
/// 32 bytes is `buzz-acp`'s sanitized server-name cap, budgeted against
/// `buzz-agent`'s 64-byte `<server>__<tool>` limit.
pub const MAX_NAME_BYTES: usize = 32;
/// Byte ceiling on a server's executable path or command word.
pub const MAX_COMMAND_BYTES: usize = 1024;
/// Byte ceiling on one command-line argument.
pub const MAX_ARG_BYTES: usize = 1024;
/// Byte ceiling on an HTTP server URL.
pub const MAX_URL_BYTES: usize = 2048;
/// Byte ceiling on one forwarded environment variable name.
pub const MAX_ENV_NAME_BYTES: usize = 128;
/// Byte ceiling on a pinned skill's `SKILL.md` body.
pub const MAX_SKILL_BODY_BYTES: usize = 64 * 1024;

/// Skill names the nest already owns. Writing one would clobber the
/// `buzz-cli` skill symlink `nest.rs` maintains under every runtime's skill
/// directory.
///
/// Compared case-insensitively: the name becomes a directory name, and APFS
/// and NTFS are case-insensitive, so `BUZZ-CLI` and `buzz-cli` are the same
/// directory on the filesystems this ships on.
const RESERVED_SKILL_NAMES: &[&str] = &["buzz-cli"];

/// Environment variable names [`McpTransport::Stdio::env_passthrough`] refuses.
///
/// These four are the managed agent's own identity: `runtime.rs` sets
/// `BUZZ_PRIVATE_KEY` and `BUZZ_RELAY_URL` on every spawn, `NOSTR_PRIVATE_KEY`
/// mirrors the key, and `BUZZ_AUTH_TAG` scopes it. A generated config that
/// forwarded one would expand it from the runtime's own environment into an
/// arbitrary stdio MCP child, which could then sign as the agent on the relay.
/// `crates/buzz-acp/README.md` withholds exactly these four from untrusted
/// extra servers; this generator refuses to name them at all.
///
/// Compared case-insensitively, so a case variant cannot slip past the list.
pub const DENIED_ENV_NAMES: &[&str] = &[
    "BUZZ_PRIVATE_KEY",
    "NOSTR_PRIVATE_KEY",
    "BUZZ_RELAY_URL",
    "BUZZ_AUTH_TAG",
];

/// Why a spec was rejected, or a write failed.
///
/// No variant carries a caller-supplied value. Names, arguments and
/// environment values may hold operator or relay text, and an error string is
/// logged and surfaced; the variants name the *field* and its index instead, so
/// nothing echoes back out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigGenError {
    /// More entries than the field's ceiling allows.
    TooMany {
        /// The field that overflowed.
        field: &'static str,
        /// The ceiling.
        limit: usize,
        /// How many entries were supplied.
        got: usize,
    },
    /// A string longer than the field's byte ceiling.
    TooLong {
        /// The field that overflowed.
        field: &'static str,
        /// Index of the offending entry within its collection.
        index: usize,
        /// The ceiling, in bytes.
        limit: usize,
        /// The supplied length, in bytes.
        got: usize,
    },
    /// A string that is empty, or holds a character the field does not allow.
    Invalid {
        /// The field that failed validation.
        field: &'static str,
        /// Index of the offending entry within its collection.
        index: usize,
        /// What the field does allow.
        expected: &'static str,
    },
    /// Two entries in the same collection resolved to the same name.
    Duplicate {
        /// The field that collided.
        field: &'static str,
        /// Index of the second entry with the name.
        index: usize,
    },
    /// The runtime id is not in the known-ACP-runtime catalog, or the catalog
    /// entry declares no skill directory.
    UnknownRuntime {
        /// The runtime ids this generator supports.
        supported: &'static str,
    },
    /// A filesystem operation failed. Carries the path it failed on and the
    /// operating system's message, so no failure is swallowed.
    Io {
        /// The path the operation targeted.
        path: String,
        /// The underlying error, rendered.
        source: String,
    },
}

impl std::fmt::Display for ConfigGenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooMany { field, limit, got } => {
                write!(f, "{field}: {got} entries exceeds the limit of {limit}")
            }
            Self::TooLong {
                field,
                index,
                limit,
                got,
            } => write!(
                f,
                "{field}[{index}]: {got} bytes exceeds the limit of {limit}"
            ),
            Self::Invalid {
                field,
                index,
                expected,
            } => write!(f, "{field}[{index}]: expected {expected}"),
            Self::Duplicate { field, index } => {
                write!(f, "{field}[{index}]: duplicate name")
            }
            Self::UnknownRuntime { supported } => {
                write!(f, "unknown runtime; supported runtimes: {supported}")
            }
            Self::Io { path, source } => write!(f, "{path}: {source}"),
        }
    }
}

impl std::error::Error for ConfigGenError {}

/// The runtimes this generator emits configuration for.
pub const SUPPORTED_RUNTIMES: &str = "claude, codex";

/// How a runtime reaches one MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpTransport {
    /// A local child process speaking MCP over stdio.
    Stdio {
        /// The executable to run.
        command: String,
        /// Its arguments, in order.
        args: Vec<String>,
        /// Environment variable names to forward. Emitted as `"${NAME}"`
        /// placeholders for Claude and as `env_vars` names for Codex; the
        /// generator never writes a value.
        env_passthrough: Vec<String>,
    },
    /// A remote server reached over HTTP, the shape OpenSEO's published
    /// `plugins/openseo` manifests use.
    Http {
        /// The server endpoint.
        url: String,
    },
}

/// One validated MCP server entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerSpec {
    name: String,
    transport: McpTransport,
}

impl McpServerSpec {
    /// Validates and builds a stdio server entry.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigGenError`] when the name, command, arguments or
    /// forwarded environment names break a ceiling or a charset rule.
    pub fn stdio(
        name: &str,
        command: &str,
        args: &[String],
        env_passthrough: &[String],
    ) -> Result<Self, ConfigGenError> {
        check_name("server.name", 0, name)?;
        check_len("server.command", 0, command, MAX_COMMAND_BYTES)?;
        if command.trim().is_empty() {
            return Err(ConfigGenError::Invalid {
                field: "server.command",
                index: 0,
                expected: "a non-empty command",
            });
        }
        check_count("server.args", args.len(), MAX_ARGS)?;
        for (index, arg) in args.iter().enumerate() {
            check_len("server.args", index, arg, MAX_ARG_BYTES)?;
        }
        check_count(
            "server.env_passthrough",
            env_passthrough.len(),
            MAX_ENV_PASSTHROUGH,
        )?;
        let mut seen = std::collections::BTreeSet::new();
        for (index, key) in env_passthrough.iter().enumerate() {
            check_env_name("server.env_passthrough", index, key)?;
            // Environment variable names are case-sensitive on Unix, so the
            // duplicate check is exact here; the deny-list above is not.
            if !seen.insert(key.as_str()) {
                return Err(ConfigGenError::Duplicate {
                    field: "server.env_passthrough",
                    index,
                });
            }
        }
        Ok(Self {
            name: name.to_string(),
            transport: McpTransport::Stdio {
                command: command.to_string(),
                args: args.to_vec(),
                env_passthrough: env_passthrough.to_vec(),
            },
        })
    }

    /// Validates and builds an HTTP server entry.
    ///
    /// The URL is parsed rather than prefix-matched, because a prefix match
    /// accepts strings that are not usable endpoints and are dangerous to
    /// store: `http://` with no host, and — worse — `https://user:token@host/`,
    /// which puts a credential in a config file this generator otherwise never
    /// writes a secret to. Plaintext `http://` is confined to loopback, so a
    /// generated config cannot send MCP traffic over the network in the clear.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigGenError`] when the name or URL breaks a ceiling or a
    /// charset rule, when the URL does not parse, when its scheme is not
    /// `http`/`https`, when it carries userinfo or a fragment, when it has no
    /// host, or when it is plaintext `http` to a non-loopback host.
    pub fn http(name: &str, url: &str) -> Result<Self, ConfigGenError> {
        check_name("server.name", 0, name)?;
        check_len("server.url", 0, url, MAX_URL_BYTES)?;
        check_http_url(url)?;
        Ok(Self {
            name: name.to_string(),
            transport: McpTransport::Http {
                url: url.to_string(),
            },
        })
    }

    /// The server's name, as it appears as a key in the generated config.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// How the runtime reaches this server.
    #[must_use]
    pub fn transport(&self) -> &McpTransport {
        &self.transport
    }
}

/// One pinned skill: a directory name and the `SKILL.md` body to place in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedSkill {
    name: String,
    body: String,
}

impl PinnedSkill {
    /// Validates and builds a pinned skill.
    ///
    /// The name becomes a directory name, so its charset excludes `.` and any
    /// path separator: a traversing name cannot be constructed. Names the nest
    /// already owns (`buzz-cli`) are refused so a generated skill cannot
    /// clobber the bundled one.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigGenError`] when the name breaks the charset rule or the
    /// body exceeds [`MAX_SKILL_BODY_BYTES`].
    pub fn new(name: &str, body: &str) -> Result<Self, ConfigGenError> {
        check_name("skill.name", 0, name)?;
        if RESERVED_SKILL_NAMES
            .iter()
            .any(|reserved| reserved.eq_ignore_ascii_case(name))
        {
            return Err(ConfigGenError::Invalid {
                field: "skill.name",
                index: 0,
                expected: "a name the nest does not already own",
            });
        }
        check_len("skill.body", 0, body, MAX_SKILL_BODY_BYTES)?;
        if body.is_empty() {
            return Err(ConfigGenError::Invalid {
                field: "skill.body",
                index: 0,
                expected: "a non-empty SKILL.md body",
            });
        }
        Ok(Self {
            name: name.to_string(),
            body: body.to_string(),
        })
    }

    /// The skill's directory name under the runtime's skill directory.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The `SKILL.md` body.
    #[must_use]
    pub fn body(&self) -> &str {
        &self.body
    }
}

/// A validated set of extra MCP servers and pinned skills for one agent.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentRuntimeConfigSpec {
    servers: Vec<McpServerSpec>,
    skills: Vec<PinnedSkill>,
}

impl AgentRuntimeConfigSpec {
    /// Validates counts and cross-entry uniqueness and builds the spec.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigGenError`] when either collection exceeds its ceiling or
    /// two entries share a name.
    pub fn new(
        servers: Vec<McpServerSpec>,
        skills: Vec<PinnedSkill>,
    ) -> Result<Self, ConfigGenError> {
        check_count("servers", servers.len(), MAX_SERVERS)?;
        check_count("skills", skills.len(), MAX_SKILLS)?;
        // Case-folded on both collections. A skill name becomes a directory
        // name on a case-insensitive filesystem (APFS, NTFS), where `Audit` and
        // `audit` are one directory and the second write would silently
        // overwrite the first; a server name is a config key, and two entries
        // differing only in case collide in `<server>__<tool>` downstream.
        let mut seen_servers = std::collections::BTreeSet::new();
        for (index, server) in servers.iter().enumerate() {
            if !seen_servers.insert(server.name.to_ascii_lowercase()) {
                return Err(ConfigGenError::Duplicate {
                    field: "servers",
                    index,
                });
            }
        }
        let mut seen_skills = std::collections::BTreeSet::new();
        for (index, skill) in skills.iter().enumerate() {
            if !seen_skills.insert(skill.name.to_ascii_lowercase()) {
                return Err(ConfigGenError::Duplicate {
                    field: "skills",
                    index,
                });
            }
        }
        Ok(Self { servers, skills })
    }

    /// The validated MCP server entries.
    #[must_use]
    pub fn servers(&self) -> &[McpServerSpec] {
        &self.servers
    }

    /// The validated pinned skills.
    #[must_use]
    pub fn skills(&self) -> &[PinnedSkill] {
        &self.skills
    }

    /// The servers keyed by name, in the deterministic order the generated
    /// files use.
    pub(super) fn servers_by_name(&self) -> BTreeMap<&str, &McpTransport> {
        self.servers
            .iter()
            .map(|s| (s.name.as_str(), &s.transport))
            .collect()
    }
}

/// The directory a managed agent's Claude project `.mcp.json` belongs in: the
/// Buzz nest (`~/.buzz`), which is also the working directory the spawn path
/// gives every managed agent (`managed_agents/runtime.rs:564`).
///
/// Returns `None` unless the nest exists as a real directory. This is
/// deliberately narrower than `default_agent_workdir`, which falls back to
/// `$HOME` when the nest is missing or is a symlink: that fallback would make
/// this generator write `$HOME/.mcp.json` and `$HOME/.claude/skills/…` into the
/// operator's own home directory. A caller that gets `None` writes nothing.
///
/// The directory is shared by every managed agent — `runtime.rs` gives them all
/// the same working directory — so a file written here is visible to all of
/// them, not to one named agent. Per-agent scope is not achievable through this
/// root; a caller needing it must supply its own and arrange for the runtime to
/// be spawned there.
#[must_use]
pub fn claude_project_config_root() -> Option<std::path::PathBuf> {
    buzz_owned_root(super::nest_dir())
}

/// Keeps a candidate root only when it exists as a real directory.
///
/// Split out from [`claude_project_config_root`] so the rule is testable
/// without a home directory: `symlink_metadata` does not follow the final
/// component, so a symlinked nest is rejected rather than resolved.
fn buzz_owned_root(candidate: Option<std::path::PathBuf>) -> Option<std::path::PathBuf> {
    candidate.filter(|p| {
        p.symlink_metadata()
            .map(|m| m.file_type().is_dir())
            .unwrap_or(false)
    })
}

/// The skill directory a runtime discovers, relative to its working directory,
/// read from the known-ACP-runtime catalog (`discovery/catalog.rs`).
///
/// # Errors
///
/// Returns [`ConfigGenError::UnknownRuntime`] when the id is not in the catalog
/// or its entry declares no skill directory.
pub fn runtime_skill_dir(runtime_id: &str) -> Result<&'static str, ConfigGenError> {
    super::discovery::known_acp_runtime_exact(runtime_id)
        .and_then(|r| r.skill_dir)
        .ok_or(ConfigGenError::UnknownRuntime {
            supported: SUPPORTED_RUNTIMES,
        })
}

fn check_count(field: &'static str, got: usize, limit: usize) -> Result<(), ConfigGenError> {
    if got > limit {
        return Err(ConfigGenError::TooMany { field, limit, got });
    }
    Ok(())
}

fn check_len(
    field: &'static str,
    index: usize,
    value: &str,
    limit: usize,
) -> Result<(), ConfigGenError> {
    if value.len() > limit {
        return Err(ConfigGenError::TooLong {
            field,
            index,
            limit,
            got: value.len(),
        });
    }
    Ok(())
}

/// A server or skill name: 1..=[`MAX_NAME_BYTES`] bytes of ASCII alphanumerics,
/// `-` or `_`, starting with an alphanumeric.
///
/// The charset excludes `.`, `/` and `\`, so a name can never traverse out of
/// the directory it is joined onto, and it is a bare TOML key and a plain JSON
/// object key without quoting surprises.
fn check_name(field: &'static str, index: usize, value: &str) -> Result<(), ConfigGenError> {
    check_len(field, index, value, MAX_NAME_BYTES)?;
    let valid = !value.is_empty()
        && value.starts_with(|c: char| c.is_ascii_alphanumeric())
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !valid {
        return Err(ConfigGenError::Invalid {
            field,
            index,
            expected: "ASCII alphanumerics, '-' or '_', starting with an alphanumeric",
        });
    }
    Ok(())
}

/// An environment variable name: 1..=[`MAX_ENV_NAME_BYTES`] bytes of ASCII
/// alphanumerics or `_`, not starting with a digit, and not one of
/// [`DENIED_ENV_NAMES`].
fn check_env_name(field: &'static str, index: usize, value: &str) -> Result<(), ConfigGenError> {
    check_len(field, index, value, MAX_ENV_NAME_BYTES)?;
    let valid = !value.is_empty()
        && !value.starts_with(|c: char| c.is_ascii_digit())
        && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid {
        return Err(ConfigGenError::Invalid {
            field,
            index,
            expected: "ASCII alphanumerics or '_', not starting with a digit",
        });
    }
    if DENIED_ENV_NAMES
        .iter()
        .any(|denied| denied.eq_ignore_ascii_case(value))
    {
        return Err(ConfigGenError::Invalid {
            field,
            index,
            expected: "a variable that is not the agent's own relay identity \
                       (BUZZ_PRIVATE_KEY, NOSTR_PRIVATE_KEY, BUZZ_RELAY_URL, BUZZ_AUTH_TAG)",
        });
    }
    Ok(())
}

/// An MCP endpoint URL: parsed, not prefix-matched.
///
/// A prefix match accepts `http://` on its own, `https://user:token@host/mcp`
/// and `https://host/mcp#anchor`. The first is not an endpoint, the second
/// smuggles a credential into a generated file, and the third is a fragment the
/// server never sees. Plaintext `http` is confined to loopback so a generated
/// config cannot put MCP traffic on the network in the clear.
fn check_http_url(url: &str) -> Result<(), ConfigGenError> {
    let invalid = |expected: &'static str| ConfigGenError::Invalid {
        field: "server.url",
        index: 0,
        expected,
    };
    let parsed = url::Url::parse(url).map_err(|_| invalid("a parseable absolute URL"))?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(invalid("an http:// or https:// URL"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(invalid(
            "a URL with no userinfo; a credential must not be stored in a config file",
        ));
    }
    if parsed.fragment().is_some() {
        return Err(invalid("a URL with no '#' fragment"));
    }
    let Some(host) = parsed.host() else {
        return Err(invalid("a URL with a host"));
    };
    if scheme == "http" && !is_loopback_host(&host) {
        return Err(invalid(
            "https://, unless the host is loopback (127.0.0.1, ::1, localhost)",
        ));
    }
    Ok(())
}

/// Whether a parsed URL host is loopback, and so safe to reach over plaintext.
fn is_loopback_host(host: &url::Host<&str>) -> bool {
    match host {
        url::Host::Domain(name) => name.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(addr) => addr.is_loopback(),
        url::Host::Ipv6(addr) => addr.is_loopback(),
    }
}
