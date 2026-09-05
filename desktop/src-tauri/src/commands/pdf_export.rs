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
//! This module owns the browser child process rather than delegating the
//! spawn to `headless_chrome::Browser::new`. Three properties depend on that
//! ownership and none of them is available through the library's launcher:
//!
//! 1. **An absolute launch deadline.** The pinned `headless_chrome` 1.0.22
//!    wraps its 30 s launch wait (`process.rs` `ws_url_from_output`) around a
//!    blocking `BufRead::lines()` scan of the child's stderr, and its
//!    `Wait::until` helper only compares the elapsed time *between* predicate
//!    calls (`util.rs`). A browser that starts, holds stderr open and never
//!    prints the DevTools banner parks that scan forever, so the timeout
//!    never fires. [`launch_chrome`] moves the scan to its own thread and
//!    bounds the launch with a channel deadline that holds regardless of what
//!    the child does.
//! 2. **Whole-tree containment.** The library's `TemporaryProcess::drop` kills
//!    the direct PID only and discards every error, so Chrome's renderer, GPU
//!    and zygote children are not owned by anything. [`ChromeChild`] puts the
//!    child in its own process group on Unix and in a kill-on-close Job Object
//!    on Windows, and every teardown failure is returned to the caller instead
//!    of logged away.
//! 3. **Nothing written outside the scratch directory.** The library creates
//!    its own `rust-headless-chrome-profile*` temp directory when
//!    `user_data_dir` is unset; this module points the profile, disk cache and
//!    crash dumps at its own scratch directory, which is removed before the
//!    export returns.
//!
//! The export never reaches the network. Two independent guards say so: the
//! print document carries a `default-src 'none'` content-security policy that
//! permits only `data:` images and the inline stylesheet, and the browser is
//! launched with DNS resolution mapped to `~NOTFOUND`, no proxy, and a fully
//! explicit environment built from empty (`env_clear`) so nothing about the
//! shell that started Buzz can steer it. A remote image in a document
//! therefore degrades to a visible placeholder instead of a silent fetch.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

use headless_chrome::types::PrintToPdfOptions;
use headless_chrome::Browser;

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

/// Flags that keep the export's browser out of the user's profile, off every
/// background service, and out of any first-run interaction.
///
/// `--force-color-profile=srgb` is here for reproducibility rather than
/// isolation: without it the raster depends on the display profile of whatever
/// machine ran the export, and the recorded per-page hashes for the T9 fixture
/// would not compare across machines.
const CHROME_ISOLATION_ARGS: [&str; 10] = [
    "--headless",
    "--remote-debugging-port=0",
    "--no-first-run",
    "--no-default-browser-check",
    "--disable-gpu",
    "--disable-dev-shm-usage",
    "--disable-background-networking",
    "--disable-extensions",
    "--disable-sync",
    "--force-color-profile=srgb",
];

/// Proxy variables blanked in the browser child's environment.
///
/// The child's environment is built from empty ([`chrome_process_env`] is
/// applied after `Command::env_clear`), so these are already absent; naming
/// and blanking them keeps the guard explicit and falsifiable if the child is
/// ever spawned from an inherited environment again.
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

/// The only `PATH` a Unix browser child gets. A fixed value, not the
/// harness's, so nothing on the launching shell's `PATH` can be resolved by
/// the child.
const CHROME_UNIX_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// The Windows variables the child is given back after `env_clear`.
///
/// Windows resolves the process image and its DLLs through these, so a child
/// started without them very likely fails to start at all — the same reason
/// and the same list as `commands/media_transcode.rs`, which restores exactly
/// these for its ffmpeg child. `TEMP` and `TMP` are *not* restored from the
/// parent here: they are the Windows spelling of `TMPDIR`, and the export
/// points them at its own scratch directory instead, so the browser writes no
/// temporary file outside it.
const WINDOWS_LOADER_ENV_VARS: [&str; 2] = ["SystemRoot", "WINDIR"];

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

/// Bound on how long an idle browser connection may linger before it is
/// dropped.
const BROWSER_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Absolute bound on the launch: from spawn to the DevTools endpoint. Enforced
/// by this module's own deadline (see the module docs), so it holds even when
/// the browser never writes the banner and never closes stderr.
const BROWSER_LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound on stderr bytes scanned for the DevTools banner. Chrome prints the
/// banner within the first few kilobytes; a browser that writes more than this
/// without announcing an endpoint has failed to start.
const MAX_LAUNCH_STDERR_BYTES: u64 = 1024 * 1024;

