#!/usr/bin/env bash
set -euo pipefail

MODE="dry-run"
for arg in "$@"; do
    case "$arg" in
        --dry-run)
            MODE="dry-run"
            ;;
        --apply)
            MODE="apply"
            ;;
        *)
            echo "usage: cargo-target-gc.sh [--dry-run|--apply]" >&2
            exit 1
            ;;
    esac
done

python3 - "$MODE" << 'PY'
import datetime
import fcntl
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import time

mode = sys.argv[1]

new_cache_root = os.path.expanduser("~/.cache/zs/buzz-cargo-targets")
legacy_cache_root = os.path.expanduser("~/.cache/zs/buzz-targets")
os.makedirs(new_cache_root, exist_ok=True)


def fatal(msg):
    print(f"Fatal: {msg}", file=sys.stderr)
    sys.exit(1)


# --- Symlink guard on the roots themselves ----------------------------------
# A cache root that is itself a symlink means every path we build under it
# resolves through that symlink at deletion time (rm follows directory
# components; only a symlink as the FINAL path segment is left alone). That
# would let a redirected root point deletions at an unrelated directory tree,
# a repo .git, or anywhere else. Refuse to run rather than compensate per-entry.
if os.path.islink(new_cache_root):
    fatal(f"{new_cache_root} is a symlink; refusing to operate on a redirected cache root.")
if os.path.isdir(legacy_cache_root) and os.path.islink(legacy_cache_root):
    fatal(f"{legacy_cache_root} is a symlink; refusing to operate on a redirected cache root.")

canonical_new_root = os.path.realpath(new_cache_root)
canonical_legacy_root = (
    os.path.realpath(legacy_cache_root) if os.path.isdir(legacy_cache_root) else None
)

KEY_RE = re.compile(r"^[0-9a-f]{12}$")
BUILD_MARKER_FILES = ("CACHEDIR.TAG",)
BUILD_SUBDIRS = ("debug", "release")
FOURTEEN_DAYS = 14 * 86400
SIDECAR_ALLOWED_NAMES = {"root", "desktop", ".worktree-path"}


class StatError(Exception):
    pass


def _walk_onerror(err):
    raise StatError(str(err))


def get_dir_stats(dir_path):
    """(total_bytes, iso_mtime, newest_mtime_epoch) for dir_path.

    Raises StatError if ANY part of the tree could not be examined. A
    directory this function cannot fully see must never be reported as
    "old enough to delete" — callers treat StatError as unknown-state and
    fail closed (skip, don't touch)."""
    if not os.path.isdir(dir_path):
        return 0, None, 0.0
    total_bytes = 0
    try:
        newest_mtime = os.lstat(dir_path).st_mtime
    except OSError as e:
        raise StatError(str(e))
    for root, dirs, files in os.walk(dir_path, onerror=_walk_onerror):
        for d in dirs:
            try:
                st = os.lstat(os.path.join(root, d))
            except OSError as e:
                raise StatError(str(e))
            if st.st_mtime > newest_mtime:
                newest_mtime = st.st_mtime
        for f in files:
            try:
                st = os.lstat(os.path.join(root, f))
            except OSError as e:
                raise StatError(str(e))
            total_bytes += st.st_size
            if st.st_mtime > newest_mtime:
                newest_mtime = st.st_mtime
    iso_mtime = (
        datetime.datetime.fromtimestamp(newest_mtime, datetime.timezone.utc).strftime(
            "%Y-%m-%dT%H:%M:%SZ"
        )
        if newest_mtime > 0
        else None
    )
    return total_bytes, iso_mtime, newest_mtime


def format_bytes(n):
    if n >= 1024 * 1024 * 1024:
        return f"{n / (1024 * 1024 * 1024):.2f} GB"
    elif n >= 1024 * 1024:
        return f"{n / (1024 * 1024):.2f} MB"
    elif n >= 1024:
        return f"{n / 1024:.2f} KB"
    else:
        return f"{n} B"


def git_probe(args, timeout=30):
    """Run a git safety probe. Returns stdout on a clean (rc==0) exit, or
    None on ANY failure — nonzero exit, timeout, or exec error. Callers
    must treat None as "cannot determine" and fail closed: never interpret
    a failed probe as "clean" or "no unpushed commits"."""
    try:
        p = subprocess.run(args, capture_output=True, text=True, timeout=timeout)
    except (OSError, subprocess.SubprocessError):
        return None
    if p.returncode != 0:
        return None
    return p.stdout


