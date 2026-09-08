//! The `BUZZ_ACP_MCP_REGISTRY` handover file (memo decision 10).
//!
//! `BUZZ_ACP_EXTRA_MCP_COMMANDS` can only carry a server's credentials in its
//! argv, where `ps` and any crash dump can read them, so the registry hands
//! servers over as a file instead: a bounded JSON document naming each
//! server's command and args. Every generated command is `buzz-mcp-launch`,
//! which resolves the `mcp:` references in its own argv, so no resolved value
//! ever enters this process's address space or the ACP wire.
//!
//! Three things make the document trustworthy rather than merely well-formed,
//! because the marker this module sets is what makes `buzz-agent` hand a child
//! the capability that authorizes every one of the agent's secrets:
//!
//! 1. **The path is confined.** It must sit at the tail the desktop writes
//!    inside the adopted generation — `generations/<generation>/agents/<agent>/`
//!    — with the generation and the agent taken from the capability in this
//!    process's own environment, which only the spawn seam sets.
//! 2. **The open does not follow a symlink** and the result must be a regular
//!    file, so nothing can swap the confined path for a link elsewhere.
//! 3. **Every entry's command is the bundled launcher**, resolved beside this
//!    binary. A declared command must be a plain absolute path — no `.`, no
//!    `..`, the same shape the registry path itself must take — and must name
//!    that file; an entry naming anything else refuses startup rather than
//!    losing the marker silently. What the accepted entry carries onward is
//!    the **derived** launcher path, never the declared string, so the value
//!    `buzz-agent` execs under the session's own working directory is the
//!    value this check resolved.
//!
//! No entry may declare an `env` block. `buzz-agent` applies `McpServer.env` to
//! the **launcher's** own `Command`, so a declared `DYLD_INSERT_LIBRARIES` or
//! `LD_PRELOAD` would run inside the launcher, before `main`, while the
//! capability is in its environment. The desktop carries every declared
//! variable in the launcher's `--set`/`--secret` argv instead, and the launcher
//! applies those to the child after it strips the capability.
//!
//! This module only reads, confines and bounds the document. It resolves
//! nothing, and it is deliberately separate from the extras parser: both
//! produce `McpServer` entries and both are appended by
//! [`crate::build_mcp_servers`], so an operator can keep using
//! `BUZZ_ACP_EXTRA_MCP_COMMANDS` while the desktop writes this file.

use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use buzz_secret_store::AgentCapability;
use serde::Deserialize;

use crate::acp::McpServer;
use crate::{ConfigError, MAX_MCP_NAME_LEN, MAX_MCP_SERVERS};

/// Variable naming the registry file the desktop generated for this agent.
pub const MCP_REGISTRY_ENV_VAR: &str = "BUZZ_ACP_MCP_REGISTRY";

/// Schema version this build understands. A document declaring anything else
/// is refused rather than read on a guess.
pub const REGISTRY_FILE_VERSION: u32 = 1;

/// Largest accepted registry file, in bytes.
///
/// The desktop caps the operator-editable registry document at 256 KiB and
/// generates from it; the generated form is larger per entry, so this is the
/// generated bound, not a copy of the source one.
pub const MAX_REGISTRY_FILE_BYTES: usize = 1024 * 1024;

/// Largest accepted number of arguments on one server.
pub const MAX_REGISTRY_ARGS: usize = 64;

/// Largest accepted single argument, in bytes.
pub const MAX_REGISTRY_ARG_LEN: usize = 1024;

/// Largest accepted command, in bytes.
pub const MAX_REGISTRY_COMMAND_LEN: usize = 4 * 1024;

/// File name the desktop gives the handover document.
pub const REGISTRY_FILE_NAME: &str = "buzz-acp-registry.json";

/// Staging-tree directory holding one directory per generation.
///
/// The desktop's `GenerationStore::generation_dir` writes
/// `generations/<generation>`; the confinement check below requires the same
/// spelling, so a layout change on one side fails the other's tests rather
/// than widening what this side accepts.
pub const GENERATIONS_DIR: &str = "generations";

/// Directory holding one directory per agent inside a generation.
///
/// The desktop's `RegistryPaths::agent_dir` writes `agents/<agent id>`.
pub const AGENTS_DIR: &str = "agents";

/// Name of the bundled launcher, resolved beside this binary.
#[cfg(windows)]
pub const LAUNCHER_FILE_NAME: &str = "buzz-mcp-launch.exe";

/// Name of the bundled launcher, resolved beside this binary.
#[cfg(not(windows))]
pub const LAUNCHER_FILE_NAME: &str = "buzz-mcp-launch";

/// The bundled launcher's absolute path: the sibling of the running binary.
///
/// `buzz-acp` and `buzz-mcp-launch` are bundled into one directory
/// (`scripts/bundle-sidecars.sh`, `tauri.conf.json`) and built into one
/// `target/<profile>` in development, so the running binary's own directory is
/// the launcher's directory on every build. Deriving it here rather than
/// reading it from the environment is the point: an environment value could be
/// supplied by whatever supplied a hostile registry file.
///
/// # Errors
/// A message when the running binary's path or its parent cannot be resolved.
pub fn bundled_launcher() -> Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("this harness cannot resolve its own path, so it cannot verify that a registry entry names the bundled launcher: {e}"))?;
    let dir = exe.parent().ok_or_else(|| {
        format!(
            "this harness's path {} has no parent directory, so it cannot resolve the bundled launcher",
            exe.display()
        )
    })?;
    Ok(dir.join(LAUNCHER_FILE_NAME))
}

