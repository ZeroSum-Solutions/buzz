//! Regression coverage for the app-bundle discovery seam, beyond the one
//! ticket-named test at the exact `discovery::tests::…` path (which is
//! `include!`d directly into `discovery/tests.rs` — see the comment at its
//! call site — because a `mod` here would add a module segment to that
//! path). This file covers the production entry points
//! ([`super::command_search_dirs`], [`super::resolve_workspace_command`],
//! [`super::resolve_command_cached`]) and the edge cases the named test
//! doesn't exercise: case-insensitive `.app` detection and a non-executable
//! bundle candidate falling through to the workspace binary.

use std::path::PathBuf;

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

#[test]
fn is_inside_app_bundle_is_case_insensitive() {
    // APFS (macOS's default filesystem) is case-insensitive-but-preserving,
    // so a bundle directory can legitimately be named `.App` or `.APP`.
    assert!(super::is_inside_app_bundle(&PathBuf::from(
        "/Applications/Buzz.App/Contents/MacOS"
    )));
    assert!(super::is_inside_app_bundle(&PathBuf::from(
        "/Applications/BUZZ.APP/Contents/MacOS"
    )));
    assert!(super::is_inside_app_bundle(&PathBuf::from(
        "/Applications/Buzz.app/Contents/MacOS"
    )));
    assert!(!super::is_inside_app_bundle(&PathBuf::from(
        "/usr/local/bin"
    )));
}

#[cfg(unix)]
#[test]
fn bundle_search_falls_through_non_executable_candidate_to_workspace() {
    use std::os::unix::fs::PermissionsExt;

    let root = std::env::temp_dir().join(format!(
        "buzz-discovery-bundle-nonexec-{}",
        uuid::Uuid::new_v4()
    ));
    let bundle_dir = root.join("Buzz.app").join("Contents").join("MacOS");
    let workspace_dir = root.join("workspace-target-release");
    std::fs::create_dir_all(&bundle_dir).expect("create bundle dir");
    std::fs::create_dir_all(&workspace_dir).expect("create workspace dir");

    // The bundle's own candidate exists but is not executable (e.g. a
    // corrupted or half-installed bundle) — resolution must skip it rather
    // than stop searching, and fall through to the workspace binary.
    let bundle_bin = bundle_dir.join("buzz-acp");
    std::fs::write(&bundle_bin, "").expect("write bundle placeholder");
    std::fs::set_permissions(&bundle_bin, std::fs::Permissions::from_mode(0o644))
        .expect("chmod bundle file non-executable");

    let workspace_bin = workspace_dir.join("buzz-acp");
    std::fs::write(&workspace_bin, "").expect("write workspace placeholder");
    std::fs::set_permissions(&workspace_bin, std::fs::Permissions::from_mode(0o755))
        .expect("chmod workspace file executable");

    let dirs = super::order_search_dirs(Some(bundle_dir.clone()), vec![workspace_dir]);
    assert_eq!(
        dirs.first(),
        Some(&bundle_dir),
        "the bundle dir is still searched first"
    );

    let resolved = super::resolve_workspace_command_from("buzz-acp", &dirs);
    assert_eq!(
        resolved,
        Some(workspace_bin),
        "a non-executable bundle candidate must fall through to the executable \
         workspace one, not stop the search or return the unusable file"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn command_search_dirs_is_exercised_directly_and_deduplicated() {
    // Binds the real production entry point that `resolve_workspace_command`
    // and `resolve_command_cached` actually call — not just `order_search_dirs`
    // with a hand-passed arg list, which no test before this one invoked.
    let dirs = super::command_search_dirs();
    assert!(
        !dirs.is_empty(),
        "command_search_dirs must return at least the exe parent"
    );

    let mut seen = std::collections::HashSet::new();
    assert!(
        dirs.iter().all(|dir| seen.insert(dir.clone())),
        "command_search_dirs must deduplicate its search directories: {dirs:?}"
    );

    // The test binary's own exe does not live inside a `.app` bundle, so per
    // `order_search_dirs`'s contract its parent is appended last, never
    // hoisted ahead of the workspace dirs.
    if let Some(exe_parent) = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(std::path::Path::to_path_buf))
    {
        assert_eq!(
            dirs.last(),
            Some(&exe_parent),
            "a non-bundle exe parent must be searched last, not first"
        );
    }
}

#[cfg(unix)]
#[test]
fn resolve_workspace_command_and_resolve_command_cached_find_real_workspace_binary() {
    use std::os::unix::fs::PermissionsExt;

    // Bind the real, zero-arg production seams -- not
    // `resolve_workspace_command_from` with a synthetic dir list -- by
    // planting a marker executable at the exact directory
    // `command_search_dirs()` itself would search first (mirrors its own
    // `profile_target_dirs(&current_dir())[0]` selection).
    let current_dir = std::env::current_dir().expect("current dir");
    let target_dir = super::profile_target_dirs(&current_dir)[0].clone();
    std::fs::create_dir_all(&target_dir).expect("create target dir");

    let marker_name = format!("buzz-discovery-seam-marker-{}", uuid::Uuid::new_v4());
    let marker_path = target_dir.join(&marker_name);
    std::fs::write(&marker_path, "").expect("write marker");
    std::fs::set_permissions(&marker_path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod marker executable");

    let resolved_workspace = super::resolve_workspace_command(&marker_name);
    let resolved_cached = super::resolve_command_cached(&marker_name);

    let _ = std::fs::remove_file(&marker_path);

    assert_eq!(
        resolved_workspace,
        Some(marker_path.clone()),
        "resolve_workspace_command() must find a real binary via its own \
         command_search_dirs(), not just resolve_workspace_command_from() \
         with a hand-passed dir list"
    );
    assert_eq!(
        resolved_cached,
        Some(marker_path),
        "resolve_command_cached() must find the same sidecar without a \
         forced-discovery warm-up, per its own doc comment"
    );
}
