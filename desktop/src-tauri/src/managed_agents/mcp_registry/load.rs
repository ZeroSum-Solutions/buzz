//! Reading and validating the registry document (memo decision 7).
//!
//! Exactly three failures reject the whole document — a duplicate id or name,
//! the document byte cap, and the single-entry byte cap. The duplicate because
//! which of two colliding entries survives would otherwise be document order;
//! the two caps because a truncated registry is not a safe partial. A document
//! that is not JSON at all is not a fourth policy, it is the absence of a
//! document.
//!
//! Every other validation failure is per entry: the entry is disabled and
//! carries a status string the panel renders beside it, the rest of the
//! registry loads, and an agent that has a rejected entry enabled refuses to
//! spawn with that same message. A `tracing::warn` is not a refusal — that is
//! the `custom_harnesses` bar this loader deliberately does not inherit
//! (`custom_harnesses.rs:72-77,110-117`), along with its unbounded
//! `read_to_string` (`custom_harnesses.rs:110`).

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use buzz_secret_store_pkg::sentinel::{is_credential_name, scan_argv, scan_value};
use buzz_secret_store_pkg::McpSecretRef;

use super::generate::generated_arg_count;
use super::schema::{
    RegistryDocument, RegistryEntry, RegistryTransport, BUILTIN_SERVER_NAMES, MAX_ARGS,
    MAX_ARG_LEN, MAX_DOCUMENT_BYTES, MAX_DOCUMENT_SERVERS, MAX_ENTRY_BYTES, MAX_ENV_ENTRIES,
    MAX_ENV_NAME_LEN, MAX_ENV_VALUE_LEN, MAX_GENERATED_ARGS, MAX_ID_LEN, MAX_NAME_LEN,
    RESERVED_NAME_PREFIX,
};

/// The pinned auth schemes an HTTP entry may name.
const PINNED_AUTH_SCHEMES: &[&str] = &["bearer", "token", "basic", "api-key"];

/// The eight proxy variable spellings. A registry entry may declare one only
/// when its value carries no userinfo.
const PROXY_VARIABLES: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "all_proxy",
];

/// A whole-document failure. Nothing loads.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    /// The file could not be opened or read.
    #[error("cannot read the mcp registry at {path}: {reason}")]
    Io {
        /// The path that failed.
        path: String,
        /// The OS message.
        reason: String,
    },
    /// The document is over [`MAX_DOCUMENT_BYTES`].
    #[error("the mcp registry is larger than the {MAX_DOCUMENT_BYTES}-byte cap")]
    DocumentTooLarge,
    /// One entry serializes to more than [`MAX_ENTRY_BYTES`].
    #[error("the mcp registry entry `{0}` is larger than the {MAX_ENTRY_BYTES}-byte cap")]
    EntryTooLarge(String),
    /// More than [`MAX_DOCUMENT_SERVERS`] entries.
    #[error("the mcp registry declares {0} servers, over the {MAX_DOCUMENT_SERVERS} cap")]
    TooManyServers(usize),
    /// Two entries share an id or a name, case-insensitively.
    #[error("the mcp registry declares `{value}` twice as a {kind}; which one survives would be document order, so nothing is loaded")]
    Duplicate {
        /// Either `id` or `name`.
        kind: &'static str,
        /// The colliding value, lowercased.
        value: String,
    },
    /// The document is not the expected JSON shape.
    #[error("the mcp registry is not valid json: {0}")]
    Malformed(String),
    /// The document declares a schema version this build does not read.
    #[error("the mcp registry declares version {0}, which this build does not read")]
    UnsupportedVersion(u32),
}

/// A loaded entry, with the status the panel renders beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedEntry {
    /// The entry as written.
    pub entry: RegistryEntry,
    /// `None` when the entry is usable; otherwise the operator-facing reason it
    /// is disabled.
    pub rejection: Option<String>,
}

impl LoadedEntry {
    /// Whether this entry may be used at all.
    pub fn is_usable(&self) -> bool {
        self.rejection.is_none()
    }
}

/// A loaded registry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadedRegistry {
    /// Every entry in document order, each with its status.
    pub entries: Vec<LoadedEntry>,
}

impl LoadedRegistry {
    /// Find an entry by its stable id.
    pub fn by_id(&self, id: &str) -> Option<&LoadedEntry> {
        self.entries.iter().find(|loaded| loaded.entry.id == id)
    }
}

/// Read the registry document at `path` and validate it.
///
/// The file is opened without following a symlink and read to at most
/// [`MAX_DOCUMENT_BYTES`] plus one byte before any UTF-8 decoding or parsing, so
/// a sparse multi-gigabyte file is refused without being allocated.
///
/// A missing file is an empty registry, not an error: no registry is the
/// default state.
///
/// # Errors
/// [`RegistryError`] for any whole-document failure.
pub fn load_registry(path: &Path) -> Result<LoadedRegistry, RegistryError> {
    let bytes = match read_bounded_no_follow(path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(LoadedRegistry::default()),
        Err(error) => return Err(error),
    };
    parse_registry(&bytes)
}

