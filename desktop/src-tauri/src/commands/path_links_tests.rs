//! Containment tests for the path-link resolver.
//!
//! Every test drives the production resolver (`resolve_within_roots`,
//! `read_markdown_within_cap`) against a real temporary directory, so removing
//! any one guard fails a test here rather than only changing a comment.

use std::fs;
use std::path::{Path, PathBuf};

use super::path_links::{
    read_markdown_within_cap, resolve_within_roots, sender_workdir_root, PathLinkKind,
    MAX_PATH_LINK_BYTES, MAX_PATH_LINK_MARKDOWN_BYTES,
};

/// A canonical root plus a canonical sibling directory outside it.
///
/// macOS puts temporary directories behind a `/var -> /private/var` symlink,
/// so both paths are canonicalized here — the resolver compares canonical
/// paths, and a test that skipped this would compare two spellings of one
/// directory and pass for the wrong reason.
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    outside: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().canonicalize().expect("canonical temp dir");
    let root = base.join("root");
    let outside = base.join("outside");
    fs::create_dir_all(&root).expect("root dir");
    fs::create_dir_all(&outside).expect("outside dir");
    Fixture {
        _dir: dir,
        root,
        outside,
    }
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent dir");
    }
    fs::write(path, contents).expect("write fixture file");
}

#[test]
fn a_regular_file_inside_a_root_resolves_to_its_canonical_path() {
    let fixture = fixture();
    write(&fixture.root.join("docs/report.md"), "# report\n");

    let target = resolve_within_roots("docs/report.md", std::slice::from_ref(&fixture.root))
        .expect("resolution succeeds")
        .expect("the file resolves");

    assert_eq!(
        target.path,
        fixture.root.join("docs/report.md").display().to_string()
    );
    assert_eq!(target.filename, "report.md");
    assert_eq!(target.kind, PathLinkKind::Markdown);
    assert_eq!(target.size_bytes, "# report\n".len() as u64);
}

#[test]
fn an_absolute_path_inside_a_root_resolves() {
    let fixture = fixture();
    let file = fixture.root.join("approvals/item-7.html");
    write(&file, "<p>ok</p>");

    let target = resolve_within_roots(
        &file.display().to_string(),
        std::slice::from_ref(&fixture.root),
    )
    .expect("resolution succeeds")
    .expect("the file resolves");

    assert_eq!(target.kind, PathLinkKind::File);
    assert_eq!(target.filename, "item-7.html");
}

#[test]
fn an_absolute_path_outside_every_root_is_not_a_link() {
    let fixture = fixture();
    let secret = fixture.outside.join("secret.md");
    write(&secret, "secret\n");

    assert_eq!(
        resolve_within_roots(
            &secret.display().to_string(),
            std::slice::from_ref(&fixture.root)
        )
        .expect("resolution succeeds"),
        None
    );
}

