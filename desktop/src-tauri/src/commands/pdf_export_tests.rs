//! Tests for the PDF document exporter.
//!
//! The fixture is the markdown-document twin of the T8 spike fixture
//! (`docs/plans/2026-09-04-pdf-route.md`): same three headings, same four
//! table row tokens, same code marker line, so `scripts/zs/pdf-validate.sh` —
//! the validator contract named in the T8 ticket — passes every *text* check
//! against a T9 export unchanged.
//!
//! It does not pass that script's image-XObject check, and cannot: document
//! mode renders an image as the labelled link it stands for
//! (`shared/ui/markdown/documentMode.tsx`), because the print document's
//! content-security policy denies every remote subresource, so no T9 export
//! embeds an image. Running `scripts/zs/pdf-validate.sh` on a T9 artifact
//! therefore reports 11 of 12 checks passing, with the image check FAILing by
//! design. The script is the T8 contract and is left unmodified.

use super::*;

/// Document-mode HTML for `desktop/tests/fixtures/pdf-export/approval.md`,
/// produced by the desktop renderer and asserted byte-identical to it by
/// `src/shared/ui/markdown/documentMode.test.mjs`. The two lanes share one
/// artifact so the HTML this exporter is tested against cannot drift from the
/// HTML the app actually sends.
const FIXTURE_BODY_HTML: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tests/fixtures/pdf-export/approval-body.html"
));

/// Headings, table row tokens and code marker the fixture must carry through
/// an export. Same list as `scripts/zs/pdf-validate.sh`.
const FIXTURE_TEXT_MARKERS: [&str; 8] = [
    "Approval Page One",
    "Materials Table",
    "Approval ID Generator",
    "alpha-fixture-row",
    "bravo-fixture-row",
    "charlie-fixture-row",
    "delta-fixture-row",
    "# PDF_SPIKE_CODE_MARKER_7f3a",
];

fn request(body_html: &str, title: &str, filename: &str) -> Result<String, String> {
    PdfExportRequest::new(body_html, title, filename).map(|r| r.document)
}

#[test]
fn refuses_a_document_over_the_html_cap() {
    let oversized = "<p>x</p>".repeat(MAX_DOCUMENT_HTML_BYTES / 8 + 1);
    assert!(oversized.len() > MAX_DOCUMENT_HTML_BYTES);
    let error = request(&oversized, "Doc", "doc.md").unwrap_err();
    assert!(
        error.contains("too large"),
        "expected a size refusal, got: {error}"
    );

    // The same document one byte under the cap is accepted, so the test fails
    // for the cap and not for some other property of the input.
    let allowed = "<p>x</p>".repeat(MAX_DOCUMENT_HTML_BYTES / 8);
    assert!(allowed.len() <= MAX_DOCUMENT_HTML_BYTES);
    assert!(request(&allowed, "Doc", "doc.md").is_ok());
}

#[test]
fn refuses_an_empty_document() {
    assert!(request("", "Doc", "doc.md").is_err());
    assert!(request("   \n\t ", "Doc", "doc.md").is_err());
}

#[test]
fn clamps_the_title_at_the_dto() {
    let long_title = "t".repeat(MAX_TITLE_CHARS * 10);
    let document = request("<p>body</p>", &long_title, "doc.md").unwrap();
    let title = document
        .split("<title>")
        .nth(1)
        .and_then(|rest| rest.split("</title>").next())
        .unwrap_or_default();
    assert_eq!(title.chars().count(), MAX_TITLE_CHARS);
}

#[test]
fn escapes_a_hostile_title() {
    let document = request(
        "<p>body</p>",
        "</title><script>fetch('https://evil.invalid')</script>",
        "doc.md",
    )
    .unwrap();
    assert!(!document.contains("<script>"));
    assert!(document.contains("&lt;script&gt;"));
}

