//! Export a rendered markdown document as a PDF.
//!
//! Route per `docs/plans/2026-09-04-pdf-route.md` (ticket T8): the document's
//! already-rendered, document-mode HTML arrives from the frontend, is wrapped
//! here in a self-contained print document (semantic markup plus the print
//! stylesheet next to this file), and is printed by a local Chrome or
//! Chromium through the DevTools `Page.printToPDF` method. Tauri's own
//! `WebviewWindow::print()` has no bytes-out API on this `wry` pin and its
//! only headless alternative needs `unsafe` Objective-C interop, so it is not
//! a route on this fork — see the memo's "Route (a)".
//!
//! The export never reaches the network. Two independent guards say so: the
//! print document carries a `default-src 'none'` content-security policy that
//! permits only `data:` images and the inline stylesheet, and the browser is
//! launched with DNS resolution mapped to `~NOTFOUND` and every inherited
//! proxy variable blanked. A remote image in a document therefore degrades to
//! a visible placeholder instead of a silent fetch.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use headless_chrome::types::PrintToPdfOptions;
use headless_chrome::{Browser, LaunchOptionsBuilder};

use crate::commands::export_util::pick_save_path;
use crate::commands::media_filename::sanitize_filename;

/// The print stylesheet. The exported HTML carries no application classes, so
/// this is the document's only styling — it must stand on its own.
const PRINT_CSS: &str = include_str!("pdf_export_print.css");

/// Content-security policy embedded in every print document. `default-src
/// 'none'` denies scripts, frames, and every remote subresource; only `data:`
/// images and the inline `<style>` block above are allowed.
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; img-src data:; \
     style-src 'unsafe-inline'; font-src data:; base-uri 'none'; form-action 'none'";

/// Extra Chrome flags for an export. `--host-resolver-rules` fails every DNS
/// lookup, so the renderer cannot reach a host even if a future document
/// slipped past the policy above; `--no-proxy-server` keeps it off any proxy
/// the machine happens to have configured.
const CHROME_LAUNCH_ARGS: [&str; 3] = [
    "--host-resolver-rules=MAP * ~NOTFOUND",
    "--no-proxy-server",
    "--disable-remote-fonts",
];

/// Proxy variables blanked in the browser child's environment. `Command::envs`
/// adds to the inherited environment rather than replacing it, so every
/// variable that could steer the child is named here explicitly.
const PROXY_ENV_VARS: [&str; 8] = [
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "FTP_PROXY",
    "ftp_proxy",
];

/// Upper bound on the document-mode HTML accepted from the frontend.
///
/// The viewer this export is reached from refuses any markdown source over
/// 2 MiB (`fetch_markdown_doc_bytes`), and rendering markdown to HTML expands
/// it by a small constant factor, so 8 MiB is generous headroom over anything
/// legitimate while still bounding the parse the browser is asked to do.
pub(crate) const MAX_DOCUMENT_HTML_BYTES: usize = 8 * 1024 * 1024;

/// Upper bound, in characters, on the title placed in the document head.
pub(crate) const MAX_TITLE_CHARS: usize = 200;

/// Upper bound, in characters, on the suggested filename's stem.
pub(crate) const MAX_FILENAME_CHARS: usize = 120;

/// Upper bound on the PDF accepted back from the browser, and so on the bytes
/// this command will write. Exceeding it is an error the user sees, never a
/// silent truncation.
pub(crate) const MAX_PDF_BYTES: usize = 64 * 1024 * 1024;

/// Bound on any single DevTools round trip (navigate, print).
const PAGE_TIMEOUT: Duration = Duration::from_secs(45);

/// Bound on how long an idle browser process may linger before it is killed.
const BROWSER_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Escape text for insertion into HTML element or attribute content.
fn escape_html_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Bound a user- or relay-sourced string to `max` characters.
fn clamp_chars(value: &str, max: usize) -> String {
    value.trim().chars().take(max).collect()
}

/// Derive the suggested save-dialog filename from the source document's name:
/// basename only, no control characters, bounded stem, always `.pdf`.
pub(crate) fn pdf_filename(source: &str) -> String {
    let base = sanitize_filename(source);
    let stem = match base.rsplit_once('.') {
        Some((stem, _ext)) => stem,
        None => base.as_str(),
    };
    let stem: String = stem.trim().chars().take(MAX_FILENAME_CHARS).collect();
    let stem = stem.trim().to_string();
    if stem.is_empty() {
        "document.pdf".to_string()
    } else {
        format!("{stem}.pdf")
    }
}

