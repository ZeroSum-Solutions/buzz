//! Resolution of local file paths written as inline code in a message.
//!
//! Agents post report paths as text (`audit/verify/report.md`,
//! `buzz/approvals/item-7.html`). The webview decides which inline-code tokens
//! *look* like a path (`shared/ui/markdown/pathLinks.ts`); this module is the
//! only place that touches a filesystem, and it does so once per hover or
//! click, never while a channel renders.
//!
//! Containment contract: a candidate resolves only when its *canonical* form
//! is a regular file whose canonical path lies under a canonical allowed root.
//! Canonicalizing both sides is what makes `..` and an escaping symlink
//! equivalent to any other path outside the root — both land somewhere the
//! prefix check rejects. A candidate that names nothing is not an error: it is
//! "not a link", and the token stays plain text.
//!
//! Containment bounds *which* path a message can name. It says nothing about
//! what the handler does with that path, so this module bounds that too. The
//! OS default handler runs an executable, a `.command` script or a `.webloc`
//! shortcut, and `$HOME/projects` is full of all three — so a resolved file is
//! offered only when its extension is on [`OPENABLE_EXTENSIONS`] and it
//! carries no executable bit. Nothing here executes the file, and nothing
//! reaches the opener that the resolver has not just re-proven contained.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;

/// Maximum UTF-8 byte length of a candidate this module will look at.
///
/// Message content is relay-sourced and unbounded, so the cap is enforced at
/// the DTO — before any filesystem call — and mirrored in the webview's
/// `MAX_PATH_LINK_BYTES` so an over-length token never reaches IPC at all.
pub const MAX_PATH_LINK_BYTES: usize = 4096;

/// Maximum UTF-8 byte length of the sender pubkey carried alongside a
/// candidate. A pubkey is 64 hex characters; the cap leaves room for prefixed
/// forms without admitting an arbitrary relay-sourced string.
const MAX_SENDER_PUBKEY_BYTES: usize = 128;

/// Maximum size of a markdown document opened in the in-app viewer.
///
/// Matches the relay-attachment viewer's `MAX_MARKDOWN_DOC_BYTES`
/// (`media_download.rs`, `shared/ui/markdown/markdownDocFile.ts`) so a local
/// document and a shared one hit the same wall. A larger `.md` is still a
/// link — it opens with the OS handler instead of the panel.
pub const MAX_PATH_LINK_MARKDOWN_BYTES: u64 = 2 * 1024 * 1024;

/// Extensions the in-app markdown viewer renders.
const MARKDOWN_EXTENSIONS: [&str; 3] = ["md", "markdown", "mdx"];

/// Extensions a resolved path link may be offered for.
///
/// Mirrors `DOCUMENT_EXTENSIONS` in `shared/ui/markdown/pathLinks.ts`, and is
/// the guard that keeps a message from choosing what the OS default handler
/// runs. macOS `open` hands a file to LaunchServices, which *executes* a
/// `.command`, a `.sh` with an executable bit or any Mach-O binary, and
/// silently fetches the URL inside a `.webloc` or `.url`; Windows does the
/// same for `.exe`, `.cmd` and `.lnk`. `$HOME/projects` holds thousands of
/// those files, so an extension outside this inert-document list is "not a
/// link" rather than something a single click hands to the opener.
const OPENABLE_EXTENSIONS: [&str; 13] = [
    "md", "markdown", "mdx", "html", "htm", "pdf", "csv", "json", "txt", "log", "yml", "yaml",
    "toml",
];

/// How a resolved path link opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PathLinkKind {
    /// A markdown document within the viewer cap: opens in the viewer panel.
    Markdown,
    /// Another inert document type: opens with the OS default handler.
    File,
}

/// A candidate that resolved to a real file inside an allowed root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PathLinkTarget {
    /// Canonical absolute path. Always valid UTF-8: a canonical path that is
    /// not is refused, so this string re-resolves to the same file when the
    /// webview sends it back to open or read the document.
    pub path: String,
    /// Final path component, for the link title and the panel header.
    pub filename: String,
    /// Whether this opens in the viewer panel or with the OS handler.
    pub kind: PathLinkKind,
    /// Size of the resolved file in bytes.
    pub size_bytes: u64,
}