#[test]
fn derives_a_bounded_pdf_basename() {
    assert_eq!(pdf_filename("release-notes.md"), "release-notes.pdf");
    assert_eq!(pdf_filename("../../etc/passwd"), "passwd.pdf");
    assert_eq!(pdf_filename("/abs/path/notes.markdown"), "notes.pdf");
    assert_eq!(pdf_filename(r"C:\Users\me\doc.mdx"), "doc.pdf");
    // `sanitize_filename` already substitutes "file" for an empty name.
    assert_eq!(pdf_filename(""), "file.pdf");
    assert_eq!(pdf_filename(".md"), "document.pdf");
    assert_eq!(pdf_filename("a\nb\tc.md"), "abc.pdf");
    assert_eq!(pdf_filename("archive.tar.gz"), "archive.tar.pdf");

    let long = format!("{}.md", "n".repeat(MAX_FILENAME_CHARS * 4));
    let derived = pdf_filename(&long);
    assert_eq!(derived.chars().count(), MAX_FILENAME_CHARS + ".pdf".len());
    assert!(derived.ends_with(".pdf"));
}

#[test]
fn print_document_denies_every_network_subresource() {
    let document = request(
        "<p><img src=\"https://remote.invalid/logo.png\" alt=\"logo\"></p>",
        "Doc",
        "doc.md",
    )
    .unwrap();
    assert!(
        document.contains("http-equiv=\"Content-Security-Policy\""),
        "the print document must carry a content-security policy"
    );
    assert!(document.contains("default-src &#39;none&#39;"));
    assert!(document.contains("img-src data:"));
    // Nothing in the policy may re-admit a network scheme.
    let policy = document
        .split("content=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap_or_default();
    assert!(
        !policy.contains("http"),
        "policy admits a network scheme: {policy}"
    );
    assert!(
        !policy.contains('*'),
        "policy admits a wildcard source: {policy}"
    );
}

#[test]
fn chrome_launch_args_fail_every_dns_lookup() {
    assert!(
        CHROME_LAUNCH_ARGS.contains(&"--host-resolver-rules=MAP * ~NOTFOUND"),
        "the browser must not be able to resolve a host: {CHROME_LAUNCH_ARGS:?}"
    );
    assert!(CHROME_LAUNCH_ARGS.contains(&"--no-proxy-server"));
    assert!(
        CHROME_LAUNCH_ARGS.contains(&"--disable-remote-fonts"),
        "a remote font is a network fetch the print document's policy does not \
         cover on its own: {CHROME_LAUNCH_ARGS:?}"
    );
}

/// The Windows creation flags are the containment contract, so they are named
/// constants with a compile-time guard (`pdf_export.rs`), mirroring
/// `managed_agents/discovery/bounded_command.rs`. This asserts the same thing
/// at runtime on the platform that can execute it.
#[cfg(windows)]
#[test]
fn the_browser_is_spawned_suspended_and_windowless() {
    assert_eq!(CHROME_CREATION_FLAGS & CREATE_SUSPENDED, CREATE_SUSPENDED);
    assert_eq!(CHROME_CREATION_FLAGS & CREATE_NO_WINDOW, CREATE_NO_WINDOW);
}

#[test]
fn the_unix_browser_env_blanks_proxies_and_redirects_every_state_directory() {
    let scratch = std::path::Path::new("/tmp/buzz-pdf-export-test");
    let scratch_value = scratch.display().to_string();
    // A parent that has *every* variable set: what the child gets is only what
    // this map names, never what the harness happens to carry.
    let env = chrome_process_env_for(scratch, ChromeEnvTarget::Unix, |_| {
        Some("inherited-from-the-harness".to_string())
    });
    for key in PROXY_ENV_VARS {
        assert_eq!(
            env.get(key).map(String::as_str),
            Some(""),
            "{key} must be blanked in the browser child's environment"
        );
    }
    assert_eq!(env.get("NO_PROXY").map(String::as_str), Some("*"));
    assert_eq!(env.get("no_proxy").map(String::as_str), Some("*"));
    assert_eq!(env.get("PATH").map(String::as_str), Some(CHROME_UNIX_PATH));
    for key in [
        "HOME",
        "TMPDIR",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
    ] {
        assert_eq!(
            env.get(key),
            Some(&scratch_value),
            "{key} must point at the export's own scratch directory"
        );
    }
    assert!(
        !env.contains_key("SHELL"),
        "nothing outside this map may reach the child: {env:?}"
    );
}

/// The Windows environment is built and asserted on every platform.
///
/// After `env_clear` a Windows child keeps only what this map names, and
/// Windows resolves the process image and its DLLs through `SystemRoot` and
/// `WINDIR` — a child without them very likely does not start at all. The
/// repo's own precedent is `commands/media_transcode.rs`, which restores the
/// same loader variables for its ffmpeg child. Testing the pure builder rather
/// than the spawn is what makes this assertion runnable on a Linux or macOS
/// lane instead of waiting for a Windows one that does not exist.
#[test]
fn the_windows_browser_env_carries_the_loader_variables_and_no_unix_ones() {
    let scratch = std::path::Path::new("/tmp/buzz-pdf-export-test");
    let scratch_value = scratch.display().to_string();
    let env = chrome_process_env_for(scratch, ChromeEnvTarget::Windows, |key| match key {
        "SystemRoot" | "WINDIR" => Some("C:\\Windows".to_string()),
        _ => Some("inherited-from-the-harness".to_string()),
    });

    for key in WINDOWS_LOADER_ENV_VARS {
        assert_eq!(
            env.get(key).map(String::as_str),
            Some("C:\\Windows"),
            "{key} must be carried over or the browser cannot start"
        );
    }
    assert_eq!(
        env.get("PATH").map(String::as_str),
        Some("C:\\Windows\\system32;C:\\Windows"),
        "PATH must be rebuilt from SystemRoot, never inherited"
    );
    for key in ["TEMP", "TMP"] {
        assert_eq!(
            env.get(key),
            Some(&scratch_value),
            "{key} must point at the export's own scratch directory"
        );
    }
    for key in PROXY_ENV_VARS {
        assert_eq!(
            env.get(key).map(String::as_str),
            Some(""),
            "{key} must be blanked on Windows too"
        );
    }
    assert_eq!(env.get("no_proxy").map(String::as_str), Some("*"));
    // Only the loader variables are taken from the parent, even though the
    // lookup above answers every name.
    assert!(
        !env.contains_key("USERPROFILE") && !env.contains_key("APPDATA"),
        "nothing outside the loader list may be inherited: {env:?}"
    );
    // The Unix map's variables mean nothing here and must not be shipped.
    for key in [
        "HOME",
        "TMPDIR",
        "LC_ALL",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
    ] {
        assert!(!env.contains_key(key), "{key} has no meaning on Windows");
    }
}

/// The command the export actually spawns builds its environment for the
/// platform it is running on, not for whichever branch was written last.
#[test]
fn the_browser_env_is_built_for_the_host_platform() {
    let scratch = std::path::Path::new("/tmp/buzz-pdf-export-test");
    let scratch_value = scratch.display().to_string();
    let env = chrome_process_env(scratch);
    #[cfg(unix)]
    assert_eq!(
        env.get("HOME"),
        Some(&scratch_value),
        "the browser must not read the user's home directory"
    );
    #[cfg(windows)]
    assert_eq!(
        env.get("TEMP"),
        Some(&scratch_value),
        "the browser must write its temporary files inside the scratch directory"
    );
    assert_eq!(env.get("no_proxy").map(String::as_str), Some("*"));
}

#[test]
fn print_document_keeps_the_body_and_never_collapses_code() {
    let document = request(FIXTURE_BODY_HTML, "Approval", "approval.md").unwrap();
    for marker in FIXTURE_TEXT_MARKERS {
        assert!(
            document.contains(marker),
            "the print document dropped {marker}"
        );
    }
    assert!(
        document.contains("href=\"https://example.invalid/handbook\""),
        "links must survive into the printed document"
    );
    // The stylesheet must not reintroduce the viewer's height cap or scroll
    // container on code blocks.
    let style = document
        .split("<style>")
        .nth(1)
        .and_then(|rest| rest.split("</style>").next())
        .unwrap_or_default();
    assert!(style.contains("max-height: none"));
    assert!(!style.contains("overflow: auto"));
    assert!(!style.contains("overflow-y: scroll"));
}

#[test]
fn render_reports_a_missing_browser_instead_of_panicking() {
    let missing = std::path::Path::new("/nonexistent/buzz-pdf-export/chrome");
    let error = render_print_document(missing, "<!doctype html><p>x</p>").unwrap_err();
    assert!(
        error.contains("not an executable file"),
        "expected a typed browser-missing error, got: {error}"
    );
}

/// Run a poppler tool with a fully explicit environment: nothing about the
/// shell that started `cargo test` reaches the child except `PATH`, which is
/// how the tool is found in the first place.
fn poppler(tool: &str, args: &[&str]) -> String {
    let path = std::env::var("PATH").unwrap_or_default();
    let output = std::process::Command::new(tool)
        .args(args)
        .env_clear()
        .env("PATH", path)
        .env("LC_ALL", "C")
        .output()
        .unwrap_or_else(|e| panic!("{tool} (poppler) is required by this test: {e}"));
    assert!(
        output.status.success(),
        "{tool} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The real export: renders the fixture through the picked route and parses
/// the resulting PDF for page count and content.
///
/// Ignored by default because it drives a locally installed Chrome or
/// Chromium and shells out to poppler (`pdfinfo`, `pdftotext`, `pdftoppm`),
/// neither of which is present on every CI runner. Run it deliberately:
/// `cargo test pdf_export -- --ignored`.
#[test]
#[ignore = "requires a local Chrome/Chromium and poppler; run with --ignored"]
fn renders_the_fixture_to_a_three_page_pdf() {
    let document =
        PdfExportRequest::new(FIXTURE_BODY_HTML, "Approval Page One", "approval.md").unwrap();
    let chrome = chrome_executable().unwrap();
    let bytes = render_print_document(&chrome, &document.document).unwrap();
    assert!(bytes.starts_with(b"%PDF-"), "output is not a PDF");
    assert!(bytes.len() <= MAX_PDF_BYTES);

    let dir = tempfile::Builder::new()
        .prefix("buzz-pdf-export-test-")
        .tempdir()
        .unwrap();
    let pdf = dir.path().join("approval.pdf");
    std::fs::write(&pdf, &bytes).unwrap();
    let pdf_arg = pdf.display().to_string();

    let info = poppler("pdfinfo", &[&pdf_arg]);
    let pages = info
        .lines()
        .find_map(|line| line.strip_prefix("Pages:"))
        .map(|value| value.trim().to_string())
        .unwrap_or_default();
    assert_eq!(pages, "3", "page count changed:\n{info}");

    let text = poppler("pdftotext", &["-layout", &pdf_arg, "-"]);
    for marker in FIXTURE_TEXT_MARKERS {
        assert!(text.contains(marker), "extracted text is missing {marker}");
    }

    // Every page must rasterise without error, which is what proves the pages
    // are renderable and not just countable.
    let png_prefix = dir.path().join("page").display().to_string();
    poppler("pdftoppm", &["-png", "-r", "100", &pdf_arg, &png_prefix]);
    let rendered = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "png")
        })
        .count();
    assert_eq!(rendered, 3, "every page must render to a PNG");

    // The PDF's own sha256 is not stable across runs (Chrome stamps a
    // creation date into the container), so the recorded hash for this
    // fixture is of the rendered pages. Set BUZZ_PDF_EXPORT_TEST_ARTIFACTS to
    // a directory to keep the PDF and its page PNGs for that record.
    if let Ok(out_dir) = std::env::var("BUZZ_PDF_EXPORT_TEST_ARTIFACTS") {
        let out_dir = std::path::PathBuf::from(out_dir);
        std::fs::create_dir_all(&out_dir).unwrap();
        for entry in std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
        {
            let name = entry.file_name();
            std::fs::copy(entry.path(), out_dir.join(name)).unwrap();
        }
    }
}