/// Validate an in-memory document. Split out so tests drive the same code the
/// file path runs, without a file.
///
/// # Errors
/// [`RegistryError`] for any whole-document failure.
pub fn parse_registry(bytes: &[u8]) -> Result<LoadedRegistry, RegistryError> {
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(RegistryError::DocumentTooLarge);
    }
    let document: RegistryDocument =
        serde_json::from_slice(bytes).map_err(|e| RegistryError::Malformed(e.to_string()))?;
    if document.version != 1 {
        return Err(RegistryError::UnsupportedVersion(document.version));
    }
    if document.servers.len() > MAX_DOCUMENT_SERVERS {
        return Err(RegistryError::TooManyServers(document.servers.len()));
    }

    // Whole-document checks first: a duplicate or an oversized entry must
    // refuse before any entry is reported as usable.
    let mut seen_ids = BTreeMap::new();
    let mut seen_names = BTreeMap::new();
    for entry in &document.servers {
        let serialized =
            serde_json::to_vec(entry).map_err(|e| RegistryError::Malformed(e.to_string()))?;
        if serialized.len() > MAX_ENTRY_BYTES {
            return Err(RegistryError::EntryTooLarge(entry.id.clone()));
        }
        let id = entry.id.to_lowercase();
        if seen_ids.insert(id.clone(), ()).is_some() {
            return Err(RegistryError::Duplicate {
                kind: "id",
                value: id,
            });
        }
        let name = entry.name.to_lowercase();
        if seen_names.insert(name.clone(), ()).is_some() {
            return Err(RegistryError::Duplicate {
                kind: "name",
                value: name,
            });
        }
    }

    Ok(LoadedRegistry {
        entries: document
            .servers
            .into_iter()
            .map(|entry| {
                let rejection = validate_entry(&entry).err();
                LoadedEntry { entry, rejection }
            })
            .collect(),
    })
}

/// Open `path` without following a symlink and read at most the document cap
/// plus one byte.
///
/// `Ok(None)` when the file does not exist.
pub(crate) fn read_bounded_no_follow(path: &Path) -> Result<Option<Vec<u8>>, RegistryError> {
    let io_error = |reason: String| RegistryError::Io {
        path: path.display().to_string(),
        reason,
    };

    let mut options = std::fs::File::options();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // O_NOFOLLOW: a symlink in place of the registry redirects the read.
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }

    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error.to_string())),
    };

    // Cap + 1: a read that fills the buffer proves the file is over the cap,
    // and no more than that is ever allocated.
    let mut buffer = Vec::new();
    let mut limited = file.take((MAX_DOCUMENT_BYTES as u64) + 1);
    limited
        .read_to_end(&mut buffer)
        .map_err(|e| io_error(e.to_string()))?;
    if buffer.len() > MAX_DOCUMENT_BYTES {
        return Err(RegistryError::DocumentTooLarge);
    }
    Ok(Some(buffer))
}

/// Validate one entry. `Err` carries the operator-facing rejection reason.
fn validate_entry(entry: &RegistryEntry) -> Result<(), String> {
    check_id(&entry.id)?;
    check_name(&entry.name)?;
    if entry.name.starts_with(RESERVED_NAME_PREFIX) {
        return Err(format!(
            "`{}` uses the reserved `{RESERVED_NAME_PREFIX}` prefix",
            entry.name
        ));
    }
    if BUILTIN_SERVER_NAMES.contains(&entry.name.as_str()) {
        return Err(format!(
            "`{}` is the name of a built-in server; two servers with one name serialize to one config key",
            entry.name
        ));
    }
    validate_env(&entry.env)?;

    match &entry.transport {
        RegistryTransport::Stdio { command, args } => validate_stdio(command, args)?,
        RegistryTransport::Http { url, auth } => validate_http(url, auth.as_ref())?,
    }

    // The bound that actually costs is the generated launcher argv, which is
    // what `buzz-acp` reads and bounds. Counted from the generator itself, so
    // the two cannot drift.
    let generated = generated_arg_count(entry);
    if generated > MAX_GENERATED_ARGS {
        return Err(format!(
            "it generates a launcher command line of {generated} arguments, over the \
             {MAX_GENERATED_ARGS} cap the agent harness accepts"
        ));
    }
    Ok(())
}

/// The registry id: desktop-local, so it keeps the wider charset.
fn check_id(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("its id is empty".to_string());
    }
    if value.len() > MAX_ID_LEN {
        return Err(format!(
            "its id is {} bytes, over the {MAX_ID_LEN}-byte cap",
            value.len()
        ));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        return Err("its id may only use lowercase letters, digits, `_` and `-`".to_string());
    }
    Ok(())
}

/// The server name: the one operator string that crosses into `buzz-acp`, so
/// it takes the stricter of the two sides' bounds (Sol W4). `buzz-acp` caps a
/// name at 32 bytes over `[A-Za-z0-9-]` and refuses the whole handover
/// document past either, so an underscore or a 33rd byte accepted here would
/// stop every registry-enabled agent from starting.
fn check_name(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("its name is empty".to_string());
    }
    if value.len() > MAX_NAME_LEN {
        return Err(format!(
            "its name is {} bytes, over the {MAX_NAME_LEN}-byte cap the agent harness accepts",
            value.len()
        ));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(
            "its name may only use lowercase letters, digits and `-`; the agent harness refuses \
             any other character in a server name"
                .to_string(),
        );
    }
    Ok(())
}