/// Whether `declared` and `expected` name the same file.
///
/// Both sides are canonicalized so a bundle symlink, a `//` or a relative
/// prefix does not read as a different binary. A side that cannot be
/// canonicalized — most often a path that does not exist — falls back to the
/// literal comparison, which is the conservative answer: it refuses rather
/// than accepts.
///
/// Passing this check is **not** what makes the entry safe to exec: the
/// declared string is never executed. [`parse_registry_file`] stores
/// [`launcher_command`]'s derived path instead, so a link swapped after this
/// call changes nothing about what runs.
fn same_file(declared: &str, expected: &Path) -> bool {
    match (
        std::fs::canonicalize(declared),
        std::fs::canonicalize(expected),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => Path::new(declared) == expected,
    }
}

/// The command string every accepted registry entry becomes.
///
/// Derived from `launcher` — [`bundled_launcher`]'s sibling-of-this-binary
/// path — and never from the document. Canonicalized when the filesystem can
/// resolve it, which is the same path `same_file` compared against, so the
/// value that runs is the value that passed the check; a link swapped after
/// the check cannot redirect the exec, and a relative declared command cannot
/// be resolved a second time against the child's own `cwd`
/// (`buzz-agent` sets `Command::current_dir(spec.cwd)` before it spawns).
///
/// # Errors
/// A message when the derived path is not valid UTF-8, since `McpServer`
/// carries the command as a `String` on the ACP wire.
fn launcher_command(launcher: &Path) -> Result<String, String> {
    let derived = std::fs::canonicalize(launcher).unwrap_or_else(|_| launcher.to_path_buf());
    derived.to_str().map(str::to_string).ok_or_else(|| {
        format!(
            "the bundled launcher's path {} is not valid UTF-8, so it cannot be named on the ACP \
             wire",
            derived.display()
        )
    })
}

/// Why `path` is not a plain absolute path, or `None` when it is one.
///
/// One shape, two callers: the registry file's own path
/// ([`confine_registry_path`]) and every entry's declared command. Both are
/// strings that reach an `open` or an `exec`, and both are refused unless they
/// are absolute and free of `.` and `..` — a relative path is resolved against
/// whichever working directory the consumer happens to hold, and a `..`
/// component lets a confined-looking path name something outside the
/// confinement.
fn plain_absolute_defect(path: &Path) -> Option<&'static str> {
    if !path.is_absolute() {
        return Some("is not an absolute path");
    }
    if path
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Some("holds a `.` or `..` component");
    }
    None
}

/// Refuse a registry path that is not the one the adopted generation wrote.
///
/// The desktop stages the document at
/// `<base>/mcp/generations/<generation>/agents/<agent id>/buzz-acp-registry.json`
/// and names that exact path in the spawn environment. Requiring the last five
/// components pins the file to **this** agent's directory inside **this**
/// generation: the generation and the agent id come from the capability, which
/// only the spawn seam sets and which the desktop strips from every user env
/// layer.
///
/// # Errors
/// A message naming the path when it is relative, holds a `.` or `..`
/// component, or does not end in the required five components.
pub fn confine_registry_path(path: &Path, capability: &AgentCapability) -> Result<(), String> {
    if let Some(defect) = plain_absolute_defect(path) {
        return Err(format!(
            "{MCP_REGISTRY_ENV_VAR} names {}, which {defect}",
            path.display()
        ));
    }
    let expected_tail = [
        GENERATIONS_DIR.to_string(),
        capability.generation().to_string(),
        AGENTS_DIR.to_string(),
        capability.agent_id().to_string(),
        REGISTRY_FILE_NAME.to_string(),
    ];
    let tail: Vec<String> = path
        .components()
        .rev()
        .take(expected_tail.len())
        .filter_map(|c| match c {
            Component::Normal(part) => part.to_str().map(str::to_string),
            _ => None,
        })
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if tail != expected_tail {
        return Err(format!(
            "{MCP_REGISTRY_ENV_VAR} names {}, which is outside this agent's directory in the \
             adopted configuration generation; the desktop writes {}",
            path.display(),
            expected_tail.join("/")
        ));
    }
    Ok(())
}

/// The document as the desktop writes it.
#[derive(Debug, Deserialize)]
struct RegistryFile {
    version: u32,
    #[serde(default)]
    servers: Vec<RegistryFileServer>,
}

/// One generated server.
#[derive(Debug, Deserialize)]
struct RegistryFileServer {
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: Vec<RegistryFileEnv>,
}

/// One declared environment entry.
///
/// The desktop writes none, and [`parse_registry_file`] refuses a document
/// that declares one. The shape is still deserialized so that refusal can name
/// how many were declared instead of failing as a parse error the operator
/// cannot act on.
#[derive(Debug, Deserialize)]
struct RegistryFileEnv {
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    value: String,
}