// ---------------------------------------------------------------------------
// The orchestration seam: order, and what each guard actually prevents.
//
// `run_export` is the production pipeline — `export_document_pdf` is a thin
// wrapper that supplies the real dialog, renderer and writer to it — so these
// tests bind the production seam rather than a test-only helper. Each one
// fails if the guard it names is removed: reorder the render above the picker
// and the cancel test sees a render; drop the size check and the over-cap test
// sees a write.
// ---------------------------------------------------------------------------

use std::sync::{Arc, Mutex};

/// Records which side-effecting steps ran, in order.
#[derive(Clone, Default)]
struct StepLog(Arc<Mutex<Vec<&'static str>>>);

impl StepLog {
    fn record(&self, step: &'static str) {
        self.0.lock().expect("step log poisoned").push(step);
    }

    fn steps(&self) -> Vec<&'static str> {
        self.0.lock().expect("step log poisoned").clone()
    }
}

fn fixture_request() -> PdfExportRequest {
    PdfExportRequest::new("<p>body</p>", "Doc", "doc.md").expect("fixture request")
}

#[test]
fn a_cancelled_dialog_renders_nothing_and_writes_nothing() {
    let log = StepLog::default();
    let (pick, render, write) = (log.clone(), log.clone(), log.clone());

    let saved = tauri::async_runtime::block_on(run_export(
        fixture_request(),
        |_name| async move {
            pick.record("pick");
            Ok(None)
        },
        |_document| async move {
            render.record("render");
            Ok(Vec::new())
        },
        |_dest, _bytes| {
            write.record("write");
            Ok(())
        },
    ))
    .expect("a cancelled dialog is not a failure");

    assert!(!saved, "a cancelled dialog must not report a saved file");
    assert_eq!(
        log.steps(),
        vec!["pick"],
        "the dialog must come first, and nothing may follow a cancel"
    );
}

