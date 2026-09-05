//! Tests for the PDF document exporter.
//!
//! The fixture is the markdown-document twin of the T8 spike fixture
//! (`docs/plans/2026-09-04-pdf-route.md`): same three headings, same four
//! table row tokens, same code marker line, so `scripts/zs/pdf-validate.sh` —
//! the validator contract named in the T8 ticket — validates a T9 export
//! unchanged.

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
}

#[test]
fn chrome_process_env_blanks_inherited_proxies() {
    let scratch = std::path::Path::new("/tmp/buzz-pdf-export-test");
    let env = chrome_process_env(scratch);
    for key in PROXY_ENV_VARS {
        assert_eq!(
            env.get(key).map(String::as_str),
            Some(""),
            "{key} must be blanked in the browser child's environment"
        );
    }
    assert_eq!(env.get("NO_PROXY").map(String::as_str), Some("*"));
    assert_eq!(env.get("no_proxy").map(String::as_str), Some("*"));
    assert_eq!(
        env.get("HOME").map(String::as_str),
        Some(scratch.display().to_string().as_str()),
        "the browser must not read the user's home directory"
    );
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