fn validate_stdio(command: &str, args: &[String]) -> Result<(), String> {
    // Shape guard for the document-supplied command and args (PR 23 follow-up
    // 8). A NUL cannot be passed to `execve`, so a value carrying one is a
    // command line that silently truncates at the OS boundary or fails far
    // from here; it is refused where the operator can see why. Count and
    // length are bounded below and by the generated-argv cap in
    // `validate_entry`.
    if command.as_bytes().contains(&0) {
        return Err("its command holds a NUL byte, which no command line can carry".to_string());
    }
    if command.len() > MAX_ARG_LEN {
        return Err(format!(
            "its command is {} bytes, over the {MAX_ARG_LEN}-byte cap; the command is generated \
             as one launcher argument",
            command.len()
        ));
    }
    if let Some((index, _)) = args
        .iter()
        .enumerate()
        .find(|(_, arg)| arg.as_bytes().contains(&0))
    {
        return Err(format!(
            "argument {index} holds a NUL byte, which no command line can carry"
        ));
    }
    if !Path::new(command).is_absolute() {
        return Err(format!(
            "`{command}` is not an absolute path; the launcher clears PATH, so a bare name would resolve through an environment nobody controls"
        ));
    }
    if args.len() > MAX_ARGS {
        return Err(format!(
            "it declares {} arguments, over the {MAX_ARGS} cap",
            args.len()
        ));
    }
    if let Some(over) = args.iter().find(|arg| arg.len() > MAX_ARG_LEN) {
        return Err(format!(
            "one argument is {} bytes, over the {MAX_ARG_LEN}-byte cap",
            over.len()
        ));
    }
    if let Some((index, hit)) = scan_argv(args) {
        return Err(format!(
            "argument {index} {} — argv is readable by `ps` and by any crash dump, so declare an `mcp:` reference in `env` instead",
            hit.reason
        ));
    }
    Ok(())
}

fn validate_http(url: &str, auth: Option<&super::schema::HttpAuth>) -> Result<(), String> {
    // The proxy is the one place the upstream rules live, so the loader asks it
    // rather than keeping a second copy that can drift.
    buzz_mcp_launch_pkg::proxy::upstream::validate_upstream(url).map_err(|e| e.to_string())?;
    if let Some(auth) = auth {
        if !PINNED_AUTH_SCHEMES.contains(&auth.scheme.as_str()) {
            return Err(format!(
                "`{}` is not a pinned auth scheme (expected one of {})",
                auth.scheme,
                PINNED_AUTH_SCHEMES.join(", ")
            ));
        }
        McpSecretRef::parse(&auth.secret)
            .map_err(|e| format!("its credential reference is invalid: {e}"))?;
    }
    Ok(())
}

fn validate_env(env: &BTreeMap<String, String>) -> Result<(), String> {
    if env.len() > MAX_ENV_ENTRIES {
        return Err(format!(
            "it declares {} env entries, over the {MAX_ENV_ENTRIES} cap",
            env.len()
        ));
    }
    for (name, value) in env {
        if name.is_empty() || name.len() > MAX_ENV_NAME_LEN {
            return Err(format!(
                "the name of one env entry is {} bytes; a name must be 1 to \
                 {MAX_ENV_NAME_LEN} bytes",
                name.len()
            ));
        }
        if name.as_bytes().contains(&0)
            || name.as_bytes().contains(&b'=')
            || value.as_bytes().contains(&0)
        {
            return Err(format!(
                "`{name}` holds a NUL or `=`, which no `NAME=VALUE` argument can carry"
            ));
        }
        if value.len() > MAX_ENV_VALUE_LEN {
            return Err(format!(
                "the value of `{name}` is {} bytes, over the {MAX_ENV_VALUE_LEN}-byte cap",
                value.len()
            ));
        }
        if McpSecretRef::parse(value).is_ok() {
            continue;
        }
        if value.starts_with(buzz_secret_store_pkg::namespace::MCP_NAMESPACE_PREFIX) {
            return Err(format!(
                "the reference in `{name}` is not a valid `mcp:` reference"
            ));
        }
        if PROXY_VARIABLES.contains(&name.as_str())
            && buzz_mcp_launch_pkg::env::proxy_value_carries_userinfo(value)
        {
            return Err(format!(
                "`{name}` carries userinfo; an authenticated proxy URL is itself a credential, so declare it as an `mcp:` reference"
            ));
        }
        if is_credential_name(name) {
            return Err(format!(
                "`{name}` reads as a credential but carries a literal value; declare an `mcp:` reference instead"
            ));
        }
        if let Some(hit) = scan_value(value) {
            return Err(format!(
                "the value of `{name}` {}; declare an `mcp:` reference instead",
                hit.reason
            ));
        }
    }
    Ok(())
}