/// Read the registry file at `path`, bounded at [`MAX_REGISTRY_FILE_BYTES`].
///
/// The final component is opened without following a symlink and the open file
/// must be a regular one, so nothing can swap the confined path for a link to
/// another file, a FIFO whose read never returns, or a device. The read is
/// bounded before any UTF-8 decoding or parsing, so a file grown past the cap —
/// by a bug in the writer or by anything else that can write the app data
/// directory — is refused rather than allocated.
///
/// # Errors
/// A message naming the path when the file cannot be opened, is a symlink or
/// not a regular file, cannot be read, or is over the cap.
pub fn read_registry_file(path: &Path) -> Result<Vec<u8>, String> {
    let file = open_no_follow(path).map_err(|e| {
        format!(
            "{MCP_REGISTRY_ENV_VAR} names {}, which cannot be opened: {e}",
            path.display()
        )
    })?;
    let kind = file
        .metadata()
        .map_err(|e| {
            format!(
                "{MCP_REGISTRY_ENV_VAR} names {}, whose type cannot be read: {e}",
                path.display()
            )
        })?
        .file_type();
    if !kind.is_file() {
        return Err(format!(
            "{MCP_REGISTRY_ENV_VAR} names {}, which is not a regular file",
            path.display()
        ));
    }
    // One byte past the cap, so a file that exactly fills it is accepted and
    // one byte more is reported: the exhausted limit is what detects the
    // overflow, which widening the bound cannot leave unreported.
    let mut bounded = file.take(MAX_REGISTRY_FILE_BYTES as u64 + 1);
    let mut bytes = Vec::new();
    bounded.read_to_end(&mut bytes).map_err(|e| {
        format!(
            "{MCP_REGISTRY_ENV_VAR} names {}, which cannot be read: {e}",
            path.display()
        )
    })?;
    if bounded.limit() == 0 {
        return Err(format!(
            "{MCP_REGISTRY_ENV_VAR} names {}, which is larger than the {MAX_REGISTRY_FILE_BYTES}-byte cap",
            path.display()
        ));
    }
    Ok(bytes)
}

/// Open `path` for reading without following a symlink at its final component.
///
/// On Unix this is `O_NOFOLLOW`, which fails the open itself, so there is no
/// window between the check and the read. Windows has no equivalent open flag
/// here, so the link check is a `symlink_metadata` call before the open; the
/// caller's regular-file check on the opened handle still holds either way.
fn open_no_follow(path: &Path) -> std::io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the path is a symbolic link",
            ));
        }
        File::open(path)
    }
}

/// Parse `bytes` into MCP servers, refusing anything past a bound.
///
/// `max_servers` is the caller's remaining budget: the whole `session/new`
/// array is capped, and the primary server plus any
/// `BUZZ_ACP_EXTRA_MCP_COMMANDS` entries have already taken their share.
///
/// Every server is marked untrusted. These are third-party processes reached
/// through a launcher, so they receive no Buzz identity variable and never run
/// a hook, exactly like an extras entry.
///
/// `launcher` is the bundled launcher's absolute path ([`bundled_launcher`]).
/// Every entry's command must be a plain absolute path — no `.`, no `..`, the
/// same shape [`confine_registry_path`] requires — naming that file: the
/// `registry_launched` marker is what makes `buzz-agent` hand a child the
/// capability, so an entry naming anything else refuses startup rather than
/// losing the marker silently.
///
/// The accepted entry's `command` is then built from `launcher`, not from the
/// document (see [`launcher_command`]). The declared string is compared and
/// discarded: `buzz-agent` execs the stored command under the session's own
/// `cwd`, so storing the declared string would resolve a relative path a
/// second time somewhere else, and would re-follow a symlink that could have
/// been swapped since the comparison.
///
/// # Errors
/// A message naming the failing entry by index — never by content, since an
/// argument may be operator-supplied — when the document does not parse,
/// declares an unsupported version, holds more servers than `max_servers`,
/// repeats a name, declares an `env` block, names a command that is relative,
/// holds a `.` or `..` component, or is not `launcher`, or breaches a
/// per-entry bound. Also when the derived launcher path is not valid UTF-8.
pub fn parse_registry_file(
    bytes: &[u8],
    max_servers: usize,
    launcher: &Path,
) -> Result<Vec<McpServer>, String> {
    let document: RegistryFile = serde_json::from_slice(bytes)
        .map_err(|e| format!("the mcp registry file is not a valid registry document: {e}"))?;
    if document.version != REGISTRY_FILE_VERSION {
        return Err(format!(
            "the mcp registry file declares version {}, and this build reads version {REGISTRY_FILE_VERSION}",
            document.version
        ));
    }
    if document.servers.len() > max_servers {
        return Err(format!(
            "the mcp registry file declares {} servers, and only {max_servers} more fit in one \
             session (buzz-agent rejects the whole array past the cap, so every session would fail)",
            document.servers.len()
        ));
    }

    // Resolved once, from this process's own binary location, and stored on
    // every accepted entry in place of the declared string.
    let command = launcher_command(launcher)?;

    let mut servers: Vec<McpServer> = Vec::with_capacity(document.servers.len());
    for (index, server) in document.servers.iter().enumerate() {
        let entry = index + 1;
        let bound = |what: &str, actual: usize, cap: usize| {
            format!("mcp registry entry {entry} has {actual} {what}, over the cap of {cap}")
        };
        if server.name.is_empty() || server.name.len() > MAX_MCP_NAME_LEN {
            return Err(format!(
                "mcp registry entry {entry} has a name of {} bytes; a name must be 1 to \
                 {MAX_MCP_NAME_LEN} bytes",
                server.name.len()
            ));
        }
        if !server
            .name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(format!(
                "mcp registry entry {entry} has a name outside `[A-Za-z0-9-]`, which the \
                 downstream tool-name validator refuses"
            ));
        }
        if server.command.is_empty() || server.command.len() > MAX_REGISTRY_COMMAND_LEN {
            return Err(format!(
                "mcp registry entry {entry} has a command of {} bytes; a command must be 1 to \
                 {MAX_REGISTRY_COMMAND_LEN} bytes",
                server.command.len()
            ));
        }
        if server.args.len() > MAX_REGISTRY_ARGS {
            return Err(bound("arguments", server.args.len(), MAX_REGISTRY_ARGS));
        }
        if let Some(long) = server
            .args
            .iter()
            .find(|arg| arg.len() > MAX_REGISTRY_ARG_LEN)
        {
            return Err(bound(
                "bytes in one argument",
                long.len(),
                MAX_REGISTRY_ARG_LEN,
            ));
        }
        if !server.env.is_empty() {
            // `buzz-agent` applies this block to the launcher's own `Command`,
            // where a loader variable would run attacker code inside the
            // process that holds the capability. The desktop never writes one:
            // a declared variable rides the launcher's `--set`/`--secret` argv
            // and reaches the child only.
            return Err(format!(
                "mcp registry entry {entry} declares {} environment variables; the desktop \
                 carries them in the launcher's arguments and writes no `env` block, so this \
                 document was not generated by this desktop",
                server.env.len()
            ));
        }
        // Shape before identity. A relative command would be resolved against
        // whatever working directory the consumer holds — `buzz-agent` execs
        // this entry under the session's `cwd`, not this process's — and a
        // `..` component would let a launcher-shaped prefix name something
        // else. Neither can be answered by the `same_file` comparison below,
        // which resolves against *this* process's working directory.
        if let Some(defect) = plain_absolute_defect(Path::new(&server.command)) {
            return Err(format!(
                "mcp registry entry {entry} names a command that {defect}; the desktop writes the \
                 bundled launcher {} by resolved absolute path",
                launcher.display()
            ));
        }
        if !same_file(&server.command, launcher) {
            return Err(format!(
                "mcp registry entry {entry} names a command that is not the bundled launcher \
                 {}; only the launcher may run with this agent's capability",
                launcher.display()
            ));
        }
        if let Some(first) = servers.iter().position(|s| s.name == server.name) {
            return Err(format!(
                "mcp registry entries {} and {entry} declare the same server name; both native \
                 config formats key servers by name, so one entry would silently keep the other's \
                 command",
                first + 1
            ));
        }
        servers.push(McpServer {
            name: server.name.clone(),
            // The derived launcher path, never `server.command`. The declared
            // string was compared above and is discarded here: what runs is
            // the path this process resolved beside its own binary, so a
            // symlink swapped after the comparison, or a relative path
            // re-resolved against the session's `cwd`, cannot redirect the
            // exec that carries this agent's capability.
            command: command.clone(),
            args: server.args.clone(),
            // Never an `env` block: the check above refused any document that
            // declared one, and this is what `buzz-agent` applies to the
            // launcher's own process.
            env: Vec::new(),
            trusted: false,
            // The file sits in this agent's directory inside the adopted
            // generation, was opened without following a link, and this entry
            // names the bundled launcher. The marker is what makes
            // `buzz-agent` hand the child `BUZZ_MCP_CAPABILITY`; without it
            // every credential-backed registry server exits 1 before it runs.
            registry_launched: true,
        });
    }
    Ok(servers)
}

