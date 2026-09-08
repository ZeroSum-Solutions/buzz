#!/usr/bin/env bash
set -euo pipefail
exec python3 - "$0" "${1:-}" <<'PY'
import hashlib
import os
import subprocess
import sys
import tempfile

script, subtree = sys.argv[1:]
if subtree not in ("root", "desktop"):
    sys.exit("usage: cargo-target-dir.sh {root|desktop}")
# Respect caller-selected build caches, including the closeout build lane.
override = os.environ.get("BUZZ_ROOT_TARGET_DIR" if subtree == "root" else "BUZZ_DESKTOP_TARGET_DIR")
if override:
    print(override)
    sys.exit(0)
# Remove exactly Git's output delimiter, never whitespace belonging to the path.
raw_root = subprocess.check_output(["git", "-C", os.path.dirname(os.path.abspath(script)),
                                    "rev-parse", "--show-toplevel"])
if not raw_root.endswith(b"\n"):
    sys.exit("git returned an unterminated worktree path")
root = os.fsdecode(raw_root[:-1])
if os.environ.get("CI"):
    print(os.path.join(root, "target" if subtree == "root" else "desktop/src-tauri/target"))
    sys.exit(0)
key = hashlib.sha256(os.fsencode(root)).hexdigest()[:12]
cache = os.path.expanduser("~/.cache/zs/buzz-cargo-targets")
if os.path.islink(cache) or os.path.islink(os.path.join(cache, key)):
    sys.exit("refusing a symlinked Cargo cache root or entry")
entry = os.path.join(cache, key)
os.makedirs(os.path.join(entry, subtree), exist_ok=True)
# Same-directory rename keeps readers from observing a partial marker. A crash
# may leave a temp file; the report recognizes it without deleting any content.
fd, temporary = tempfile.mkstemp(prefix=".worktree-path.", dir=entry)
try:
    with os.fdopen(fd, "wb") as marker:
        marker.write(os.fsencode(root))
    os.replace(temporary, os.path.join(entry, ".worktree-path"))
finally:
    if os.path.exists(temporary):
        os.unlink(temporary)
print(os.path.join(entry, subtree))
PY