/// The working directory a message's sender writes into, when the desktop can
/// map that sender's pubkey to one.
///
/// There is no such mapping today. Every managed agent is spawned in the same
/// directory (`managed_agents::default_agent_workdir`, applied at
/// `managed_agents/runtime.rs:622`) and `ManagedAgentRecord` carries no
/// per-agent working directory, so a sender pubkey selects nothing and the
/// two shared roots below are the whole list. The seam is kept, and asserted
/// in `path_links_tests.rs`, so a future per-sender working directory has one
/// place to land and one test that records the change.
pub(super) fn sender_workdir_root(_sender_pubkey: Option<&str>) -> Option<PathBuf> {
    None
}

/// The nest (`~/.buzz`), which is the working directory every managed agent
/// is actually spawned in.
///
/// `managed_agents::default_agent_workdir` prefers the nest and falls back to
/// `$HOME`; only the nest half is a root here, because the fallback would
/// admit the whole home directory. A relative path an agent writes is
/// relative to this directory, so without it a relative candidate could only
/// ever mean "under `$HOME/projects`".
fn nest_root() -> Option<PathBuf> {
    crate::managed_agents::nest_dir()
}

/// `$HOME/projects`, where the reports these links point at are written.
fn projects_root() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join("projects"))
}

/// The canonical roots a candidate from `sender_pubkey` may resolve inside.
///
/// A root that does not exist, or cannot be canonicalized, is dropped: on a
/// machine with neither `~/.buzz` nor `$HOME/projects` the list is empty and
/// every candidate is "not a link", which is the correct answer rather than
/// an error. `$HOME` itself is never a root.
pub(super) fn path_link_roots(sender_pubkey: Option<&str>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for root in sender_workdir_root(sender_pubkey)
        .into_iter()
        .chain(nest_root())
        .chain(projects_root())
    {
        if let Ok(canonical) = root.canonicalize() {
            if canonical.is_dir() && !roots.contains(&canonical) {
                roots.push(canonical);
            }
        }
    }
    roots
}

/// Refuse a sender pubkey longer than the DTO admits.
fn check_sender_pubkey(sender_pubkey: Option<&str>) -> Result<(), String> {
    match sender_pubkey {
        Some(pubkey) if pubkey.len() > MAX_SENDER_PUBKEY_BYTES => Err(format!(
            "sender pubkey exceeds {MAX_SENDER_PUBKEY_BYTES} bytes"
        )),
        _ => Ok(()),
    }
}

fn extension_lowercase(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
}

fn markdown_extension(path: &Path) -> bool {
    extension_lowercase(path)
        .map(|extension| MARKDOWN_EXTENSIONS.contains(&extension.as_str()))
        .unwrap_or(false)
}

/// True when the file's extension is one the OS handler treats as a document.
fn openable_extension(path: &Path) -> bool {
    extension_lowercase(path)
        .map(|extension| OPENABLE_EXTENSIONS.contains(&extension.as_str()))
        .unwrap_or(false)
}

/// True when the file carries any executable bit.
///
/// The second half of the "what does the handler do with it" guard: a
/// `report.md` that is also `chmod +x` is an anomaly for a file a *message*
/// named, and refusing it costs a real document nothing. Windows has no such
/// bit, so there it is always false and the extension allowlist stands alone.
#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &std::fs::Metadata) -> bool {
    false
}

/// Refuse, by inspection alone, a candidate whose *lexical* form can name
/// something outside the roots or off this machine.
///
/// This runs before `canonicalize`, because `canonicalize` is itself the
/// dangerous call: `Path::is_absolute` accepts a Windows UNC path, and
/// canonicalizing `\\attacker\share\note.md` opens an SMB connection and an
/// NTLM handshake to a host named by relay-sourced text — credential exposure
/// driven by a message, before any containment logic has run. The shapes that
/// can do that are therefore rejected without touching a filesystem:
///
/// - a backslash anywhere: UNC (`\\host\share`), verbatim (`\\?\`), device
///   (`\\.\`) and every Windows spelling of a drive-relative path;
/// - a leading `//`, the forward-slash spelling of a UNC share;
/// - a `Prefix` or `RootDir` component on a candidate that is not absolute —
///   `C:file` is *relative* on Windows yet replaces the whole left-hand side
///   of a `join`;
/// - a `..` component, which a join would resolve outside the root.
pub(super) fn is_lexically_refused(candidate: &str) -> bool {
    if candidate.contains('\\') || candidate.starts_with("//") {
        return true;
    }
    let path = Path::new(candidate);
    let absolute = path.is_absolute();
    path.components().any(|component| match component {
        Component::ParentDir => true,
        Component::Prefix(_) | Component::RootDir => !absolute,
        _ => false,
    })
}