#[test]
fn a_chosen_destination_is_rendered_then_written_in_that_order() {
    let log = StepLog::default();
    let (pick, render, write) = (log.clone(), log.clone(), log.clone());
    let written = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink = Arc::clone(&written);

    let saved = tauri::async_runtime::block_on(run_export(
        fixture_request(),
        |name| async move {
            pick.record("pick");
            assert_eq!(name, "doc.pdf");
            Ok(Some(std::path::PathBuf::from(
                "/tmp/buzz-pdf-export-seam.pdf",
            )))
        },
        |document| async move {
            render.record("render");
            assert!(document.contains("<p>body</p>"));
            Ok(b"%PDF-1.4 fixture".to_vec())
        },
        |dest, bytes| {
            write.record("write");
            assert_eq!(dest, std::path::Path::new("/tmp/buzz-pdf-export-seam.pdf"));
            sink.lock().expect("sink poisoned").extend_from_slice(bytes);
            Ok(())
        },
    ))
    .expect("the happy path must save");

    assert!(saved);
    assert_eq!(log.steps(), vec!["pick", "render", "write"]);
    assert_eq!(
        &*written.lock().expect("sink poisoned"),
        b"%PDF-1.4 fixture"
    );
}

#[test]
fn a_failed_render_is_surfaced_and_writes_nothing() {
    let log = StepLog::default();
    let write = log.clone();

    let error = tauri::async_runtime::block_on(run_export(
        fixture_request(),
        |_name| async {
            Ok(Some(std::path::PathBuf::from(
                "/tmp/buzz-pdf-export-seam.pdf",
            )))
        },
        |_document| async { Err("PDF export could not start Chrome: no such file".to_string()) },
        |_dest, _bytes| {
            write.record("write");
            Ok(())
        },
    ))
    .expect_err("a render failure must not report a save");

    assert!(error.contains("could not start Chrome"), "got: {error}");
    assert!(
        log.steps().is_empty(),
        "a failed render must not be written"
    );
}