/// The line Chrome writes to stderr once its DevTools endpoint is listening.
const DEVTOOLS_BANNER: &str = "DevTools listening on ";

/// Freeze the browser so the Job Object takes ownership before any of its code
/// runs (see [`ChromeChild::spawn`]).
#[cfg(windows)]
const CREATE_SUSPENDED: u32 = 0x0000_0004;

/// Suppress the console window a GUI-spawned console child would otherwise
/// flash.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The exact creation flags the export's browser is spawned with.
///
/// `Command::creation_flags` *replaces* rather than accumulates, so this
/// constant has to carry every flag the child needs — the same reason and the
/// same shape as `managed_agents/discovery/bounded_command.rs`.
#[cfg(windows)]
const CHROME_CREATION_FLAGS: u32 = CREATE_SUSPENDED | CREATE_NO_WINDOW;

/// Compile-time guard: the browser's flags must always carry *both* bits. An
/// edit that drops `CREATE_SUSPENDED` (reopening the spawn-to-assign race that
/// lets a renderer escape the job) or `CREATE_NO_WINDOW` (flashing a console
/// out of the GUI process) fails the build on Windows rather than shipping
/// silently.
#[cfg(windows)]
const _: () = {
    assert!(CHROME_CREATION_FLAGS & CREATE_SUSPENDED == CREATE_SUSPENDED);
    assert!(CHROME_CREATION_FLAGS & CREATE_NO_WINDOW == CREATE_NO_WINDOW);
};

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

/// Which platform's environment [`chrome_process_env_for`] builds.
///
/// Named rather than `cfg`-branched inside the builder so the Windows map can
/// be asserted on every platform: the shape of an environment that only one
/// CI lane can execute is exactly the kind of guard that otherwise ships
/// untested.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChromeEnvTarget {
    Unix,
    Windows,
}

impl ChromeEnvTarget {
    /// The platform this build targets.
    const HOST: Self = if cfg!(windows) {
        Self::Windows
    } else {
        Self::Unix
    };
}

/// The complete environment the browser child process is started with, for
/// `target`, reading the parent's variables through `inherited`.
///
/// It is applied after `Command::env_clear`, so the returned map is the
/// child's whole environment and nothing about the shell that launched Buzz
/// reaches it beyond what is named here: the proxy variables are named and
/// blanked and `no_proxy` is `*` on both platforms, so no export can be routed
/// through a network hop.
///
/// The rest is platform-specific because the two platforms need different
/// variables to start a process at all:
///
/// - **Unix**: `PATH` is a fixed list rather than the harness's, and `HOME`,
///   `TMPDIR` and the XDG directories all point at the export's own scratch
///   directory so the child reads and writes none of the user's browser state.
/// - **Windows**: none of those mean anything. The loader variables
///   ([`WINDOWS_LOADER_ENV_VARS`]) are carried over from the parent because the
///   image and DLL lookup needs them, `PATH` is rebuilt from `SystemRoot`
///   rather than inherited, and `TEMP`/`TMP` point at the scratch directory.
fn chrome_process_env_for(
    scratch_dir: &Path,
    target: ChromeEnvTarget,
    inherited: impl Fn(&str) -> Option<String>,
) -> HashMap<String, String> {
    let scratch = scratch_dir.display().to_string();
    let mut env: HashMap<String, String> = PROXY_ENV_VARS
        .iter()
        .map(|key| ((*key).to_string(), String::new()))
        .collect();
    env.insert("NO_PROXY".to_string(), "*".to_string());
    env.insert("no_proxy".to_string(), "*".to_string());
    match target {
        ChromeEnvTarget::Unix => {
            env.insert("PATH".to_string(), CHROME_UNIX_PATH.to_string());
            env.insert("LC_ALL".to_string(), "C".to_string());
            env.insert("HOME".to_string(), scratch.clone());
            env.insert("TMPDIR".to_string(), scratch.clone());
            env.insert("XDG_CONFIG_HOME".to_string(), scratch.clone());
            env.insert("XDG_CACHE_HOME".to_string(), scratch.clone());
            env.insert("XDG_DATA_HOME".to_string(), scratch);
        }
        ChromeEnvTarget::Windows => {
            for key in WINDOWS_LOADER_ENV_VARS {
                if let Some(value) = inherited(key) {
                    env.insert(key.to_string(), value);
                }
            }
            if let Some(root) = env.get("SystemRoot").cloned() {
                env.insert("PATH".to_string(), format!("{root}\\system32;{root}"));
            }
            env.insert("TEMP".to_string(), scratch.clone());
            env.insert("TMP".to_string(), scratch);
        }
    }
    env
}

