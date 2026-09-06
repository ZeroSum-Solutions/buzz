//! Containment tests for the path-link resolver.
//!
//! Every test drives the production resolver (`resolve_within_roots`,
//! `read_markdown_within_cap`) against a real temporary directory, so removing
//! any one guard fails a test here rather than only changing a comment.

use std::fs;
use std::path::{Path, PathBuf};

use super::path_links::{
    is_lexically_refused, open_resolved_path_link, path_link_roots, read_bounded,
    read_markdown_path_link, read_markdown_within_cap, resolve_blocking, resolve_within_roots,
    sender_workdir_root, PathLinkKind, PathLinkTarget, MAX_PATH_LINK_BYTES,
    MAX_PATH_LINK_MARKDOWN_BYTES,
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

// ---------------------------------------------------------------------------
// What the OS default handler would do with the resolved file.
//
// Containment says which path a message may name. These say that naming an
// executable, a script or a shortcut is "not a link" — removing either guard
// in `resolve_within_roots` fails one of them.
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("chmod");
}

#[cfg(unix)]
#[test]
fn an_executable_command_script_inside_a_root_is_not_a_link() {
    // The real shape found on this machine: a `.command` file with a shebang,
    // mode 755, inside `$HOME/projects`. macOS `open` runs it in Terminal.
    let fixture = fixture();
    let script = fixture.root.join("open-previews.command");
    write(&script, "#!/bin/bash\necho pwned\n");
    make_executable(&script);

    assert_eq!(
        resolve_within_roots("open-previews.command", std::slice::from_ref(&fixture.root))
            .expect("resolution succeeds"),
        None
    );
}

