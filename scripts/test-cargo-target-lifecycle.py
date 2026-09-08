#!/usr/bin/env python3
"""Exercise real target scripts only in disposable repositories and homes."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

SOURCE = Path(__file__).resolve().parent


class LifecycleTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="buzz-target-test-")
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name).resolve()
        self.home = self.base / "home"
        self.home.mkdir()
        self.repo = self.base / "repo"
        self.repo.mkdir()
        self.environment = {"HOME": str(self.home), "PATH": os.environ["PATH"], "CI": "",
                            "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull}
        self.git("init", "-q")
        (self.repo / "scripts/zs").mkdir(parents=True)
        for name in ["cargo-target-dir.sh", "cargo-target-gc.sh"]:
            shutil.copy2(SOURCE / "zs" / name, self.repo / "scripts/zs" / name)
        self.git("add", ".")
        self.git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.test",
                 "-c", "core.hooksPath=/dev/null", "commit", "-qm", "fixture")

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.repo, env=self.environment,
                              check=True, capture_output=True)

    def helper(self, repo=None, **overrides):
        return subprocess.run(["bash", str((repo or self.repo) / "scripts/zs/cargo-target-dir.sh"), "root"],
                              cwd=self.repo, env=dict(self.environment, **overrides),
                              capture_output=True, check=True).stdout[:-1]

    def scan(self, *args):
        return subprocess.run(["bash", "scripts/zs/cargo-target-gc.sh", *args], cwd=self.repo,
                              env=self.environment, capture_output=True, text=True)

    def cache(self, owner):
        cache = self.home / ".cache/zs/buzz-cargo-targets" / hashlib.sha256(os.fsencode(owner)).hexdigest()[:12]
        (cache / "root").mkdir(parents=True)
        (cache / ".worktree-path").write_bytes(os.fsencode(owner))
        (cache / "root/CACHEDIR.TAG").write_text("Signature: 8a477f597d28d172789f06886806bc55")
        return cache

    def test_apply_refuses_unfenced_deletion_even_for_orphan(self):
        cache = self.cache(str(self.base / "removed"))
        result = self.scan("--apply")
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertTrue(cache.exists())
        self.assertIn("report-only", result.stderr)

    def test_exact_whitespace_identity(self):
        for ending in [" ", "\n"]:
            with self.subTest(ending=repr(ending)):
                worktree = self.base / ("linked" + ending)
                self.git("worktree", "add", "--detach", str(worktree))
                target = Path(os.fsdecode(self.helper(worktree)))
                self.assertEqual((target.parent / ".worktree-path").read_bytes(), os.fsencode(worktree))
                (target / "CACHEDIR.TAG").touch()
                result = self.scan()
                self.assertEqual(result.returncode, 0, result.stderr)
                manifest = sorted((self.home / ".cache/zs/buzz-cargo-targets").glob("gc-manifest-*"))[-1]
                rows = [json.loads(line) for line in manifest.read_text().splitlines()]
                matching = [row for row in rows if row["cache_key"] == target.parent.name]
                self.assertTrue(matching)
                self.assertNotEqual(matching[0]["status"], "orphaned")

    def test_explicit_target_override_is_preserved(self):
        override = str(self.base / "shared root")
        self.assertEqual(self.helper(BUZZ_ROOT_TARGET_DIR=override), os.fsencode(override))

    def test_ci_uses_default_target(self):
        self.assertEqual(self.helper(CI="true"), os.fsencode(self.repo / "target"))

    def test_cross_worktree_cwd_selects_script_owner(self):
        other = self.base / "other"
        self.git("worktree", "add", "--detach", str(other))
        target = Path(os.fsdecode(self.helper(other)))
        self.assertEqual((target.parent / ".worktree-path").read_bytes(), os.fsencode(other))

    def test_nested_git_is_not_candidate(self):
        cache = self.cache(str(self.base / "removed"))
        nested = cache / "root/debug/a/b/c/.git"
        nested.mkdir(parents=True)
        result = self.scan()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("contains-git", result.stdout)
        self.assertTrue(nested.exists())

    def test_authenticated_crash_residue_does_not_hide_cache(self):
        owner = str(self.base / "removed")
        cache = self.cache(owner)
        residue = cache / ".worktree-path.abcd1234"
        residue.write_bytes(os.fsencode(owner)[:8])
        result = self.scan()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("unrecognized-content", result.stdout)
        self.assertIn("orphaned", result.stdout)
        self.assertTrue(residue.exists(), "report must not remove crash residue")

    def test_unrelated_content_stays_unrecognized(self):
        cache = self.cache(str(self.base / "removed"))
        (cache / ".worktree-path.abcd1234").write_text("unrelated notes")
        result = self.scan()
        self.assertIn("unrecognized-content", result.stdout)

    def test_recorded_entrypoints_select_managed_targets(self):
        for name in ["run-tests.sh", "_goose-env.sh", "build-sprig.sh", "instance-env.sh"]:
            with self.subTest(name=name):
                self.assertIn("cargo-target-dir.sh", (SOURCE / name).read_text())
        wrapper = (SOURCE.parent / "desktop/scripts/tauri-command.mjs").read_text()
        self.assertIn("cargo-target-dir.sh", wrapper)
        justfile = (SOURCE.parent / "Justfile").read_text()
        self.assertNotIn("./target/release/buzz-acp", justfile)

    def test_fork_hooks_and_script_changes_select_required_checks(self):
        hooks = (SOURCE.parent / "lefthook.yml").read_text()
        self.assertNotIn("origin/main...HEAD", hooks)
        self.assertIn("origin/zs/main...HEAD", hooks)
        workflow = (SOURCE.parent / ".github/workflows/ci.yml").read_text()
        rust_filter = workflow.split("            rust:", 1)[1].split("            desktop:", 1)[0]
        self.assertIn("'scripts/**'", rust_filter)
        self.assertIn("'Justfile'", rust_filter)
        self.assertIn("python3 scripts/test-cargo-target-lifecycle.py", workflow)

    def test_root_symlink_fails_closed(self):
        root = self.home / ".cache/zs"
        root.mkdir(parents=True)
        (root / "buzz-cargo-targets").symlink_to(self.repo, target_is_directory=True)
        result = self.scan()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("symlink", result.stderr)


if __name__ == "__main__":
    unittest.main()