#[test]
fn an_over_cap_render_is_refused_instead_of_written() {
    let log = StepLog::default();
    let write = log.clone();

    let error = tauri::async_runtime::block_on(run_export(
        fixture_request(),
        |_name| async {
            Ok(Some(std::path::PathBuf::from(
                "/tmp/buzz-pdf-export-seam.pdf",
            )))
        },
        |_document| async { Ok(vec![0u8; MAX_PDF_BYTES + 1]) },
        |_dest, _bytes| {
            write.record("write");
            Ok(())
        },
    ))
    .expect_err("an over-cap PDF must be refused");

    assert!(error.contains("too large to save"), "got: {error}");
    assert!(
        log.steps().is_empty(),
        "the size cap is what stops the write; with it removed the file would have been written"
    );

    // The same export one byte under the cap is written, so the test fails for
    // the cap and not for some other property of the input.
    let saved = tauri::async_runtime::block_on(run_export(
        fixture_request(),
        |_name| async {
            Ok(Some(std::path::PathBuf::from(
                "/tmp/buzz-pdf-export-seam.pdf",
            )))
        },
        |_document| async { Ok(vec![0u8; MAX_PDF_BYTES]) },
        |_dest, _bytes| Ok(()),
    ))
    .expect("a PDF at the cap must save");
    assert!(saved);
}