/// Resolve `candidate` against already-canonical `roots`.
///
/// `Err` means the candidate was refused outright (over the byte cap).
/// `Ok(None)` means "not a link" — a missing file, a directory, a path
/// outside every root, a symlink whose target leaves its root, or a file the
/// OS handler would run rather than display. `Ok(Some(_))` is an inert,
/// non-executable regular file inside a root, named by its canonical path.
pub(super) fn resolve_within_roots(
    candidate: &str,
    roots: &[PathBuf],
) -> Result<Option<PathLinkTarget>, String> {
    // Enforced before any filesystem call: an over-length candidate never
    // reaches `canonicalize`.
    if candidate.len() > MAX_PATH_LINK_BYTES {
        return Err(format!(
            "path link candidate exceeds {MAX_PATH_LINK_BYTES} bytes"
        ));
    }
    if candidate.is_empty() || candidate.contains('\0') {
        return Ok(None);
    }
    // Lexical refusal comes first, so no network or device path is ever
    // handed to `canonicalize`.
    if is_lexically_refused(candidate) {
        return Ok(None);
    }

    let candidate_path = Path::new(candidate);
    let absolute = candidate_path.is_absolute();
    for root in roots {
        let joined = if absolute {
            // Lexical containment, again before any filesystem call: an
            // absolute candidate that is not spelled under this root is not
            // canonicalized against it at all.
            if !candidate_path.starts_with(root) {
                continue;
            }
            candidate_path.to_path_buf()
        } else {
            root.join(candidate)
        };
        // Canonicalizing resolves every symlink, so a candidate that leaves
        // its root — however it left — fails the prefix check below.
        let Ok(canonical) = joined.canonicalize() else {
            continue;
        };
        // Component-wise: `/a/bc` does not start with `/a/b`, so the prefix is
        // always a whole path segment.
        if !canonical.starts_with(root) {
            continue;
        }
        let Ok(metadata) = canonical.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        // Containment decided *which* file. These two decide what handing it
        // to the OS default handler can do.
        if !openable_extension(&canonical) || is_executable(&metadata) {
            continue;
        }
        // A canonical path that is not UTF-8 could not survive the round trip
        // back through the webview, so it is not offered as a link.
        let (Some(path), Some(filename)) = (
            canonical.to_str().map(str::to_owned),
            canonical
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned),
        ) else {
            continue;
        };
        let size_bytes = metadata.len();
        let kind = if markdown_extension(&canonical) && size_bytes <= MAX_PATH_LINK_MARKDOWN_BYTES {
            PathLinkKind::Markdown
        } else {
            PathLinkKind::File
        };
        return Ok(Some(PathLinkTarget {
            path,
            filename,
            kind,
            size_bytes,
        }));
    }
    Ok(None)
}

/// Read at most `limit` bytes from `path`.
///
/// The bound is on the bytes this call actually reads, not on a size some
/// earlier call looked up: a file that grew in between still costs at most
/// `limit` bytes of memory here.
pub(super) fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let file =
        std::fs::File::open(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    Ok(bytes)
}

/// Read a resolved markdown document, bounded by the bytes actually read.
///
/// The file is re-opened here rather than trusting the size recorded at
/// resolve time, and one byte past the cap is read so a document that grew in
/// between is refused instead of silently truncated.
pub(super) fn read_markdown_within_cap(path: &Path) -> Result<String, String> {
    let bytes = read_bounded(path, MAX_PATH_LINK_MARKDOWN_BYTES + 1)?;
    if bytes.len() as u64 > MAX_PATH_LINK_MARKDOWN_BYTES {
        return Err("This file is too large to preview.".to_string());
    }
    String::from_utf8(bytes)
        .map_err(|_| "This file isn't valid text, so it can't be previewed.".to_string())
}

