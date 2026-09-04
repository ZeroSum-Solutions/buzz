// `include!`d directly into `discovery::tests` (see the `include!` call site
// in `tests.rs`) rather than declared as a `mod`, so this test's path is
// exactly `discovery::tests::bundle_exe_prefers_bundle_over_workspace_target`
// as the ticket names it — a `mod bundle_search;` here would add a module
// segment and break that exact path. Additional bundle-search coverage that
// does not need this exact path lives in the sibling `discovery::bundle_search_tests`
// module instead (`discovery/bundle_search_tests.rs`).

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

    let dirs = order_search_dirs(Some(bundle_dir.clone()), vec![workspace_dir.clone()]);
    assert_eq!(
        dirs.first(),
        Some(&bundle_dir),
        "the bundle dir must be searched before the workspace target dir"
    );

    let resolved = resolve_workspace_command_from("buzz-acp", &dirs);
    assert_eq!(
        resolved,
        Some(bundle_bin),
        "resolution must prefer the bundle's binary over the workspace target one"
    );

    let _ = std::fs::remove_dir_all(root);
}