// ---------------------------------------------------------------------------
// The write seam.
// ---------------------------------------------------------------------------

#[test]
fn the_write_replaces_the_destination_contents() {
    let dir = tempfile::Builder::new()
        .prefix("buzz-pdf-export-write-")
        .tempdir()
        .unwrap();
    let dest = dir.path().join("doc.pdf");
    std::fs::write(&dest, b"stale").unwrap();

    write_pdf_atomically(&dest, b"%PDF-1.4 fresh").unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), b"%PDF-1.4 fresh");
}

/// A failed write must leave the file the user chose to overwrite exactly as
/// it was. The staged-then-renamed write is what guarantees that: a plain
/// `std::fs::write` opens the existing file for truncation and succeeds even
/// here (an existing file can be opened for writing in a directory that denies
/// creation), so this test fails the moment the atomic write is swapped for
/// one.
#[cfg(unix)]
#[test]
fn a_failed_write_leaves_the_existing_pdf_intact() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::Builder::new()
        .prefix("buzz-pdf-export-write-")
        .tempdir()
        .unwrap();
    let dest = dir.path().join("doc.pdf");
    std::fs::write(&dest, b"%PDF-1.4 previous").unwrap();

    // Deny creation of the staging file inside the destination's directory.
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
    let outcome = write_pdf_atomically(&dest, b"%PDF-1.4 replacement");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();

    let error = outcome.expect_err("a write that cannot be staged must fail, not half-succeed");
    assert!(
        error.contains("doc.pdf"),
        "the failure must name the file: {error}"
    );
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        b"%PDF-1.4 previous",
        "the destination the user chose must be untouched by a failed write"
    );
}

// ---------------------------------------------------------------------------
// The browser launch: the absolute deadline, the process tree, the flags and
// the environment the child is actually started with.
// ---------------------------------------------------------------------------