/// What this process itself contributes to reading a registry file: the
/// capability the spawn seam minted for it, and the launcher bundled beside
/// this binary.
///
/// Both are deliberately taken from the process rather than from the config,
/// because the config's `mcp_registry` path is the input being checked.
#[derive(Debug, Clone)]
pub struct RegistrySource {
    capability: AgentCapability,
    launcher: PathBuf,
}

impl RegistrySource {
    /// Resolve both from this process.
    ///
    /// # Errors
    /// A message when [`crate::mcp_registry::MCP_REGISTRY_ENV_VAR`]'s companion
    /// capability is absent or unusable, or when the bundled launcher's path
    /// cannot be resolved.
    pub fn from_process() -> Result<Self, String> {
        // The capability names the agent and the generation this spawn
        // adopted. Both come from the desktop's spawn seam, which strips the
        // variable from every user env layer before it writes its own — so it
        // is the one input here that a saved persona or an imported agent
        // definition cannot supply.
        let capability =
            AgentCapability::from_env(|name| std::env::var(name).ok()).map_err(|e| {
                format!(
                "{MCP_REGISTRY_ENV_VAR} is set, but this process holds no usable MCP capability \
                 ({e}), so its registry servers could not resolve a single credential; restart \
                 the agent from the desktop"
            )
            })?;
        Ok(Self {
            capability,
            launcher: bundled_launcher()?,
        })
    }

    /// Build one explicitly.
    ///
    /// Test-only: production must take both from the process, or the checks
    /// they drive would be satisfied by the same side that supplied the path.
    #[cfg(test)]
    pub(crate) fn for_test(capability: AgentCapability, launcher: PathBuf) -> Self {
        Self {
            capability,
            launcher,
        }
    }
}

