//! Every place that materializes sidecar placeholders must name this binary.
//!
//! Tauri validates `bundle.externalBin` at compile time, so a desktop config
//! that lists `binaries/buzz-mcp-launch` breaks any job that type-checks the
//! Tauri crate without a placeholder for it. That break lands in CI, not
//! locally, which is exactly the class of drift a test can hold: each site
//! below is read as text and asserted to name this binary, so dropping one
//! fails here rather than in a required job.

use std::path::{Path, PathBuf};

/// The repository root, from this crate's manifest directory.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/<crate> sits two levels under the repository root")
        .to_path_buf()
}

fn read(relative: &str) -> String {
    let path = repo_root().join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{relative} is not readable at {}: {e}. Every path this test names must \
             match `git ls-files` byte for byte, including case: a case-insensitive \
             filesystem hides a mismatch that fails the Linux unit lane.",
            path.display()
        )
    })
}

/// The slice of `text` from `anchor` up to `terminator`, so a match elsewhere
/// in the same file — a release build list, say — cannot stand in for the
/// placeholder step itself.
fn section<'a>(text: &'a str, relative: &str, anchor: &str, terminator: &str) -> &'a str {
    let start = text
        .find(anchor)
        .unwrap_or_else(|| panic!("{relative} no longer contains {anchor:?}"));
    let rest = &text[start + anchor.len()..];
    let end = rest.find(terminator).unwrap_or(rest.len());
    &rest[..end]
}

#[test]
fn every_sidecar_placeholder_site_names_the_launcher() {
    const BINARY: &str = "buzz-mcp-launch";

    for config in ["tauri.conf.json", "tauri.windows.conf.json"] {
        let text = read(&format!("desktop/src-tauri/{config}"));
        assert!(
            text.contains(&format!("binaries/{BINARY}")),
            "{config} no longer lists the launcher, so this test guards nothing; \
             drop it or narrow it deliberately"
        );
    }

    // (file, anchor at the placeholder site, where that site ends)
    let sites = [
        (
            ".github/workflows/_ci-rust.yml",
            "name: Create sidecar placeholders",
            "\n      - name:",
        ),
        (
            ".github/workflows/_ci-desktop-macos.yml",
            "name: Create sidecar placeholders",
            "\n      - name:",
        ),
        // Tracked as `Justfile`; a lowercase spelling only resolves on a
        // case-insensitive filesystem.
        ("Justfile", "_ensure-sidecar-stubs:", "\n\n"),
        ("scripts/bundle-sidecars.sh", "SIDECARS=(", ")"),
    ];

    for (relative, anchor, terminator) in sites {
        let text = read(relative);
        let site = section(&text, relative, anchor, terminator);
        assert!(
            site.contains(BINARY),
            "{relative} creates sidecar placeholders without {BINARY}; the Tauri \
             crate lists it in externalBin and will not compile there"
        );
    }
}