/// A stand-in browser that behaves the way the pinned `headless_chrome`
/// launcher cannot survive: it records the argv and environment it was given,
/// forks a grandchild, then holds its stderr open forever without ever
/// printing the DevTools banner.
#[cfg(unix)]
fn write_fake_chrome(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let path = dir.join("fake-chrome");
    let dump = dir.display();
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" > '{dump}/argv'\n\
             /usr/bin/env > '{dump}/env'\n\
             sh -c 'while :; do sleep 5; done' &\n\
             echo $! > '{dump}/grandchild'\n\
             while :; do sleep 5; done\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A stand-in browser that floods stderr instead of announcing an endpoint:
/// it writes more than [`MAX_LAUNCH_STDERR_BYTES`] of banner-free noise, then
/// holds the process open forever.
#[cfg(unix)]
fn write_flooding_chrome(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let path = dir.join("flooding-chrome");
    // 1,200 lines of 1,023 characters is 1,228,800 bytes, past the 1 MiB cap.
    std::fs::write(
        &path,
        "#!/bin/sh\n\
         line=$(printf '%01023d' 0)\n\
         yes \"$line\" | head -n 1200 >&2\n\
         while :; do sleep 5; done\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// The stderr scan is bounded in bytes, and this is the test that fails when
/// that bound is removed.
///
/// A browser that writes megabytes without ever printing the DevTools banner
/// must be given up on at the cap, with the cap named in the error — not read
/// until it decides to stop. With `.take(MAX_LAUNCH_STDERR_BYTES)` removed the
/// scan keeps reading, the child never exits, and the launch fails with the
/// deadline's message instead of this one.
#[cfg(unix)]
#[test]
fn a_browser_that_floods_stderr_is_given_up_on_at_the_scan_cap() {
    let dir = tempfile::Builder::new()
        .prefix("buzz-pdf-export-flood-")
        .tempdir()
        .unwrap();
    let chrome = write_flooding_chrome(dir.path());

    let started = std::time::Instant::now();
    let error = launch_chrome(&chrome, dir.path(), std::time::Duration::from_secs(5))
        .err()
        .expect("a browser that never reports an endpoint must not launch");
    let elapsed = started.elapsed();

    assert!(
        error.contains(&format!("wrote more than {MAX_LAUNCH_STDERR_BYTES} bytes")),
        "expected the stderr scan cap to fire, got: {error}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "the cap must fire before the launch deadline, took {elapsed:?}"
    );
}

#[cfg(unix)]
fn read_when_present(path: &std::path::Path) -> String {
    for _ in 0..100 {
        if let Ok(text) = std::fs::read_to_string(path) {
            if !text.is_empty() {
                return text;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("{} was never written", path.display());
}

/// The launch deadline is this module's, not the library's: a browser that
/// starts and never reports an endpoint is given up on, and its whole process
/// tree — not just the PID we spawned — is torn down before the error is
/// returned.
///
/// The same run asserts the two network-egress guards on the launch that uses
/// them rather than on the constants: the recorded argv must carry the
/// DNS-failing resolver rule and `--no-proxy-server`, and the recorded
/// environment must be the one this module builds, with nothing inherited from
/// the process running the tests. Removing `.args(CHROME_LAUNCH_ARGS)` or
/// `.env_clear()` from `chrome_command` fails this test.
#[cfg(unix)]
#[test]
fn a_browser_that_never_reports_an_endpoint_is_bounded_and_its_tree_killed() {
    std::env::set_var(
        "BUZZ_PDF_EXPORT_TEST_SENTINEL",
        "inherited-from-the-harness",
    );

    let dir = tempfile::Builder::new()
        .prefix("buzz-pdf-export-launch-")
        .tempdir()
        .unwrap();
    let chrome = write_fake_chrome(dir.path());

    let started = std::time::Instant::now();
    let error = launch_chrome(&chrome, dir.path(), std::time::Duration::from_secs(2))
        .err()
        .expect("a browser that never reports an endpoint must not launch");
    let elapsed = started.elapsed();

    assert!(
        error.contains("timed out starting Chrome"),
        "expected the launch deadline to fire, got: {error}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "the deadline must hold regardless of the child, took {elapsed:?}"
    );

    let argv = read_when_present(&dir.path().join("argv"));
    let argv: Vec<&str> = argv.lines().collect();
    assert!(
        argv.contains(&"--host-resolver-rules=MAP * ~NOTFOUND"),
        "the launch must fail every DNS lookup: {argv:?}"
    );
    assert!(
        argv.contains(&"--no-proxy-server"),
        "the launch must refuse every proxy: {argv:?}"
    );
    assert!(
        argv.contains(&"--disable-remote-fonts"),
        "the launch must not fetch a remote font: {argv:?}"
    );
    assert!(
        argv.iter()
            .any(|arg| *arg == format!("--user-data-dir={}", dir.path().join("profile").display())),
        "the profile must live inside the export's own scratch directory: {argv:?}"
    );

    let env = read_when_present(&dir.path().join("env"));
    let names: Vec<&str> = env
        .lines()
        .filter_map(|line| line.split_once('=').map(|(name, _)| name))
        .collect();
    assert!(
        !names.contains(&"BUZZ_PDF_EXPORT_TEST_SENTINEL"),
        "the browser must inherit nothing from the process that launched it: {names:?}"
    );
    for key in PROXY_ENV_VARS {
        assert!(
            env.lines().all(|line| line != format!("{key}=inherited")),
            "{key} must not come from the harness"
        );
    }
    assert!(
        env.lines().any(|line| line == "no_proxy=*"),
        "the child's own environment must deny proxies: {env}"
    );
    assert!(
        env.lines()
            .any(|line| line == format!("HOME={}", dir.path().display())),
        "the child's HOME must be the scratch directory: {env}"
    );

    let grandchild: u32 = read_when_present(&dir.path().join("grandchild"))
        .trim()
        .parse()
        .expect("the fake browser must report its grandchild's pid");
    let mut alive = true;
    for _ in 0..100 {
        if !crate::managed_agents::process_is_running(grandchild) {
            alive = false;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        !alive,
        "the browser's descendants must be killed with it, {grandchild} survived"
    );
}

/// The offline check the T8 spike ran, carried into T9's suite: a real render
/// of a document that asks for remote subresources by hostname *and* by direct
/// IP must open zero connections to a listener on this machine.
///
/// Direct-IP is the case the launch flags alone do not cover — Chrome's
/// resolver short-circuits an IP literal, so `--host-resolver-rules` never
/// sees it — which is why the print document's `default-src 'none'` policy is
/// the second, independent guard. Removing either one is meant to fail here.
///
/// Ignored by default for the same reason as the render test below: it drives
/// a locally installed Chrome or Chromium. Run it with
/// `cargo test pdf_export -- --ignored`.
#[test]
#[ignore = "requires a local Chrome/Chromium; run with --ignored"]
fn the_render_opens_no_network_connection() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let connections = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&connections);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if stream.is_err() {
                break;
            }
            counted.fetch_add(1, Ordering::SeqCst);
        }
    });

    let body = format!(
        "<p>By name <img src=\"http://sentinel.invalid:{port}/by-name.png\" alt=\"by name\"></p>\
         <p>By address <img src=\"http://127.0.0.1:{port}/by-address.png\" alt=\"by address\"></p>\
         <p><a href=\"http://127.0.0.1:{port}/link\">a link is not a fetch</a></p>"
    );
    let request = PdfExportRequest::new(&body, "Egress", "egress.md").unwrap();
    let chrome = chrome_executable().unwrap();
    let bytes = render_print_document(&chrome, &request.document).unwrap();
    assert!(bytes.starts_with(b"%PDF-"), "output is not a PDF");

    assert_eq!(
        connections.load(Ordering::SeqCst),
        0,
        "the export reached the network"
    );
}