/// Append the desktop's generated registry servers to `servers`.
///
/// Nothing happens when [`MCP_REGISTRY_ENV_VAR`] is unset or empty, so an
/// operator running only `BUZZ_ACP_EXTRA_MCP_COMMANDS` is unaffected.
///
/// # Errors
/// [`ConfigError::ConfigFile`] when the file cannot be read or does not
/// satisfy a bound (see [`read_registry_file`] and [`parse_registry_file`]),
/// when the agent command is not the one adapter that honours the `trusted`
/// marker, when this process holds no usable capability, when the path is
/// outside this agent's directory in the adopted generation, or when a
/// generated name collides with a server already built. Every one fails
/// startup rather than the first session: a harness that started here would
/// announce itself online and then fail every `session/new`.
pub fn append_registry_mcp_servers(
    config: &crate::config::Config,
    servers: &mut Vec<McpServer>,
    source: Option<&RegistrySource>,
) -> Result<(), ConfigError> {
    let Some(path) = config
        .mcp_registry
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    else {
        return Ok(());
    };

    // Same boundary the extras path draws, for the same reason: these servers
    // are declared untrusted, and only `buzz-agent` acts on that marker.
    // Every other adapter spawns them itself out of a process holding
    // BUZZ_PRIVATE_KEY, so refuse at startup rather than leak on the first
    // session.
    let adapter = crate::config::normalize_agent_command_identity(&config.agent_command);
    if adapter != crate::EXTRA_MCP_ADAPTER {
        return Err(ConfigError::ConfigFile(format!(
            "{MCP_REGISTRY_ENV_VAR} is set, but the agent command is `{adapter}`. Only \
             `{}` honours the `trusted` marker that withholds BUZZ_PRIVATE_KEY, \
             NOSTR_PRIVATE_KEY, BUZZ_RELAY_URL and BUZZ_AUTH_TAG from a registry MCP server; \
             every other ACP adapter spawns the declared servers itself, from a process that \
             inherited this one's environment.",
            crate::EXTRA_MCP_ADAPTER
        )));
    }

    let resolved;
    let source = match source {
        Some(source) => source,
        None => {
            resolved = RegistrySource::from_process().map_err(ConfigError::ConfigFile)?;
            &resolved
        }
    };
    confine_registry_path(Path::new(path), &source.capability).map_err(ConfigError::ConfigFile)?;

    let budget = MAX_MCP_SERVERS.saturating_sub(servers.len());
    let bytes = read_registry_file(Path::new(path)).map_err(ConfigError::ConfigFile)?;
    let generated =
        parse_registry_file(&bytes, budget, &source.launcher).map_err(ConfigError::ConfigFile)?;

    for server in generated {
        if servers.iter().any(|existing| existing.name == server.name) {
            return Err(ConfigError::ConfigFile(format!(
                "mcp registry server `{}` has the same name as a server this harness already \
                 declares; rename it in the registry and restart",
                server.name
            )));
        }
        servers.push(server);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::{wire_log_line, REDACTED_ENV_VALUE};

    /// A directory holding a file that stands in for the bundled launcher, so
    /// the command check canonicalizes a real path on both sides.
    struct Launcher {
        dir: tempfile::TempDir,
    }

    impl Launcher {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            std::fs::write(dir.path().join(LAUNCHER_FILE_NAME), b"#!/bin/sh\n")
                .expect("write the stand-in launcher");
            Self { dir }
        }

        fn path(&self) -> PathBuf {
            self.dir.path().join(LAUNCHER_FILE_NAME)
        }

        fn command(&self) -> String {
            self.path().display().to_string()
        }

        /// The path a parsed entry must carry: the launcher this process
        /// derived, canonicalized. On macOS a `tempfile` directory sits under
        /// `/var`, a symlink to `/private/var`, so this differs from
        /// [`Self::command`] and a test asserting it catches a build that
        /// stored the declared string instead.
        fn canonical_command(&self) -> String {
            std::fs::canonicalize(self.path())
                .expect("the stand-in launcher canonicalizes")
                .display()
                .to_string()
        }
    }

    /// A document holding one server as the desktop generates it: the launcher
    /// command, its argv, and no `env` block.
    fn document(servers: &str) -> String {
        format!("{{\"version\":1,\"servers\":[{servers}]}}")
    }

    fn stdio(launcher: &Launcher, name: &str) -> String {
        format!(
            "{{\"name\":\"{name}\",\"command\":\"{}\",\
             \"args\":[\"launch\",\"--server\",\"{name}\",\"--secret\",\"TOKEN=mcp:{name}-token\",\
             \"--\",\"/usr/local/bin/{name}\"]}}",
            launcher.command()
        )
    }

    /// A capability for `agent` at `generation`, built the way the desktop
    /// builds one from a binding record.
    fn capability(agent: &str, generation: u64) -> AgentCapability {
        AgentCapability::bind(agent, generation, &"ab".repeat(16)).expect("a valid capability")
    }

    /// The path the desktop writes for `agent` at `generation` under `base`.
    fn staged_path(base: &Path, agent: &str, generation: u64) -> PathBuf {
        base.join(GENERATIONS_DIR)
            .join(generation.to_string())
            .join(AGENTS_DIR)
            .join(agent)
            .join(REGISTRY_FILE_NAME)
    }

    #[test]
    fn registry_file_becomes_untrusted_servers_with_no_env_block() {
        let launcher = Launcher::new();
        let servers = parse_registry_file(
            document(&stdio(&launcher, "github")).as_bytes(),
            MAX_MCP_SERVERS,
            &launcher.path(),
        )
        .expect("a well-formed document loads");
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "github");
        // The derived launcher path, not the declared string.
        assert_eq!(servers[0].command, launcher.canonical_command());
        // The credential rides the launcher's own argv as a reference, so the
        // wire entry carries no environment at all — `buzz-agent` would apply
        // one to the launcher's process, not the server's.
        assert!(servers[0].env.is_empty());
        assert!(servers[0]
            .args
            .iter()
            .any(|arg| arg == "TOKEN=mcp:github-token"));
        // Untrusted: no Buzz identity variable, no hooks.
        assert!(!servers[0].trusted);
        // Registry-launched: this entry runs the bundled launcher, so
        // buzz-agent forwards BUZZ_MCP_CAPABILITY to it.
        assert!(servers[0].registry_launched);
    }

    /// A declared `env` block is refused, loader variables included.
    ///
    /// `buzz-agent` applies `McpServer.env` to the launcher's own `Command`
    /// (`crates/buzz-agent/src/mcp.rs`), so an accepted `DYLD_INSERT_LIBRARIES`
    /// would run attacker code inside the process holding the capability.
    /// Deleting the refusal makes this test fail with the marker still set.
    #[test]
    fn registry_file_refuses_a_declared_env_block() {
        let launcher = Launcher::new();
        for name in ["DYLD_INSERT_LIBRARIES", "LD_PRELOAD", "TOKEN"] {
            let entry = format!(
                "{{\"name\":\"a\",\"command\":\"{}\",\"args\":[],\
                 \"env\":[{{\"name\":\"{name}\",\"value\":\"/tmp/evil.dylib\"}}]}}",
                launcher.command()
            );
            let error = parse_registry_file(
                document(&entry).as_bytes(),
                MAX_MCP_SERVERS,
                &launcher.path(),
            )
            .expect_err("a declared env block is refused");
            assert!(error.contains("environment variables"), "{error}");
        }
    }

    /// Only the bundled launcher may carry the marker.
    ///
    /// The marker is what makes `buzz-agent` hand a child the capability that
    /// authorizes every `mcp:` record bound to this agent, so an entry naming
    /// another command refuses startup instead of running with it.
    #[test]
    fn registry_file_refuses_a_command_that_is_not_the_bundled_launcher() {
        let launcher = Launcher::new();
        let entry = format!(
            "{{\"name\":\"a\",\"command\":\"{}\",\"args\":[]}}",
            launcher.dir.path().join("not-the-launcher").display()
        );
        let error = parse_registry_file(
            document(&entry).as_bytes(),
            MAX_MCP_SERVERS,
            &launcher.path(),
        )
        .expect_err("a foreign command is refused");
        assert!(error.contains("bundled launcher"), "{error}");
    }

    /// What runs is the launcher this process derived, never the string the
    /// document declared.
    ///
    /// A link to the launcher passes the identity check — both sides
    /// canonicalize to the same file — but storing the link would leave the
    /// exec pointing at a name whose target can be re-pointed after the check
    /// and before `buzz-agent` spawns it, with this agent's capability in the
    /// child's environment. Building `McpServer.command` from the declared
    /// string again fails this: the stored command would be the link.
    #[test]
    #[cfg(unix)]
    fn registry_file_commands_are_the_derived_launcher_not_the_declared_path() {
        let launcher = Launcher::new();
        let link = launcher.dir.path().join("link-to-launcher");
        std::os::unix::fs::symlink(launcher.path(), &link).expect("symlink to the launcher");

        let entry = format!(
            "{{\"name\":\"a\",\"command\":\"{}\",\"args\":[]}}",
            link.display()
        );
        let servers = parse_registry_file(
            document(&entry).as_bytes(),
            MAX_MCP_SERVERS,
            &launcher.path(),
        )
        .expect("a link to the bundled launcher names the bundled launcher");

        assert_eq!(servers.len(), 1);
        assert!(
            servers[0].registry_launched,
            "the entry lost the marker it earned"
        );
        assert_ne!(
            Path::new(&servers[0].command),
            link.as_path(),
            "a registry-launched server would exec the declared symlink"
        );
        assert_eq!(servers[0].command, launcher.canonical_command());
    }

    /// A relative command is refused, and the reason reaches the operator.
    ///
    /// `buzz-agent` execs a registry entry under the **session's** working
    /// directory (`Command::current_dir(spec.cwd)`), not this process's, so a
    /// relative command names one file where it is checked and another where
    /// it runs. The shape check refuses it first; without it the entry falls
    /// through to the identity comparison, which reports the wrong cause —
    /// this test asserts the surfaced reason, so deleting the shape check
    /// fails it.
    #[test]
    fn registry_file_refuses_a_relative_command() {
        let launcher = Launcher::new();
        let relative = document(&format!(
            "{{\"name\":\"a\",\"command\":\"{LAUNCHER_FILE_NAME}\",\"args\":[]}}"
        ));
        let error = parse_registry_file(relative.as_bytes(), MAX_MCP_SERVERS, &launcher.path())
            .expect_err("a relative command is refused");
        assert!(
            error.contains("is not an absolute path"),
            "a relative command was refused for the wrong reason: {error}"
        );
    }

    /// An absolute command holding a `..` component is refused.
    ///
    /// The path below canonicalizes to the launcher, so the identity
    /// comparison accepts it on its own: only the shape check refuses it, and
    /// deleting that check turns this refusal into an accepted
    /// `registry_launched` entry.
    #[test]
    fn registry_file_refuses_a_command_with_a_parent_directory_component() {
        let launcher = Launcher::new();
        let dir = launcher.dir.path();
        std::fs::create_dir_all(dir.join("sub")).expect("create the traversed directory");
        let traversal = dir.join("sub").join("..").join(LAUNCHER_FILE_NAME);
        assert_eq!(
            std::fs::canonicalize(&traversal).expect("the traversal resolves"),
            std::fs::canonicalize(launcher.path()).expect("the launcher resolves"),
            "the traversal must resolve to the launcher, or this test proves nothing"
        );

        let doc = document(&format!(
            "{{\"name\":\"a\",\"command\":\"{}\",\"args\":[]}}",
            traversal.display()
        ));
        let error = parse_registry_file(doc.as_bytes(), MAX_MCP_SERVERS, &launcher.path())
            .expect_err("a `..` traversal in the command is refused");
        assert!(
            error.contains("holds a `.` or `..` component"),
            "{} was refused for the wrong reason: {error}",
            traversal.display()
        );
    }

    /// The accepted path is this agent's directory in the adopted generation.
    ///
    /// Every rejected shape here is one a hostile `BUZZ_ACP_MCP_REGISTRY` would
    /// take. Deleting the confinement accepts all four.
    #[test]
    fn registry_path_is_confined_to_the_adopted_generation() {
        let base = tempfile::tempdir().expect("tempdir");
        let capability = capability("agent-a", 7);
        let good = staged_path(base.path(), "agent-a", 7);
        confine_registry_path(&good, &capability).expect("the staged path is accepted");

        // Each case names the cause it must be refused for, not merely that it
        // was refused (PR 23 follow-up 4). `..` in particular is refused by the
        // shape check rather than by the tail comparison, and asserting only
        // `is_err()` would keep passing if the shape check were deleted and the
        // tail happened to disagree for an unrelated reason.
        for (path, why, cause) in [
            (
                staged_path(base.path(), "agent-b", 7),
                "another agent's directory",
                "outside this agent's directory",
            ),
            (
                staged_path(base.path(), "agent-a", 6),
                "a superseded generation",
                "outside this agent's directory",
            ),
            (
                base.path().join("elsewhere").join(REGISTRY_FILE_NAME),
                "a path outside the staging tree",
                "outside this agent's directory",
            ),
            (
                base.path()
                    .join(GENERATIONS_DIR)
                    .join("7")
                    .join(AGENTS_DIR)
                    .join("agent-a")
                    .join("..")
                    .join("agent-b")
                    .join(REGISTRY_FILE_NAME),
                "a `..` traversal",
                "holds a `.` or `..` component",
            ),
            (
                PathBuf::from("relative/registry.json"),
                "a relative path",
                "is not an absolute path",
            ),
        ] {
            let error = confine_registry_path(&path, &capability)
                .expect_err(&format!("{why} was accepted: {}", path.display()));
            assert!(
                error.contains(cause),
                "{why} was refused for the wrong reason; expected {cause:?}, got {error}"
            );
        }
    }

    /// The open refuses a symlink and anything that is not a regular file.
    #[test]
    #[cfg(unix)]
    fn registry_file_open_refuses_a_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real.json");
        std::fs::write(&real, "{}").expect("write");
        let link = dir.path().join("link.json");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        let error = read_registry_file(&link).expect_err("a symlink is refused");
        assert!(error.contains("cannot be opened"), "{error}");

        let error = read_registry_file(dir.path()).expect_err("a directory is refused");
        assert!(
            error.contains("not a regular file") || error.contains("cannot be read"),
            "{error}"
        );
    }

    /// The marker crosses the wire under the name `buzz-agent` deserializes,
    /// and only a registry entry carries it.
    ///
    /// The launcher reads the capability from its own inherited environment
    /// and exits 1 without it, so an entry that reached `session/new` without
    /// this field would start no server at all; and a marker on an extra
    /// command would hand an operator's own process a bearer token for every
    /// reference the agent can resolve. Both halves are asserted on the
    /// serialized JSON, which is what the two crates actually agree on.
    #[test]
    fn registry_file_marks_only_its_own_servers_registry_launched() {
        let launcher = Launcher::new();
        let mut servers = vec![McpServer {
            name: "extra".to_string(),
            command: "/opt/extra".to_string(),
            args: Vec::new(),
            env: Vec::new(),
            trusted: false,
            registry_launched: false,
        }];
        let generated = parse_registry_file(
            document(&stdio(&launcher, "github")).as_bytes(),
            MAX_MCP_SERVERS,
            &launcher.path(),
        )
        .expect("a well-formed document loads");
        servers.extend(generated);

        let wire = serde_json::to_value(&servers).expect("serializes");
        assert_eq!(
            wire[0].get("registry_launched"),
            None,
            "an operator's own server must not be marked: {wire}"
        );
        assert_eq!(
            wire[1]["registry_launched"],
            serde_json::json!(true),
            "the registry entry lost its marker: {wire}"
        );
    }

    #[test]
    fn registry_file_bounds_are_enforced_per_entry() {
        let launcher = Launcher::new();
        let command = launcher.command();
        let over_version = "{\"version\":2,\"servers\":[]}";
        assert!(
            parse_registry_file(over_version.as_bytes(), MAX_MCP_SERVERS, &launcher.path())
                .expect_err("an unknown version is refused")
                .contains("version 2")
        );

        let long_name = "a".repeat(MAX_MCP_NAME_LEN + 1);
        let doc = document(&format!(
            "{{\"name\":\"{long_name}\",\"command\":\"{command}\",\"args\":[]}}"
        ));
        assert!(
            parse_registry_file(doc.as_bytes(), MAX_MCP_SERVERS, &launcher.path())
                .expect_err("an over-long name is refused")
                .contains("bytes")
        );

        let bad_name = document(&format!(
            "{{\"name\":\"a/b\",\"command\":\"{command}\",\"args\":[]}}"
        ));
        assert!(
            parse_registry_file(bad_name.as_bytes(), MAX_MCP_SERVERS, &launcher.path()).is_err()
        );

        let args: Vec<String> = (0..=MAX_REGISTRY_ARGS)
            .map(|i| format!("\"{i}\""))
            .collect();
        let many_args = document(&format!(
            "{{\"name\":\"a\",\"command\":\"{command}\",\"args\":[{}]}}",
            args.join(",")
        ));
        assert!(
            parse_registry_file(many_args.as_bytes(), MAX_MCP_SERVERS, &launcher.path())
                .expect_err("too many arguments is refused")
                .contains("over the cap")
        );

        let long_arg = "v".repeat(MAX_REGISTRY_ARG_LEN + 1);
        let big_arg = document(&format!(
            "{{\"name\":\"a\",\"command\":\"{command}\",\"args\":[\"{long_arg}\"]}}"
        ));
        assert!(
            parse_registry_file(big_arg.as_bytes(), MAX_MCP_SERVERS, &launcher.path())
                .expect_err("an over-long argument is refused")
                .contains("over the cap")
        );

        // The budget is the caller's remaining share of one session's array,
        // not the file's own size: the primary server and any extras have
        // already taken theirs.
        let two = document(&format!(
            "{},{}",
            stdio(&launcher, "a"),
            stdio(&launcher, "b")
        ));
        assert!(parse_registry_file(two.as_bytes(), 1, &launcher.path())
            .expect_err("more servers than the remaining budget is refused")
            .contains("only 1 more"));
        assert!(parse_registry_file(two.as_bytes(), 2, &launcher.path()).is_ok());
    }

    #[test]
    fn registry_file_refuses_two_entries_with_one_name() {
        let launcher = Launcher::new();
        let doc = document(&format!(
            "{},{}",
            stdio(&launcher, "github"),
            stdio(&launcher, "github")
        ));
        assert!(
            parse_registry_file(doc.as_bytes(), MAX_MCP_SERVERS, &launcher.path())
                .expect_err("a repeated name is refused")
                .contains("same server name")
        );
    }

    #[test]
    fn registry_file_read_is_bounded_and_missing_files_are_surfaced() {
        let launcher = Launcher::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("registry.json");

        let error = read_registry_file(&path).expect_err("a missing file is an error");
        assert!(error.contains(MCP_REGISTRY_ENV_VAR), "{error}");

        std::fs::write(&path, vec![b'x'; MAX_REGISTRY_FILE_BYTES + 1]).expect("write");
        let error = read_registry_file(&path).expect_err("an oversized file is refused");
        assert!(error.contains("cap"), "{error}");

        std::fs::write(&path, document(&stdio(&launcher, "github"))).expect("write");
        let bytes = read_registry_file(&path).expect("a file under the cap reads");
        assert_eq!(
            parse_registry_file(&bytes, MAX_MCP_SERVERS, &launcher.path())
                .expect("parses")
                .len(),
            1
        );
    }

    /// The redaction is only worth anything if the log site uses it.
    ///
    /// `wire_log_redacts_env_values` drives the serializer, which a revert of
    /// the log site back to `serde_json::to_string(&msg)` would leave green —
    /// a guard whose removal fails no test. This reads `acp.rs` and asserts
    /// every outbound `acp::wire` line goes through `wire_log_line`, so that
    /// revert fails here.
    #[test]
    fn every_outbound_wire_log_serializes_through_the_redacting_writer() {
        let source = include_str!("acp.rs");
        let outbound: Vec<&str> = source
            .lines()
            .map(str::trim)
            .filter(|line| line.contains("target: \"acp::wire\"") && line.contains("\"\u{2192}"))
            .collect();
        assert!(
            outbound.len() >= 3,
            "expected every outbound wire log site; found {}",
            outbound.len()
        );
        for line in outbound {
            assert!(
                line.contains("wire_log_line(&msg)"),
                "an outbound acp::wire log does not redact declared env values: {line}"
            );
        }
    }

    /// Memo decision 10's logging clause, at the seam `acp::wire` uses.
    ///
    /// `send_request` logs the whole serialized `session/new`, and `McpServer.env`
    /// is not skipped by its `Serialize`. The redaction is structural, so the
    /// guarantee does not depend on the log level staying below `debug`.
    /// Restoring `serde_json::to_string(&msg)` at the log site fails this.
    ///
    /// The server is built here rather than parsed: a registry entry now
    /// carries no `env` block at all, and the servers that still can — the
    /// primary one this harness declares — are the ones the redaction protects.
    #[test]
    fn wire_log_redacts_env_values() {
        let servers = vec![McpServer {
            name: "github".to_string(),
            command: "/opt/buzz/bin/buzz-mcp-launch".to_string(),
            args: vec!["launch".to_string()],
            env: vec![crate::acp::EnvVar {
                name: "TOKEN".to_string(),
                value: "mcp:github-token".to_string(),
            }],
            trusted: false,
            registry_launched: false,
        }];
        // The exact `params` shape `session_new_full` builds.
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/new",
            "params": { "cwd": "/tmp", "mcpServers": servers },
        });

        let logged = wire_log_line(&message);
        assert!(
            !logged.contains("mcp:github-token"),
            "the wire log carried a declared env value: {logged}"
        );
        assert!(logged.contains(REDACTED_ENV_VALUE), "{logged}");
        // Redaction, not blanking: the log still names the server, the
        // launcher and the variable, which is what makes it worth keeping.
        assert!(logged.contains("github"), "{logged}");
        assert!(logged.contains("buzz-mcp-launch"), "{logged}");
        assert!(logged.contains("TOKEN"), "{logged}");
        // A message with no mcpServers is passed through unchanged.
        let plain = serde_json::json!({"jsonrpc": "2.0", "method": "x", "params": {"a": 1}});
        assert_eq!(
            wire_log_line(&plain),
            serde_json::to_string(&plain).expect("serializes")
        );
    }
}