/// [`chrome_process_env_for`] for the platform this build runs on, reading the
/// real parent environment.
fn chrome_process_env(scratch_dir: &Path) -> HashMap<String, String> {
    chrome_process_env_for(scratch_dir, ChromeEnvTarget::HOST, |key| {
        std::env::var(key).ok()
    })
}

/// The exact command the export spawns: the browser at `chrome`, every
/// directory it may write pointed inside `scratch_dir`, and an environment
/// built from empty.
fn chrome_command(chrome: &Path, scratch_dir: &Path) -> Command {
    let mut command = Command::new(chrome);
    command
        .args(CHROME_ISOLATION_ARGS)
        .arg(format!(
            "--user-data-dir={}",
            scratch_dir.join("profile").display()
        ))
        .arg(format!(
            "--disk-cache-dir={}",
            scratch_dir.join("cache").display()
        ))
        .arg(format!(
            "--crash-dumps-dir={}",
            scratch_dir.join("crash").display()
        ))
        .args(CHROME_LAUNCH_ARGS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .env_clear()
        .envs(chrome_process_env(scratch_dir));
    command
}

/// A browser child process plus ownership of its whole descendant tree.
///
/// Chrome forks renderer, GPU and zygote children; killing the root PID alone
/// leaves them running. On Unix the child leads its own process group, so a
/// group signal reaches every descendant that has not left it; on Windows the
/// child is frozen at spawn, assigned to a kill-on-close Job Object before any
/// of its code runs, then resumed, so no descendant can exist outside the job.
/// Failing to establish that ownership is an error, never a warning
/// (`AGENTS.md`, "Bound every resource, loop, and process tree").
struct ChromeChild {
    child: Child,
    #[cfg(windows)]
    job: Option<crate::managed_agents::JobHandle>,
    terminated: bool,
}

impl ChromeChild {
    /// Spawn `command` with tree ownership established before the child runs.
    fn spawn(mut command: Command) -> Result<Self, String> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            command.creation_flags(CHROME_CREATION_FLAGS);
        }

        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut child = command
            .spawn()
            .map_err(|e| format!("PDF export could not start a browser: {e}"))?;

        #[cfg(windows)]
        let job = {
            let Some(job) = crate::managed_agents::create_job_for_child(child.id()) else {
                let _ = child.kill();
                let _ = child.wait();
                return Err(
                    "PDF export could not contain the browser's process tree, so it did not \
                     start one."
                        .to_string(),
                );
            };
            if !crate::managed_agents::resume_process(child.id()) {
                // Dropping the job kills the still-frozen child.
                drop(job);
                let _ = child.wait();
                return Err("PDF export could not start the contained browser process.".to_string());
            }
            job
        };

        Ok(Self {
            child,
            #[cfg(windows)]
            job: Some(job),
            terminated: false,
        })
    }

    /// Take the piped stderr the DevTools banner arrives on.
    fn take_stderr(&mut self) -> Result<ChildStderr, String> {
        self.child
            .stderr
            .take()
            .ok_or_else(|| "PDF export could not read the browser's output.".to_string())
    }

    /// Kill the whole tree and reap the root. Idempotent; every failure is
    /// returned with the PID that produced it.
    fn terminate(&mut self) -> Result<(), String> {
        if self.terminated {
            return Ok(());
        }
        self.terminated = true;
        let pid = self.child.id();

        #[cfg(unix)]
        // `terminate_process` signals the child's process group — SIGTERM,
        // a bounded grace period, then SIGKILL — so descendants go with it.
        let contained = crate::managed_agents::terminate_process(pid)
            .map_err(|e| format!("PDF export could not stop the browser's process tree: {e}"));

        #[cfg(windows)]
        // Closing the kill-on-close job reaps every descendant, even once the
        // root has exited.
        let contained = {
            drop(self.job.take());
            Ok(())
        };

        #[cfg(not(any(unix, windows)))]
        let contained: Result<(), String> =
            Err("PDF export cannot contain a browser process tree on this platform.".to_string());

        let reaped = self
            .child
            .wait()
            .map(|_| ())
            .map_err(|e| format!("PDF export could not reap the browser process {pid}: {e}"));

        contained.and(reaped)
    }
}