/// Build the self-contained print document around a document-mode HTML body.
///
/// The body is already-escaped markup produced by the desktop renderer; the
/// title is untrusted text and is escaped here.
pub(crate) fn build_print_document(title: &str, body_html: &str) -> String {
    format!(
        "<!doctype html>\n\
         <html lang=\"en\">\n\
         <head>\n\
         <meta charset=\"utf-8\">\n\
         <meta http-equiv=\"Content-Security-Policy\" content=\"{policy}\">\n\
         <title>{title}</title>\n\
         <style>\n{PRINT_CSS}</style>\n\
         </head>\n\
         <body>\n\
         <article class=\"buzz-pdf-document\">\n{body_html}\n</article>\n\
         </body>\n\
         </html>\n",
        policy = escape_html_text(CONTENT_SECURITY_POLICY),
        title = escape_html_text(title),
    )
}

/// One validated export request: everything relay- or user-sourced has been
/// capped before any browser or filesystem work starts.
struct PdfExportRequest {
    document: String,
    filename: String,
}

impl PdfExportRequest {
    fn new(body_html: &str, title: &str, filename: &str) -> Result<Self, String> {
        if body_html.trim().is_empty() {
            return Err("This document is empty, so there is nothing to export.".to_string());
        }
        if body_html.len() > MAX_DOCUMENT_HTML_BYTES {
            return Err(format!(
                "This document is too large to export as a PDF (limit {} MiB, got {} MiB).",
                MAX_DOCUMENT_HTML_BYTES / (1024 * 1024),
                body_html.len() / (1024 * 1024),
            ));
        }
        Ok(Self {
            document: build_print_document(&clamp_chars(title, MAX_TITLE_CHARS), body_html),
            filename: pdf_filename(filename),
        })
    }
}

/// The environment the browser child process is started with.
///
/// `headless_chrome` hands this map to `Command::envs`, which merges into the
/// inherited environment rather than replacing it, so the map names every
/// variable that must not be taken from whatever shell launched Buzz: the
/// proxy variables are blanked and `no_proxy` is set to `*` so an inherited
/// proxy cannot route an export through a network hop, and `HOME` points at
/// the export's own scratch directory so the child reads none of the user's
/// browser state.
fn chrome_process_env(scratch_dir: &Path) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = PROXY_ENV_VARS
        .iter()
        .map(|key| ((*key).to_string(), String::new()))
        .collect();
    env.insert("NO_PROXY".to_string(), "*".to_string());
    env.insert("no_proxy".to_string(), "*".to_string());
    env.insert("HOME".to_string(), scratch_dir.display().to_string());
    env
}

/// Print options: US Letter, backgrounds on, margins owned by the `@page`
/// rule in the stylesheet rather than by the browser.
fn print_options() -> PrintToPdfOptions {
    PrintToPdfOptions {
        landscape: Some(false),
        display_header_footer: Some(false),
        print_background: Some(true),
        scale: None,
        paper_width: Some(8.5),
        paper_height: Some(11.0),
        margin_top: Some(0.0),
        margin_bottom: Some(0.0),
        margin_left: Some(0.0),
        margin_right: Some(0.0),
        page_ranges: None,
        ignore_invalid_page_ranges: None,
        header_template: None,
        footer_template: None,
        prefer_css_page_size: Some(true),
        transfer_mode: None,
        generate_document_outline: None,
        generate_tagged_pdf: None,
    }
}

/// Locate the Chrome or Chromium the export will drive.
fn chrome_executable() -> Result<PathBuf, String> {
    headless_chrome::browser::default_executable().map_err(|e| {
        format!("PDF export needs Google Chrome or Chromium installed on this machine ({e}).")
    })
}