def worktree_list_z(timeout=30):
    """`git worktree list --porcelain -z`, parsed NUL-safely. Returns a list
    of {"path", "head", "branch"} dicts in the order git reports them
    (element 0 is always the main checkout), or None on any failure — exec
    error, nonzero exit, or timeout. The non-`-z` porcelain form is
    newline/whitespace-delimited and silently mis-parses a worktree path
    that itself contains a space or newline; `-z` NUL-delimits every field
    so no path content can be confused with a field or record separator."""
    try:
        p = subprocess.run(
            ["git", "worktree", "list", "--porcelain", "-z"],
            capture_output=True,
            timeout=timeout,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if p.returncode != 0:
        return None
    fields = p.stdout.split(b"\x00")
    if fields and fields[-1] == b"":
        fields = fields[:-1]
    worktrees = []
    current = None
    for raw in fields:
        if raw == b"":
            if current is not None:
                worktrees.append(current)
                current = None
            continue
        text = raw.decode("utf-8", "surrogateescape")
        if current is None:
            current = {"path": None, "head": None, "branch": None}
        if text.startswith("worktree "):
            current["path"] = text[len("worktree "):]
        elif text.startswith("HEAD "):
            current["head"] = text[len("HEAD "):]
        elif text.startswith("branch "):
            ref = text[len("branch "):]
            if ref.startswith("refs/heads/"):
                ref = ref[len("refs/heads/"):]
            current["branch"] = ref
        elif text == "detached":
            current["branch"] = "detached"
        # other fields (bare, locked[, reason], prunable[, reason]) carry no
        # information this GC uses.
    if current is not None:
        worktrees.append(current)
    return worktrees


def lsof_active(path, timeout=30):
    """True if any process has an open file handle under path, OR if that
    cannot be determined — activity checks fail closed toward "assume
    active" rather than toward deletion. lsof's exit code 1 is documented
    to mean BOTH "no matches" and "an error occurred" (permission, I/O,
    a target that vanished mid-scan); the two are told apart only by
    whether lsof also wrote to stderr. A bare rc=1 with stderr output is
    therefore treated as "cannot determine" (active), not "no match"."""
    lsof_bin = shutil.which("lsof")
    if not lsof_bin:
        return True
    try:
        p = subprocess.run([lsof_bin, "+D", path], capture_output=True, text=True, timeout=timeout)
    except (OSError, subprocess.SubprocessError):
        return True
    if p.returncode == 0:
        return bool(p.stdout.strip())
    if p.returncode == 1:
        if p.stderr.strip():
            return True
        return bool(p.stdout.strip())
    # Any other exit code is an lsof error condition.
    return True


def looks_like_build_output(entry_path):
    """A cache directory is only ever eligible for deletion if something
    proves it actually holds Cargo build output, never on directory-name
    pattern alone. Cargo always writes CACHEDIR.TAG at a target dir's root;
    the new layout nests root/desktop target dirs one level down."""
    if any(os.path.exists(os.path.join(entry_path, m)) for m in BUILD_MARKER_FILES):
        return True
    if any(os.path.isdir(os.path.join(entry_path, b)) for b in BUILD_SUBDIRS):
        return True
    try:
        top = os.listdir(entry_path)
    except OSError:
        return False
    for sub in top:
        if sub not in ("root", "desktop"):
            continue
        nested = os.path.join(entry_path, sub)
        if not os.path.isdir(nested):
            continue
        if any(os.path.exists(os.path.join(nested, m)) for m in BUILD_MARKER_FILES):
            return True
        if any(os.path.isdir(os.path.join(nested, b)) for b in BUILD_SUBDIRS):
            return True
    return False


def has_unrecognized_content(entry_path, allowed_names):
    try:
        names = os.listdir(entry_path)
    except OSError:
        return True
    return any(n not in allowed_names for n in names)


def contains_git(entry_path):
    """Full-depth check for a nested .git anywhere under the candidate. The
    deletion contract is that no .git directory is ever touched; a shallow
    check of only the entry root and its known root/desktop subtrees misses
    a repository (or arbitrary directory tree) landed deeper, e.g. under
    root/debug/scratch/.git. os.walk does not follow symlinked directories
    by default, so a symlink cycle cannot make this loop forever — the walk
    is bounded by the real (non-symlink) directory tree, which is finite.
    Any traversal error (permission, a path vanishing mid-walk) is treated
    as "contains a .git" — fail closed toward skipping, not deleting."""
    try:
        for root, dirs, files in os.walk(entry_path, onerror=_walk_onerror):
            if ".git" in dirs or ".git" in files:
                return True
    except StatError:
        return True
    return False


def marker_valid(entry_path, expected_key):
    """Read and authenticate this entry's `.worktree-path` sidecar: it must
    exist, be a regular readable non-empty file, and hash back to the
    entry's own key. Returns (True, recorded_path) or (False, None)."""
    sidecar = os.path.join(entry_path, ".worktree-path")
    if not os.path.isfile(sidecar) or os.path.islink(sidecar):
        return False, None
    try:
        with open(sidecar, "r", encoding="utf-8") as sf:
            recorded_path = sf.read().strip()
    except OSError:
        return False, None
    if not recorded_path:
        return False, None
    if hashlib.sha256(recorded_path.encode()).hexdigest()[:12] != expected_key:
        return False, None
    return True, recorded_path


def skip_row(cache_key, entry_path, reason, bytes_count=0, iso_mtime=None):
    return {
        "worktree_path": None,
        "branch": None,
        "head_sha": None,
        "upstream": None,
        "status": "skipped",
        "reason": reason,
        "cache_key": cache_key,
        "cache_bytes": bytes_count,
        "last_mtime": iso_mtime,
        "size_flag": bytes_count > 5 * 1024 * 1024 * 1024,
        "delete_path": None,
    }


# Step 0: Guard. The canonical main checkout is derived ONLY from git's own
# NUL-safe porcelain output (always the first worktree entry) — there is no
# environment override. An override that a caller can set to "whatever
# directory I happen to be in" defeats the guard it is supposed to enforce,
# so it does not exist here.
wt_list = worktree_list_z()
if wt_list is None:
    fatal("git worktree list --porcelain -z failed; cannot safely determine live worktrees.")
if not wt_list or not wt_list[0]["path"]:
    fatal("git worktree list --porcelain -z returned no worktrees; cannot safely determine the main checkout.")
main_worktree = wt_list[0]["path"]

current_worktree_out = git_probe(["git", "rev-parse", "--show-toplevel"])
if current_worktree_out is None:
    fatal("git rev-parse --show-toplevel failed.")
current_worktree = current_worktree_out.strip()
if current_worktree != main_worktree:
    fatal(
        f"cargo-target-gc must be run from main checkout ({main_worktree}), "
        f"currently in {current_worktree}"
    )

# Step 1: Forward pass — worktrees already parsed above (NUL-safe).
worktrees = wt_list
live_worktrees_by_path = {wt["path"]: wt for wt in worktrees if wt["path"]}

legacy_basenames_live = {}
for wt in worktrees:
    b = os.path.basename(wt["path"].rstrip("/"))
    legacy_basenames_live.setdefault(b, []).append(wt["path"])

rows = []
processed_cache_paths = set()
live_wt_new_cache = {}
live_wt_legacy_cache = {}

# Step 2: Reverse pass — new (keyed) cache dirs.
for entry in sorted(os.listdir(new_cache_root)):
    entry_path = os.path.join(new_cache_root, entry)
    if os.path.islink(entry_path):
        rows.append(skip_row(entry, entry_path, "symlink-entry"))
        continue
    if not os.path.isdir(entry_path):
        continue

    if not KEY_RE.match(entry):
        rows.append(skip_row(entry, entry_path, "unrecognized-entry"))
        continue

    canonical_entry = os.path.realpath(entry_path)
    if (
        os.path.dirname(canonical_entry) != canonical_new_root
        or os.path.basename(canonical_entry) != entry
    ):
        rows.append(skip_row(entry, entry_path, "path-escape"))
        continue

    if has_unrecognized_content(entry_path, SIDECAR_ALLOWED_NAMES):
        rows.append(skip_row(entry, entry_path, "unrecognized-content"))
        continue

    if contains_git(entry_path):
        rows.append(skip_row(entry, entry_path, "contains-git"))
        continue

    marker_ok, recorded_path = marker_valid(entry_path, entry)
    if not marker_ok:
        # Missing, unreadable, empty, or hash-mismatched marker: a build in
        # progress (or an interrupted sidecar write) can leave exactly this
        # state. Never treat it as orphaned — fail closed.
        rows.append(skip_row(entry, entry_path, "missing-marker"))
        continue

    if not looks_like_build_output(entry_path):
        rows.append(skip_row(entry, entry_path, "no-build-marker"))
        continue

    if recorded_path in live_worktrees_by_path:
        live_wt_new_cache[recorded_path] = (entry, entry_path, canonical_entry)
        continue

    try:
        bytes_count, iso_mtime, mtime_epoch = get_dir_stats(entry_path)
    except StatError:
        rows.append(skip_row(entry, entry_path, "stat-error"))
        continue

    if lsof_active(entry_path):
        rows.append(skip_row(entry, entry_path, "orphaned-active", bytes_count, iso_mtime))
        processed_cache_paths.add(canonical_entry)
        continue

    rows.append({
        "worktree_path": recorded_path,
        "branch": None,
        "head_sha": None,
        "upstream": None,
        "status": "orphaned",
        "reason": "worktree-removed-or-moved",
        "cache_key": entry,
        "cache_bytes": bytes_count,
        "last_mtime": iso_mtime,
        "mtime_epoch": mtime_epoch,
        "size_flag": bytes_count > 5 * 1024 * 1024 * 1024,
        "delete_path": canonical_entry,
    })
    processed_cache_paths.add(canonical_entry)

# Step 2: Reverse pass — legacy cache dirs (no marker; matched by basename
# against the COMPLETE live worktree list, not a single hard-coded parent
# directory). A legacy entry with no live-worktree owner has no way to be
# authenticated as Buzz build output at all, so it is reported for visibility
# but never auto-deleted — that risk (an unrelated directory that happens to
# share a name) is exactly what an unauthenticated legacy cache cannot rule
# out, and this GC only ever deletes what it can prove is disposable.
if canonical_legacy_root:
    for entry in sorted(os.listdir(legacy_cache_root)):
        entry_path = os.path.join(legacy_cache_root, entry)
        if os.path.islink(entry_path):
            rows.append(skip_row(entry, entry_path, "symlink-entry"))
            continue
        if not os.path.isdir(entry_path):
            continue

        canonical_entry = os.path.realpath(entry_path)
        if (
            os.path.dirname(canonical_entry) != canonical_legacy_root
            or os.path.basename(canonical_entry) != entry
        ):
            rows.append(skip_row(entry, entry_path, "path-escape"))
            continue

        if contains_git(entry_path):
            rows.append(skip_row(entry, entry_path, "contains-git"))
            continue

        if not looks_like_build_output(entry_path):
            rows.append(skip_row(entry, entry_path, "no-build-marker"))
            continue

        matches = legacy_basenames_live.get(entry, [])
        if len(matches) > 1:
            rows.append(skip_row(entry, entry_path, "ambiguous-legacy-owner"))
            continue
        if len(matches) == 1:
            live_wt_legacy_cache[matches[0]] = (entry, entry_path, canonical_entry)
            continue

        rows.append(skip_row(entry, entry_path, "legacy-unverifiable-owner"))
        processed_cache_paths.add(canonical_entry)

# Step 3 & Step 5: Live worktrees evaluation
now = time.time()
fourteen_days_ago = now - FOURTEEN_DAYS

for wt in worktrees:
    wt_path = wt["path"]
    head_sha = wt["head"]
    branch = wt["branch"]
    upstream = None
    status = None
    reason = None

    if wt_path == main_worktree:
        status = "skipped"
        reason = "main-checkout"
    elif not os.path.exists(wt_path):
        status = "skipped"
        reason = "path-gone"
    else:
        status_out = git_probe(["git", "-C", wt_path, "status", "--porcelain=v1"])
        if status_out is None:
            status = "skipped"
            reason = "probe-failed"
        elif status_out.strip():
            status = "skipped"
            reason = "dirty"
        else:
            up_out = git_probe(
                ["git", "-C", wt_path, "rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]
            )
            if up_out is None:
                status = "skipped"
                reason = "no-upstream"
            else:
                upstream = up_out.strip()
                rev_out = git_probe(["git", "-C", wt_path, "rev-list", "@{u}..HEAD"])
                if rev_out is None:
                    status = "skipped"
                    reason = "probe-failed"
                elif rev_out.strip():
                    status = "skipped"
                    reason = "unpushed"

    caches = []
    if wt_path in live_wt_new_cache:
        caches.append(live_wt_new_cache[wt_path])
    if wt_path in live_wt_legacy_cache:
        caches.append(live_wt_legacy_cache[wt_path])

    if not caches:
        final_status = status if status is not None else "skipped"
        final_reason = reason if reason is not None else "no-cache"
        rows.append({
            "worktree_path": wt_path,
            "branch": branch,
            "head_sha": head_sha,
            "upstream": upstream,
            "status": final_status,
            "reason": final_reason,
            "cache_key": None,
            "cache_bytes": 0,
            "last_mtime": None,
            "size_flag": False,
            "delete_path": None,
        })
    else:
        for c_key, c_path, c_canonical in caches:
            processed_cache_paths.add(c_canonical)
            try:
                c_bytes, c_iso_mtime, c_mtime = get_dir_stats(c_path)
            except StatError:
                rows.append({
                    "worktree_path": wt_path,
                    "branch": branch,
                    "head_sha": head_sha,
                    "upstream": upstream,
                    "status": "skipped",
                    "reason": "stat-error",
                    "cache_key": c_key,
                    "cache_bytes": 0,
                    "last_mtime": None,
                    "size_flag": False,
                    "delete_path": None,
                })
                continue

            if status is not None:
                c_status = status
                c_reason = reason
                delete_target = None
            elif c_mtime >= fourteen_days_ago:
                c_status = "skipped"
                c_reason = "recent"
                delete_target = None
            elif lsof_active(c_path):
                c_status = "skipped"
                c_reason = "active-use-detected"
                delete_target = None
            else:
                c_status = "candidate"
                c_reason = None
                delete_target = c_canonical

            rows.append({
                "worktree_path": wt_path,
                "branch": branch,
                "head_sha": head_sha,
                "upstream": upstream,
                "status": c_status,
                "reason": c_reason,
                "cache_key": c_key,
                "cache_bytes": c_bytes,
                "last_mtime": c_iso_mtime,
                "mtime_epoch": c_mtime,
                "size_flag": c_bytes > 5 * 1024 * 1024 * 1024,
                "delete_path": delete_target,
            })

# Step 4: Write manifest BEFORE deleting anything
manifest_time = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
manifest_path = os.path.join(new_cache_root, f"gc-manifest-{manifest_time}.jsonl")
with open(manifest_path, "w", encoding="utf-8") as mf:
    for r in rows:
        entry = {k: r[k] for k in (
            "worktree_path", "branch", "head_sha", "upstream", "status",
            "reason", "cache_key", "cache_bytes", "last_mtime", "size_flag",
        )}
        mf.write(json.dumps(entry) + "\n")

candidates = [r for r in rows if r.get("delete_path")]
skipped = [r for r in rows if r["status"] == "skipped"]

print(f"=== Cargo Target GC ({mode}) ===")
print(f"Manifest written to: {manifest_path}\n")

if skipped:
    print("--- Skipped Targets ---")
    for r in skipped:
        size_str = format_bytes(r["cache_bytes"])
        branch_str = r["branch"] or "N/A"
        head_str = r["head_sha"][:12] if r["head_sha"] else "N/A"
        print(
            f"[skipped]   {r['worktree_path'] or r['cache_key']} (branch:"
            f" {branch_str}, HEAD: {head_str}, size: {size_str}) - reason:"
            f" {r['reason']}"
        )
    print()

if candidates:
    print("--- Eviction Candidates ---")
    for r in candidates:
        size_str = format_bytes(r["cache_bytes"])
        branch_str = r["branch"] or "N/A"
        head_str = r["head_sha"][:12] if r["head_sha"] else "N/A"
        reason_str = f" - reason: {r['reason']}" if r["reason"] else ""
        print(
            f"[{r['status']}]  {r['delete_path']} | Size: {size_str}"
            f" ({r['cache_bytes']} bytes) | Branch: {branch_str} | HEAD:"
            f" {head_str} | Status: {r['status']}{reason_str}"
        )
    print()

total_bytes = sum(r["cache_bytes"] for r in candidates)
print(f"Total candidates: {len(candidates)} ({format_bytes(total_bytes)} reclaimable)")

if mode == "dry-run":
    print("Dry-run complete: no files deleted. Use --apply to execute deletion.")
    sys.exit(0)

# --- Apply: fence the whole pass, then revalidate each candidate immediately
# before removing it -----------------------------------------------------
# The scan above can be arbitrarily stale by the time we get here (this loop
# itself takes time, and nothing but this same process's own sequencing
# stands between "scanned" and "deleted"). Two defenses, neither of which
# alone is sufficient:
#
#   1. An exclusive lock over the cache root for the ENTIRE apply pass, so
#      two `cargo-target-gc.sh --apply` invocations (including one that
#      bypasses the Justfile's broader with-gate-lock.sh wrapper via direct
#      invocation) can never revalidate-then-delete the same entries at
#      once.
#   2. A full re-authentication of each candidate immediately before its own
#      deletion — not just "is it still there", but every check that made it
#      eligible in the first place (marker, content allowlist, nested-.git,
#      build-output marker, byte/mtime identity since the scan) — because an
#      ordinary `cargo build` takes none of this GC's locks and can start
#      writing into a candidate at any point between the scan and the
#      delete. This narrows that race to the gap between the last
#      revalidation check and the `rmtree` call itself, which is the
#      smallest window achievable without making Cargo itself lock-aware
#      (out of scope here — see storage-build-spec.md).

APPLY_LOCK_PATH = os.path.join(new_cache_root, ".gc-apply.lock")
_lock_fd = os.open(APPLY_LOCK_PATH, os.O_RDWR | os.O_CREAT, 0o644)
try:
    fcntl.flock(_lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
except BlockingIOError:
    fatal(
        f"another cargo-target-gc.sh --apply is already running ({APPLY_LOCK_PATH} "
        "is held); refusing to run concurrently."
    )
os.ftruncate(_lock_fd, 0)
os.write(_lock_fd, f"{os.getpid()}\n".encode())
# _lock_fd is intentionally never closed: the flock is held until this
# process exits, which is the entire remaining apply pass.


def revalidate(row, path):
    if not os.path.isdir(path):
        return False, "path-gone"
    real_now = os.path.realpath(path)
    if real_now != path:
        return False, "path-changed-identity"
    in_root = any(
        root and os.path.dirname(real_now) == root
        for root in (canonical_new_root, canonical_legacy_root)
    )
    if not in_root:
        fatal(f"delete path '{real_now}' escaped the allowed cache roots! Aborting run.")
    if os.path.islink(path):
        return False, "became-symlink"
    if lsof_active(path):
        return False, "active-use-detected"

    # Refuse if the entry's content changed since it was scanned: an
    # unrelated size/mtime shift under a cache dir this GC is about to
    # delete means something (almost certainly a build) wrote to it after
    # the plan was made.
    try:
        bytes_now, _iso_now, mtime_now = get_dir_stats(path)
    except StatError:
        return False, "stat-error"
    if bytes_now != row.get("cache_bytes") or mtime_now != row.get("mtime_epoch"):
        return False, "changed-since-plan"

    if contains_git(path):
        return False, "contains-git-at-apply"
    if not looks_like_build_output(path):
        return False, "no-build-marker-at-apply"

    if row["status"] == "orphaned":
        cache_key = row.get("cache_key")
        if cache_key:
            marker_ok, _recorded = marker_valid(path, cache_key)
            if not marker_ok:
                return False, "marker-invalid-at-apply"
        if has_unrecognized_content(path, SIDECAR_ALLOWED_NAMES):
            return False, "unrecognized-content-at-apply"
        wt_list_now = worktree_list_z()
        if wt_list_now is None:
            return False, "probe-failed"
        live_now = {wt["path"] for wt in wt_list_now if wt["path"]}
        wt_path = row.get("worktree_path")
        if wt_path and wt_path in live_now:
            return False, "worktree-reappeared"
        return True, "ok"

    # status == "candidate": a live, clean, pushed, idle worktree cache.
    wt_path = row["worktree_path"]
    if wt_path == main_worktree:
        return False, "is-main"
    if not os.path.isdir(wt_path):
        return False, "worktree-path-gone"
    status_out = git_probe(["git", "-C", wt_path, "status", "--porcelain=v1"])
    if status_out is None or status_out.strip():
        return False, "now-dirty-or-probe-failed"
    up_out = git_probe(
        ["git", "-C", wt_path, "rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]
    )
    if up_out is None:
        return False, "now-no-upstream"
    rev_out = git_probe(["git", "-C", wt_path, "rev-list", "@{u}..HEAD"])
    if rev_out is None or rev_out.strip():
        return False, "now-unpushed-or-probe-failed"
    if mtime_now >= time.time() - FOURTEEN_DAYS:
        return False, "now-recent"
    return True, "ok"


if not candidates:
    print("No candidates to delete.")
    sys.exit(0)

deleted = 0
for r in candidates:
    path = r["delete_path"]
    ok, why = revalidate(r, path)
    if not ok:
        print(f"Skipping (revalidation: {why}): {path}")
        continue
    print(f"Deleting: {path}")
    shutil.rmtree(path)
    deleted += 1

print(f"Successfully deleted {deleted} candidate director{'y' if deleted == 1 else 'ies'}.")
PY