impl Drop for ChromeChild {
    fn drop(&mut self) {
        // Backstop for the paths that return before the explicit teardown;
        // those paths surface the failure themselves, so this only reports
        // what would otherwise be invisible.
        if let Err(error) = self.terminate() {
            eprintln!("buzz-desktop: pdf_export: {error}");
        }
    }
}

/// Scan the browser's stderr for the DevTools endpoint on its own thread.
///
/// The scan itself is unbounded in time — a browser may hold stderr open
/// forever — which is exactly why it does not run on the caller's thread: the
/// caller waits on the returned channel with a deadline. The scan is bounded
/// in *bytes* so a chatty or hostile child cannot grow the buffer, and it
/// keeps draining after the banner so a full stderr pipe never blocks the
/// browser.
fn spawn_devtools_url_reader(stderr: ChildStderr) -> Receiver<Result<String, String>> {
    let (tx, rx) = mpsc::sync_channel::<Result<String, String>>(1);
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr.take(MAX_LAUNCH_STDERR_BYTES));
        let mut scanned: u64 = 0;
        let mut line = String::new();
        let outcome = loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    break if scanned >= MAX_LAUNCH_STDERR_BYTES {
                        Err(format!(
                            "the browser wrote more than {MAX_LAUNCH_STDERR_BYTES} bytes without \
                             reporting a DevTools endpoint"
                        ))
                    } else {
                        Err("the browser exited before it reported a DevTools endpoint".to_string())
                    };
                }
                Ok(read) => {
                    scanned += read as u64;
                    if let Some(offset) = line.find(DEVTOOLS_BANNER) {
                        let url = line[offset + DEVTOOLS_BANNER.len()..].trim().to_string();
                        if url.starts_with("ws://") {
                            break Ok(url);
                        }
                    }
                }
                Err(e) => break Err(format!("the browser's output could not be read: {e}")),
            }
        };
        // A receiver that has already given up on the deadline is gone; the
        // send failing then is the expected case, not an error to report.
        let _ = tx.send(outcome);

        let mut rest = reader.into_inner().into_inner();
        let mut sink = [0u8; 4096];
        while matches!(rest.read(&mut sink), Ok(read) if read > 0) {}
    });
    rx
}

/// Append a teardown failure to the failure that caused the teardown, so
/// neither is lost.
fn with_teardown(primary: String, teardown: Result<(), String>) -> String {
    match teardown {
        Ok(()) => primary,
        Err(secondary) => format!("{primary} ({secondary})"),
    }
}

