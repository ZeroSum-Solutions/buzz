//! Regression coverage for the app-bundle discovery seam, beyond the one
//! ticket-named test at the exact `discovery::tests::…` path (which is
//! `include!`d directly into `discovery/tests.rs` — see the comment at its
//! call site — because a `mod` here would add a module segment to that
//! path). This file covers the production entry points
//! ([`super::command_search_dirs`], [`super::resolve_command`],
//! [`super::resolve_workspace_command`], [`super::resolve_command_cached`])
//! and the edge cases the named test doesn't exercise: case-insensitive
//! `.app` detection, a non-executable bundle candidate falling through to
//! the workspace binary, and — via the `#[cfg(test)]` exe-parent override in
//! `discovery.rs` — the same bundle-vs-workspace preference exercised
//! through `command_search_dirs()` and `resolve_command()` themselves,
//! rather than only through the extracted `order_search_dirs`/
//! `resolve_workspace_command_from` helpers. Any test that sets the
//! override holds `exe_parent_override_test_lock()` for its whole
//! override-active window; any test whose assertion depends on the *real*
//! (non-overridden) exe parent also takes that lock, so the two classes of
//! test can never interleave.

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
    //
    // Takes the exe-parent-override lock even though it never sets the
    // override itself: this test's assertion depends on the *real* exe
    // parent, so it must not interleave with a test that has the override
    // active (see the module doc comment).
    let _guard = super::exe_parent_override_test_lock();
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

#[test]
fn command_search_dirs_prefers_bundle_exe_parent_when_overridden() {
    // `std::env::current_exe()` inside a `cargo test` process is always the
    // test runner's own binary, which is never itself packaged inside a
    // `.app` bundle — so there is no way to invoke the real
    // `command_search_dirs()` (as opposed to `order_search_dirs` with a
    // hand-passed `exe_parent`) from a process whose *physical* exe parent
    // is bundle-shaped. The `#[cfg(test)]` override in `discovery.rs`
    // closes that gap: it substitutes for `std::env::current_exe()` inside
    // `command_search_dirs()` itself, so this test binds the real
    // production entry point, not a proxy for it.
    let _guard = super::exe_parent_override_test_lock();

    let bundle_root = std::env::temp_dir().join(format!(
        "buzz-discovery-csd-bundle-{}",
        uuid::Uuid::new_v4()
    ));
    let bundle_dir = bundle_root.join("Buzz.app").join("Contents").join("MacOS");
    std::fs::create_dir_all(&bundle_dir).expect("create bundle dir");

    super::set_exe_parent_override_for_test(Some(bundle_dir.clone()));
    let dirs = super::command_search_dirs();
    super::set_exe_parent_override_for_test(None);

    let _ = std::fs::remove_dir_all(&bundle_root);

    assert_eq!(
        dirs.first(),
        Some(&bundle_dir),
        "command_search_dirs() must hoist a bundle-shaped exe parent ahead \
         of the workspace target dirs it also searches, when the process's \
         own exe genuinely resides inside a `.app` bundle"
    );
}

#[cfg(unix)]
#[test]
fn resolve_command_prefers_bundle_binary_over_workspace_target_binary() {
    use std::os::unix::fs::PermissionsExt;

    // `resolve_command()` is the top-level forced-discovery entry point
    // (cache miss -> `resolve_command_uncached` -> `resolve_workspace_command`
    // -> `command_search_dirs`) — this binds that whole real chain, not
    // `resolve_workspace_command_from` with a synthetic dir list.
    let _guard = super::exe_parent_override_test_lock();

    // A real workspace-target binary, planted at the exact directory the
    // real `command_search_dirs()` would search (mirrors the existing seam
    // test's approach).
    let current_dir = std::env::current_dir().expect("current dir");
    let target_dir = super::profile_target_dirs(&current_dir)[0].clone();
    std::fs::create_dir_all(&target_dir).expect("create target dir");

    let marker_name = format!(
        "buzz-discovery-resolve-command-bundle-{}",
        uuid::Uuid::new_v4()
    );
    let workspace_bin = target_dir.join(&marker_name);
    std::fs::write(&workspace_bin, "").expect("write workspace placeholder");
    std::fs::set_permissions(&workspace_bin, std::fs::Permissions::from_mode(0o755))
        .expect("chmod workspace executable");

    // A same-named binary in a synthetic bundle dir, injected as the exe
    // parent via the test-only override (see
    // `command_search_dirs_prefers_bundle_exe_parent_when_overridden` for
    // why the override exists).
    let bundle_root = std::env::temp_dir().join(format!(
        "buzz-discovery-resolve-command-bundle-root-{}",
        uuid::Uuid::new_v4()
    ));
    let bundle_dir = bundle_root.join("Buzz.app").join("Contents").join("MacOS");
    std::fs::create_dir_all(&bundle_dir).expect("create bundle dir");
    let bundle_bin = bundle_dir.join(&marker_name);
    std::fs::write(&bundle_bin, "").expect("write bundle placeholder");
    std::fs::set_permissions(&bundle_bin, std::fs::Permissions::from_mode(0o755))
        .expect("chmod bundle executable");

    super::set_exe_parent_override_for_test(Some(bundle_dir));
    let resolved = super::resolve_command(&marker_name);
    super::set_exe_parent_override_for_test(None);

    let _ = std::fs::remove_file(&workspace_bin);
    let _ = std::fs::remove_dir_all(&bundle_root);

    assert_eq!(
        resolved,
        Some(bundle_bin),
        "resolve_command() -- the top-level forced-discovery entry point -- \
         must resolve the bundle-located binary before the workspace target \
         binary of the same name"
    );
}
