//! Regression coverage for [`super::order_search_dirs`] and
//! [`super::resolve_workspace_command_from`]: when the current exe lives
//! inside a macOS `.app` bundle, its own directory must be searched before
//! any workspace `target/` dir — a stray sibling checkout must not shadow
//! the bundle's shipped binary. Split into its own file (rather than
//! `discovery/tests.rs`) because that file is already at its file-size
//! ratchet ceiling.

use std::path::PathBuf;

#[cfg(unix)]
#[test]
fn bundle_exe_prefers_bundle_over_workspace_target() {
    use std::os::unix::fs::PermissionsExt;

    let root = std::env::temp_dir().join(format!("buzz-discovery-bundle-{}", uuid::Uuid::new_v4()));
    // Bundle-shaped path: `<Something>.app/Contents/MacOS/` is the exe's
    // parent directory inside a macOS app bundle.
    let bundle_dir = root.join("Buzz.app").join("Contents").join("MacOS");
    // Stand-in for a workspace `target/release` dir that also happens to
    // exist on the machine (e.g. a sibling checkout) and would otherwise
    // shadow the bundle's own binary.
    let workspace_dir = root.join("workspace-target-release");
    std::fs::create_dir_all(&bundle_dir).expect("create bundle dir");
    std::fs::create_dir_all(&workspace_dir).expect("create workspace dir");

    let write_executable = |dir: &std::path::Path, name: &str| -> PathBuf {
        let bin = dir.join(name);
        std::fs::write(&bin, "").expect("write placeholder");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
            .expect("chmod executable");
        bin
    };

    let bundle_bin = write_executable(&bundle_dir, "buzz-acp");
    let _workspace_bin = write_executable(&workspace_dir, "buzz-acp");

    let dirs = super::order_search_dirs(Some(bundle_dir.clone()), vec![workspace_dir.clone()]);
    assert_eq!(
        dirs.first(),
        Some(&bundle_dir),
        "the bundle dir must be searched before the workspace target dir"
    );

    let resolved = super::resolve_workspace_command_from("buzz-acp", &dirs);
    assert_eq!(
        resolved,
        Some(bundle_bin),
        "resolution must prefer the bundle's binary over the workspace target one"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn existing_discovery_tests_are_unaffected_by_ordering_change() {
    // `order_search_dirs` with no bundle-shaped exe parent must reduce to the
    // pre-existing workspace-first, deduplicated ordering used by every
    // other discovery test.
    let workspace_dirs = vec![PathBuf::from("/a"), PathBuf::from("/b")];
    let dirs = super::order_search_dirs(Some(PathBuf::from("/a")), workspace_dirs);
    assert_eq!(
        dirs,
        vec![PathBuf::from("/a"), PathBuf::from("/b")],
        "a non-bundle exe parent must not be reordered ahead of workspace dirs"
    );
}