#[test]
fn a_parent_traversal_cannot_escape_a_root() {
    let fixture = fixture();
    write(&fixture.outside.join("secret.md"), "secret\n");
    write(&fixture.root.join("docs/report.md"), "# report\n");

    // Both spellings canonicalize to the file outside the root.
    for candidate in ["../outside/secret.md", "docs/../../outside/secret.md"] {
        assert_eq!(
            resolve_within_roots(candidate, std::slice::from_ref(&fixture.root))
                .expect("resolution succeeds"),
            None,
            "{candidate} must not escape the root"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_symlink_whose_target_leaves_the_root_is_rejected() {
    let fixture = fixture();
    let secret = fixture.outside.join("secret.md");
    write(&secret, "secret\n");
    std::os::unix::fs::symlink(&secret, fixture.root.join("escape.md")).expect("symlink");

    assert_eq!(
        resolve_within_roots("escape.md", std::slice::from_ref(&fixture.root))
            .expect("resolution succeeds"),
        None
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_whose_target_stays_inside_the_root_still_resolves() {
    // The paired positive control: the rejection above is about leaving the
    // root, not about symlinks, so a containment guard that simply refused
    // every symlink would fail here.
    let fixture = fixture();
    let real = fixture.root.join("docs/report.md");
    write(&real, "# report\n");
    std::os::unix::fs::symlink(&real, fixture.root.join("latest.md")).expect("symlink");

    let target = resolve_within_roots("latest.md", std::slice::from_ref(&fixture.root))
        .expect("resolution succeeds")
        .expect("the file resolves");

    assert_eq!(target.path, real.display().to_string());
}

#[test]
fn a_missing_file_is_not_a_link_and_not_an_error() {
    let fixture = fixture();

    assert_eq!(
        resolve_within_roots("docs/never-written.md", std::slice::from_ref(&fixture.root))
            .expect("a missing file is not an error"),
        None
    );
}

#[test]
fn a_directory_inside_a_root_is_not_a_link() {
    let fixture = fixture();
    fs::create_dir_all(fixture.root.join("docs")).expect("docs dir");

    assert_eq!(
        resolve_within_roots("docs", std::slice::from_ref(&fixture.root))
            .expect("resolution succeeds"),
        None
    );
}

#[test]
fn an_over_length_candidate_is_refused_before_any_filesystem_call() {
    // No roots at all: with the cap removed this call reaches the (empty) root
    // loop and answers `Ok(None)`, so the test fails exactly when the guard
    // goes away.
    let candidate = format!("docs/{}.md", "a".repeat(MAX_PATH_LINK_BYTES));

    let error =
        resolve_within_roots(&candidate, &[]).expect_err("an over-length candidate is refused");
    assert!(error.contains(&MAX_PATH_LINK_BYTES.to_string()), "{error}");

    let at_cap = format!("{}.md", "a".repeat(MAX_PATH_LINK_BYTES - 3));
    assert_eq!(at_cap.len(), MAX_PATH_LINK_BYTES);
    assert_eq!(
        resolve_within_roots(&at_cap, &[]).expect("a candidate at the cap is not refused"),
        None
    );
}

#[test]
fn a_markdown_document_over_the_viewer_cap_opens_with_the_os_handler() {
    let fixture = fixture();
    let oversized = "#".repeat(MAX_PATH_LINK_MARKDOWN_BYTES as usize + 1);
    write(&fixture.root.join("huge.md"), &oversized);

    let target = resolve_within_roots("huge.md", std::slice::from_ref(&fixture.root))
        .expect("resolution succeeds")
        .expect("the file resolves");

    assert_eq!(target.kind, PathLinkKind::File);
    assert_eq!(target.size_bytes, MAX_PATH_LINK_MARKDOWN_BYTES + 1);
}

#[test]
fn a_markdown_extension_is_matched_case_insensitively() {
    let fixture = fixture();
    write(&fixture.root.join("NOTES.MD"), "# notes\n");

    let target = resolve_within_roots("NOTES.MD", std::slice::from_ref(&fixture.root))
        .expect("resolution succeeds")
        .expect("the file resolves");

    assert_eq!(target.kind, PathLinkKind::Markdown);
}

#[test]
fn the_first_root_holding_the_candidate_wins() {
    let fixture = fixture();
    let second_root = fixture.outside.clone();
    write(&fixture.root.join("report.md"), "first\n");
    write(&second_root.join("report.md"), "second\n");

    let target = resolve_within_roots("report.md", &[fixture.root.clone(), second_root])
        .expect("resolution succeeds")
        .expect("the file resolves");

    assert_eq!(
        target.path,
        fixture.root.join("report.md").display().to_string()
    );
}

#[test]
fn a_sender_pubkey_selects_no_root_today() {
    // Managed agents share one spawn working directory and carry no per-agent
    // one, so no sender-specific root exists to add. This test is the record
    // of that: a future per-sender mapping changes it deliberately.
    assert_eq!(sender_workdir_root(Some(&"a".repeat(64))), None);
    assert_eq!(sender_workdir_root(None), None);
}

#[test]
fn reading_a_document_is_bounded_by_the_bytes_it_reads() {
    let fixture = fixture();
    let at_cap = fixture.root.join("at-cap.md");
    write(&at_cap, &"a".repeat(MAX_PATH_LINK_MARKDOWN_BYTES as usize));
    assert_eq!(
        read_markdown_within_cap(&at_cap)
            .expect("a document at the cap reads")
            .len(),
        MAX_PATH_LINK_MARKDOWN_BYTES as usize
    );

    // Grown past the cap between resolve and read: refused, never truncated.
    let over_cap = fixture.root.join("over-cap.md");
    write(
        &over_cap,
        &"a".repeat(MAX_PATH_LINK_MARKDOWN_BYTES as usize + 1),
    );
    assert!(read_markdown_within_cap(&over_cap)
        .expect_err("an oversized document is refused")
        .contains("too large"));
}

#[test]
fn reading_a_document_that_is_not_text_is_refused_with_a_reason() {
    let fixture = fixture();
    let binary = fixture.root.join("binary.md");
    fs::write(&binary, [0xff, 0xfe, 0x00]).expect("write binary fixture");

    assert!(read_markdown_within_cap(&binary)
        .expect_err("invalid text is refused")
        .contains("valid text"));
}

#[test]
fn reading_a_missing_document_reports_the_path() {
    let fixture = fixture();
    let missing = fixture.root.join("missing.md");

    let error = read_markdown_within_cap(&missing).expect_err("a missing file is an error here");
    assert!(error.contains("missing.md"), "{error}");
}