/// Start the browser and wait for its DevTools endpoint under an absolute
/// deadline. On every failure path the process tree is torn down before the
/// error is returned.
fn launch_chrome(
    chrome: &Path,
    scratch_dir: &Path,
    launch_timeout: Duration,
) -> Result<(ChromeChild, String), String> {
    let mut child = ChromeChild::spawn(chrome_command(chrome, scratch_dir))?;
    let stderr = match child.take_stderr() {
        Ok(stderr) => stderr,
        Err(error) => {
            let teardown = child.terminate();
            return Err(with_teardown(error, teardown));
        }
    };

    let banner = spawn_devtools_url_reader(stderr);
    let outcome = match banner.recv_timeout(launch_timeout) {
        Ok(Ok(url)) => return Ok((child, url)),
        Ok(Err(reason)) => format!("PDF export could not start Chrome: {reason}."),
        Err(RecvTimeoutError::Timeout) => format!(
            "PDF export timed out starting Chrome: no DevTools endpoint after {} seconds.",
            launch_timeout.as_secs()
        ),
        Err(RecvTimeoutError::Disconnected) => {
            "PDF export could not start Chrome: its output ended unexpectedly.".to_string()
        }
    };
    let teardown = child.terminate();
    Err(with_teardown(outcome, teardown))
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

/// Drive an already-listening browser: open a tab, load the staged document,
/// print it.
fn print_page(devtools_ws_url: &str, page_url: &str) -> Result<Vec<u8>, String> {
    let browser = Browser::connect_with_timeout(devtools_ws_url.to_string(), BROWSER_IDLE_TIMEOUT)
        .map_err(|e| format!("PDF export could not connect to the browser: {e}"))?;
    let tab = browser
        .new_tab()
        .map_err(|e| format!("PDF export could not open a page: {e}"))?;
    let tab = tab.set_default_timeout(PAGE_TIMEOUT);
    tab.navigate_to(page_url)
        .map_err(|e| format!("PDF export could not load the document: {e}"))?;
    tab.wait_until_navigated()
        .map_err(|e| format!("PDF export timed out loading the document: {e}"))?;
    tab.print_to_pdf(Some(print_options()))
        .map_err(|e| format!("PDF export could not print the document: {e}"))
}

/// Render one print document to PDF bytes with the browser at `chrome`.
///
/// Blocking: drives a child process over the DevTools protocol. Every failure
/// is returned with the stage that produced it; nothing is written to disk
/// outside the scratch directory, which is removed before this returns.
fn render_print_document(chrome: &Path, document: &str) -> Result<Vec<u8>, String> {
    render_print_document_within(chrome, document, BROWSER_LAUNCH_TIMEOUT)
}

/// [`render_print_document`] with the launch deadline supplied, so a test can
/// exercise the deadline without waiting the production bound.
fn render_print_document_within(
    chrome: &Path,
    document: &str,
    launch_timeout: Duration,
) -> Result<Vec<u8>, String> {
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

    let (mut child, devtools_ws_url) = launch_chrome(chrome, scratch.path(), launch_timeout)?;
    let printed = print_page(&devtools_ws_url, page_url.as_str());
    let teardown = child.terminate();
    let bytes = match printed {
        Ok(bytes) => bytes,
        Err(error) => return Err(with_teardown(error, teardown)),
    };
    teardown?;

    scratch
        .close()
        .map_err(|e| format!("PDF export could not clear its scratch directory: {e}"))?;
    Ok(bytes)
}

/// Refuse a PDF larger than [`MAX_PDF_BYTES`] rather than write it. The bound
/// is on the bytes that would reach the user's disk, so it is checked between
/// the render and the write.
fn bounded_pdf_bytes(bytes: Vec<u8>) -> Result<Vec<u8>, String> {
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

/// The export pipeline, with its three side-effecting steps injected.
///
/// The order is the contract this function exists to hold, and it is the only
/// place that order is expressed: nothing is rendered until the user has
/// chosen a destination, so a cancelled dialog costs no browser and no
/// seconds; and nothing is written until the rendered bytes are inside
/// [`MAX_PDF_BYTES`], so an over-cap render never truncates the file the user
/// picked. Returns `true` when a file was written, `false` when the dialog was
/// cancelled.
async fn run_export<PickFut, RenderFut>(
    request: PdfExportRequest,
    pick: impl FnOnce(String) -> PickFut,
    render: impl FnOnce(String) -> RenderFut,
    write: impl FnOnce(&Path, &[u8]) -> Result<(), String>,
) -> Result<bool, String>
where
    PickFut: std::future::Future<Output = Result<Option<PathBuf>, String>>,
    RenderFut: std::future::Future<Output = Result<Vec<u8>, String>>,
{
    let Some(dest) = pick(request.filename).await? else {
        return Ok(false);
    };
    let bytes = bounded_pdf_bytes(render(request.document).await?)?;
    write(&dest, &bytes)?;
    Ok(true)
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
    run_export(
        request,
        |name| async move { pick_save_path(&app, &name, "PDF document", &["pdf"]).await },
        |document| async move {
            tauri::async_runtime::spawn_blocking(move || {
                let chrome = chrome_executable()?;
                render_print_document(&chrome, &document)
            })
            .await
            .map_err(|e| format!("PDF export did not finish: {e}"))?
        },
        write_pdf_atomically,
    )
    .await
}

#[cfg(test)]
#[path = "pdf_export_tests.rs"]
mod tests;