/// Render one print document to PDF bytes with the browser at `chrome`.
///
/// Blocking: drives a child process over the DevTools protocol. Every failure
/// is returned with the stage that produced it; nothing is written to disk
/// outside the scratch directory, which is removed when this returns.
fn render_print_document(chrome: &Path, document: &str) -> Result<Vec<u8>, String> {
    if !chrome.is_file() {
        return Err(format!(
            "PDF export could not start a browser: {} is not an executable file.",
            chrome.display()
        ));
    }

    let scratch = tempfile::Builder::new()
        .prefix("buzz-pdf-export-")
        .tempdir()
        .map_err(|e| format!("PDF export could not create a scratch directory: {e}"))?;
    let page_path = scratch.path().join("document.html");
    std::fs::write(&page_path, document)
        .map_err(|e| format!("PDF export could not stage the document: {e}"))?;
    let page_url = url::Url::from_file_path(&page_path)
        .map_err(|()| "PDF export could not address the staged document.".to_string())?;

    let args: Vec<&OsStr> = CHROME_LAUNCH_ARGS.iter().map(OsStr::new).collect();
    let options = LaunchOptionsBuilder::default()
        .path(Some(chrome.to_path_buf()))
        .headless(true)
        .sandbox(true)
        .ignore_certificate_errors(false)
        .idle_browser_timeout(BROWSER_IDLE_TIMEOUT)
        .args(args)
        .process_envs(Some(chrome_process_env(scratch.path())))
        .build()
        .map_err(|e| format!("PDF export could not configure the browser: {e}"))?;

    let browser =
        Browser::new(options).map_err(|e| format!("PDF export could not start Chrome: {e}"))?;
    let tab = browser
        .new_tab()
        .map_err(|e| format!("PDF export could not open a page: {e}"))?;
    let tab = tab.set_default_timeout(PAGE_TIMEOUT);
    tab.navigate_to(page_url.as_str())
        .map_err(|e| format!("PDF export could not load the document: {e}"))?;
    tab.wait_until_navigated()
        .map_err(|e| format!("PDF export timed out loading the document: {e}"))?;
    let bytes = tab
        .print_to_pdf(Some(print_options()))
        .map_err(|e| format!("PDF export could not print the document: {e}"))?;

    if bytes.len() > MAX_PDF_BYTES {
        return Err(format!(
            "The exported PDF is too large to save (limit {} MiB, got {} MiB).",
            MAX_PDF_BYTES / (1024 * 1024),
            bytes.len() / (1024 * 1024),
        ));
    }
    Ok(bytes)
}

/// Write the PDF through a staged temp file committed by rename, so the
/// chosen path only ever holds a complete document — a failed or partial
/// write leaves the previous file, or no file, untouched.
fn write_pdf_atomically(dest: &Path, bytes: &[u8]) -> Result<(), String> {
    use atomic_write_file::AtomicWriteFile;

    let mut file = AtomicWriteFile::open(dest)
        .map_err(|e| format!("Failed to open {} for writing: {e}", dest.display()))?;
    file.write_all(bytes)
        .map_err(|e| format!("Failed to write the PDF: {e}"))?;
    file.commit()
        .map_err(|e| format!("Failed to save the PDF: {e}"))?;
    Ok(())
}

/// Export a rendered markdown document as a PDF through the native save
/// dialog.
///
/// `body_html` is the document-mode HTML produced by the desktop renderer,
/// `title` the document's display name, `filename` the source attachment's
/// name (only its basename is used, and the suggested name always ends
/// `.pdf`). Returns `Ok(true)` when a file was written and `Ok(false)` when
/// the user cancelled the save dialog — the cancel path renders nothing and
/// writes nothing.
#[tauri::command]
pub async fn export_document_pdf(
    body_html: String,
    title: String,
    filename: String,
    app: tauri::AppHandle,
) -> Result<bool, String> {
    let request = PdfExportRequest::new(&body_html, &title, &filename)?;

    let dest = match pick_save_path(&app, &request.filename, "PDF document", &["pdf"]).await? {
        Some(path) => path,
        None => return Ok(false),
    };

    let document = request.document;
    let bytes = tauri::async_runtime::spawn_blocking(move || {
        let chrome = chrome_executable()?;
        render_print_document(&chrome, &document)
    })
    .await
    .map_err(|e| format!("PDF export did not finish: {e}"))??;

    write_pdf_atomically(&dest, &bytes)?;
    Ok(true)
}

#[cfg(test)]
#[path = "pdf_export_tests.rs"]
mod tests;
