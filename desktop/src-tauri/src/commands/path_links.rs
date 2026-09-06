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
//! Nothing here executes the file. `open_path_link` hands the resolved path to
//! the OS default handler through the same `tauri_plugin_opener` call the
//! project pane already uses; the relay is never consulted.

use std::io::Read;
use std::path::{Path, PathBuf};

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

/// How a resolved path link opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PathLinkKind {
    /// A markdown document within the viewer cap: opens in the viewer panel.
    Markdown,
    /// Anything else: opens with the OS default handler.
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
/// per-agent working directory, so a sender pubkey selects nothing and
/// `$HOME/projects` stays the only root. The seam is kept, and asserted in
/// `path_links_tests.rs`, so a future per-sender working directory has one
/// place to land and one test that records the change.
pub(super) fn sender_workdir_root(_sender_pubkey: Option<&str>) -> Option<PathBuf> {
    None
}

/// `$HOME/projects`, the root every sender is allowed to link into.
fn projects_root() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join("projects"))
}

/// The canonical roots a candidate from `sender_pubkey` may resolve inside.
///
/// A root that does not exist, or cannot be canonicalized, is dropped: on a
/// machine with no `$HOME/projects` the list is empty and every candidate is
/// "not a link", which is the correct answer rather than an error.
fn path_link_roots(sender_pubkey: Option<&str>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for root in sender_workdir_root(sender_pubkey)
        .into_iter()
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

fn markdown_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            let lowered = extension.to_ascii_lowercase();
            MARKDOWN_EXTENSIONS.contains(&lowered.as_str())
        })
        .unwrap_or(false)
}

/// Resolve `candidate` against already-canonical `roots`.
///
/// `Err` means the candidate was refused outright (over the byte cap).
/// `Ok(None)` means "not a link" — a missing file, a directory, a path outside
/// every root, or a symlink whose target leaves its root. `Ok(Some(_))` is a
/// regular file inside a root, named by its canonical path.
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

    for root in roots {
        let joined = if Path::new(candidate).is_absolute() {
            PathBuf::from(candidate)
        } else {
            root.join(candidate)
        };
        // Canonicalizing resolves `..` and every symlink, so a candidate that
        // leaves its root — however it left — fails the prefix check below.
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

/// Read a resolved markdown document, bounded by the bytes actually read.
///
/// The file is re-opened here rather than trusting the size recorded at
/// resolve time, and one byte past the cap is read so a document that grew in
/// between is refused instead of silently truncated.
pub(super) fn read_markdown_within_cap(path: &Path) -> Result<String, String> {
    let file =
        std::fs::File::open(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_PATH_LINK_MARKDOWN_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if bytes.len() as u64 > MAX_PATH_LINK_MARKDOWN_BYTES {
        return Err("This file is too large to preview.".to_string());
    }
    String::from_utf8(bytes)
        .map_err(|_| "This file isn't valid text, so it can't be previewed.".to_string())
}

fn resolve_blocking(
    candidate: &str,
    sender_pubkey: Option<&str>,
) -> Result<Option<PathLinkTarget>, String> {
    check_sender_pubkey(sender_pubkey)?;
    resolve_within_roots(candidate, &path_link_roots(sender_pubkey))
}

fn require_target(target: Option<PathLinkTarget>) -> Result<PathLinkTarget, String> {
    target.ok_or_else(|| "That file is no longer on this Mac.".to_string())
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
/// webview held on to, so containment is re-proven at the moment the file is
/// opened. The file is handed to the opener, never executed.
#[tauri::command]
pub async fn open_path_link(
    candidate: String,
    sender_pubkey: Option<String>,
    app: AppHandle,
) -> Result<(), String> {
    let target = tauri::async_runtime::spawn_blocking(move || {
        resolve_blocking(&candidate, sender_pubkey.as_deref())
    })
    .await
    .map_err(|error| format!("path link open task failed: {error}"))??;
    let target = require_target(target)?;
    app.opener()
        .open_path(&target.path, None::<&str>)
        .map_err(|error| format!("open {}: {error}", target.filename))
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
        let target = require_target(resolve_blocking(&candidate, sender_pubkey.as_deref())?)?;
        if target.kind != PathLinkKind::Markdown {
            return Err("That file is not a markdown document the viewer can render.".to_string());
        }
        read_markdown_within_cap(Path::new(&target.path))
    })
    .await
    .map_err(|error| format!("path link read task failed: {error}"))?
}