pub(super) fn resolve_blocking(
    candidate: &str,
    sender_pubkey: Option<&str>,
) -> Result<Option<PathLinkTarget>, String> {
    check_sender_pubkey(sender_pubkey)?;
    resolve_within_roots(candidate, &path_link_roots(sender_pubkey))
}

fn require_target(target: Option<PathLinkTarget>) -> Result<PathLinkTarget, String> {
    target.ok_or_else(|| "That file is no longer on this Mac.".to_string())
}

/// Re-resolve `candidate` against `roots` and hand the *resolved* target to
/// `open`.
///
/// The webview holds a resolved path across a hover, a message edit and a
/// click, so containment is re-proven here at the moment of opening rather
/// than trusted from whatever the webview kept. `open` is reached only for an
/// inert, non-executable regular file inside a root; `open_path_link` is this
/// function plus the OS handler, so the re-resolution cannot be removed
/// without removing the call the tests drive.
pub(super) fn open_resolved_path_link<F>(
    candidate: &str,
    sender_pubkey: Option<&str>,
    roots: &[PathBuf],
    open: F,
) -> Result<(), String>
where
    F: FnOnce(&PathLinkTarget) -> Result<(), String>,
{
    check_sender_pubkey(sender_pubkey)?;
    let target = require_target(resolve_within_roots(candidate, roots)?)?;
    open(&target)
}

/// Re-resolve `candidate` against `roots` and read it as a viewer document.
///
/// Refuses anything the resolver did not classify as a viewer-sized markdown
/// document, so the panel cannot be pointed at a PDF or an oversized file.
pub(super) fn read_markdown_path_link(
    candidate: &str,
    sender_pubkey: Option<&str>,
    roots: &[PathBuf],
) -> Result<String, String> {
    check_sender_pubkey(sender_pubkey)?;
    let target = require_target(resolve_within_roots(candidate, roots)?)?;
    if target.kind != PathLinkKind::Markdown {
        return Err("That file is not a markdown document the viewer can render.".to_string());
    }
    read_markdown_within_cap(Path::new(&target.path))
}

/// Resolve one inline-code token to a local file, or report that it is not a
/// link.
///
/// Called on hover or click only. `Ok(None)` is the ordinary answer for a
/// token that names nothing openable and renders as plain text; an `Err` is a
/// refusal the caller surfaces.
#[tauri::command]
pub async fn resolve_path_link(
    candidate: String,
    sender_pubkey: Option<String>,
) -> Result<Option<PathLinkTarget>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        resolve_blocking(&candidate, sender_pubkey.as_deref())
    })
    .await
    .map_err(|error| format!("path link resolve task failed: {error}"))?
}

/// Open a resolved path link with the OS default handler.
///
/// The candidate is resolved again here rather than trusting a path the
/// webview held on to, so containment and the openable-type check are
/// re-proven at the moment the file is opened. The file is handed to the
/// opener, never executed.
#[tauri::command]
pub async fn open_path_link(
    candidate: String,
    sender_pubkey: Option<String>,
    app: AppHandle,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let sender_pubkey = sender_pubkey.as_deref();
        let roots = path_link_roots(sender_pubkey);
        open_resolved_path_link(&candidate, sender_pubkey, &roots, |target| {
            app.opener()
                .open_path(&target.path, None::<&str>)
                .map_err(|error| format!("open {}: {error}", target.filename))
        })
    })
    .await
    .map_err(|error| format!("path link open task failed: {error}"))?
}

/// Read a local markdown document for the in-app viewer panel.
///
/// Re-resolves the candidate (same containment check as `resolve_path_link`),
/// refuses anything the resolver did not classify as a viewer-sized markdown
/// document, and bounds the read at [`MAX_PATH_LINK_MARKDOWN_BYTES`].
#[tauri::command]
pub async fn read_path_link_markdown(
    candidate: String,
    sender_pubkey: Option<String>,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let sender_pubkey = sender_pubkey.as_deref();
        let roots = path_link_roots(sender_pubkey);
        read_markdown_path_link(&candidate, sender_pubkey, &roots)
    })
    .await
    .map_err(|error| format!("path link read task failed: {error}"))?
}