#[test]
fn a_script_or_shortcut_extension_inside_a_root_is_not_a_link() {
    let fixture = fixture();
    for name in [
        "deploy.sh",
        "install.command",
        "setup.exe",
        "run.cmd",
        "shortcut.lnk",
        "bookmark.webloc",
        "bookmark.url",
        "libthing.dylib",
        "noextension",
    ] {
        write(&fixture.root.join(name), "payload\n");
        assert_eq!(
            resolve_within_roots(name, std::slice::from_ref(&fixture.root))
                .expect("resolution succeeds"),
            None,
            "{name} must not be offered to the OS handler"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_document_carrying_an_executable_bit_is_not_a_link() {
    // The extension allowlist alone would admit this file; the executable-bit
    // guard is what refuses it, so removing that guard fails here.
    let fixture = fixture();
    let document = fixture.root.join("report.md");
    write(&document, "# report\n");
    make_executable(&document);

    assert_eq!(
        resolve_within_roots("report.md", std::slice::from_ref(&fixture.root))
            .expect("resolution succeeds"),
        None
    );
}

// ---------------------------------------------------------------------------
// Lexical refusal, before any filesystem call.
// ---------------------------------------------------------------------------

#[test]
fn network_device_and_drive_relative_shapes_are_refused_by_inspection() {
    for candidate in [
        r"\\attacker\share\note.md",
        r"\\?\C:\Users\x\note.md",
        r"\\.\pipe\note.md",
        r"\note.md",
        r"docs\note.md",
        "//attacker/share/note.md",
        "../outside/secret.md",
        "docs/../../outside/secret.md",
    ] {
        assert!(
            is_lexically_refused(candidate),
            "{candidate} must be refused before any filesystem call"
        );
    }
    for candidate in ["docs/report.md", "report.md", "/Users/x/projects/report.md"] {
        assert!(!is_lexically_refused(candidate), "{candidate} is a path");
    }
}

#[cfg(unix)]
#[test]
fn a_unc_shaped_candidate_never_resolves_even_when_it_names_a_real_file() {
    // A backslash is a legal filename byte on Unix, so this file really
    // exists inside the root and would resolve if the lexical gate were
    // removed — which is exactly what makes the gate falsifiable here.
    // Built by formatting rather than `join`, which would treat a leading
    // separator as a replacement.
    let fixture = fixture();
    let name = r"\\attacker\share\note.md";
    write(
        &PathBuf::from(format!("{}/{name}", fixture.root.display())),
        "# note\n",
    );

    assert_eq!(
        resolve_within_roots(name, std::slice::from_ref(&fixture.root))
            .expect("resolution succeeds"),
        None
    );
}

// ---------------------------------------------------------------------------
// The command-level guards: the pubkey cap, the opener seam, the viewer's
// markdown-only refusal.
// ---------------------------------------------------------------------------

#[test]
fn an_over_length_sender_pubkey_is_refused_before_any_root_is_read() {
    let over = "a".repeat(129);
    let error = resolve_blocking("docs/report.md", Some(&over))
        .expect_err("an over-length sender pubkey is refused");
    assert!(error.contains("128"), "{error}");

    // At the cap the call proceeds normally (it may or may not find a file on
    // this machine; what matters is that it is not refused).
    let at_cap = "a".repeat(128);
    assert!(resolve_blocking("docs/report.md", Some(&at_cap)).is_ok());
    assert!(resolve_blocking("docs/report.md", None).is_ok());
}

/// Records every path handed to the opener seam.
fn record_open(candidate: &str, root: &Path) -> (Result<(), String>, Vec<String>) {
    let mut opened = Vec::new();
    let result = open_resolved_path_link(
        candidate,
        None,
        std::slice::from_ref(&root.to_path_buf()),
        |target: &PathLinkTarget| {
            opened.push(target.path.clone());
            Ok(())
        },
    );
    (result, opened)
}

#[test]
fn the_opener_is_reached_only_for_a_contained_inert_file() {
    let fixture = fixture();
    write(&fixture.root.join("approvals/item-7.html"), "<p>ok</p>");

    let (result, opened) = record_open("approvals/item-7.html", &fixture.root);
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        opened,
        vec![fixture
            .root
            .join("approvals/item-7.html")
            .display()
            .to_string()]
    );
}

#[test]
fn the_opener_is_never_reached_for_a_target_the_resolver_refuses() {
    let fixture = fixture();
    write(&fixture.outside.join("secret.html"), "<p>secret</p>");
    fs::create_dir_all(fixture.root.join("docs")).expect("docs dir");
    write(&fixture.root.join("deploy.sh"), "#!/bin/sh\n");
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        fixture.outside.join("secret.html"),
        fixture.root.join("escape.html"),
    )
    .expect("symlink");

    let mut candidates = vec![
        fixture.outside.join("secret.html").display().to_string(),
        "docs".to_string(),
        "deploy.sh".to_string(),
        "docs/missing.md".to_string(),
        r"\\attacker\share\note.md".to_string(),
    ];
    #[cfg(unix)]
    candidates.push("escape.html".to_string());

    for candidate in candidates {
        let (result, opened) = record_open(&candidate, &fixture.root);
        assert!(
            result.is_err(),
            "{candidate} must not reach the opener: {result:?}"
        );
        assert!(opened.is_empty(), "{candidate} reached the opener");
    }
}

#[test]
fn the_opener_refuses_an_over_length_sender_pubkey() {
    let fixture = fixture();
    write(&fixture.root.join("report.md"), "# report\n");
    let mut opened = 0;
    let error = open_resolved_path_link(
        "report.md",
        Some(&"a".repeat(129)),
        std::slice::from_ref(&fixture.root),
        |_| {
            opened += 1;
            Ok(())
        },
    )
    .expect_err("an over-length sender pubkey is refused");
    assert!(error.contains("128"), "{error}");
    assert_eq!(opened, 0);
}

#[test]
fn the_viewer_read_refuses_a_target_that_is_not_a_markdown_document() {
    let fixture = fixture();
    write(&fixture.root.join("report.md"), "# report\n");
    write(&fixture.root.join("page.html"), "<p>ok</p>");
    let oversized = "#".repeat(MAX_PATH_LINK_MARKDOWN_BYTES as usize + 1);
    write(&fixture.root.join("huge.md"), &oversized);

    assert_eq!(
        read_markdown_path_link("report.md", None, std::slice::from_ref(&fixture.root))
            .expect("a markdown document reads"),
        "# report\n"
    );
    for candidate in ["page.html", "huge.md"] {
        let error = read_markdown_path_link(candidate, None, std::slice::from_ref(&fixture.root))
            .expect_err("a non-markdown target is refused");
        assert!(error.contains("not a markdown document"), "{error}");
    }
    let error =
        read_markdown_path_link("docs/missing.md", None, std::slice::from_ref(&fixture.root))
            .expect_err("a candidate that resolves to nothing is refused");
    assert!(error.contains("no longer on this Mac"), "{error}");
}

// ---------------------------------------------------------------------------
// The read bound itself, independent of the refusal that follows it.
// ---------------------------------------------------------------------------

#[test]
fn a_bounded_read_stops_at_its_limit() {
    let fixture = fixture();
    let over_cap = fixture.root.join("over-cap.md");
    write(&over_cap, &"a".repeat(1024));

    assert_eq!(
        read_bounded(&over_cap, 10)
            .expect("a bounded read succeeds")
            .len(),
        10
    );
    assert_eq!(
        read_bounded(&over_cap, 4096)
            .expect("a bounded read succeeds")
            .len(),
        1024
    );
}

// ---------------------------------------------------------------------------
// Which roots exist at all.
// ---------------------------------------------------------------------------

#[test]
fn the_roots_are_the_nest_and_projects_never_the_home_directory() {
    let roots = path_link_roots(None);
    let Some(home) = dirs::home_dir().and_then(|home| home.canonicalize().ok()) else {
        return;
    };
    assert!(
        !roots.contains(&home),
        "$HOME itself must never be a path-link root: {roots:?}"
    );
    for root in &roots {
        assert!(
            root.starts_with(&home),
            "every root lives under $HOME: {root:?}"
        );
        assert!(root != &home);
    }
    // Whichever of the two exists on this machine is present; a root that
    // does not exist is dropped rather than erroring.
    for candidate in [home.join(".buzz"), home.join("projects")] {
        if let Ok(canonical) = candidate.canonicalize() {
            if canonical.is_dir() {
                assert!(
                    roots.contains(&canonical),
                    "{canonical:?} must be a root: {roots:?}"
                );
            }
        }
    }
}
