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

# Step 0: Guard. The canonical main checkout is derived ONLY from git's own
# porcelain output (always the first `worktree` line) — there is no
# environment override. An override that a caller can set to "whatever
# directory I happen to be in" defeats the guard it is supposed to enforce,
# so it does not exist here.
main_worktree="$(git worktree list --porcelain | awk '/^worktree /{print $2; exit}')"
current_worktree="$(git rev-parse --show-toplevel)"
if [[ "$current_worktree" != "$main_worktree" ]]; then
    echo "Error: cargo-target-gc must be run from main checkout ($main_worktree), currently in $current_worktree" >&2
    exit 1
fi

python3 - "$MODE" "$main_worktree" << 'PY'
import datetime
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import time

mode = sys.argv[1]
main_worktree = sys.argv[2]

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


def lsof_active(path, timeout=30):
    """True if any process has an open file handle under path, OR if that
    cannot be determined (lsof missing/erroring/timing out) — activity
    checks fail closed toward "assume active" rather than toward deletion."""
    lsof_bin = shutil.which("lsof")
    if not lsof_bin:
        return True
    try:
        p = subprocess.run([lsof_bin, "+D", path], capture_output=True, text=True, timeout=timeout)
    except (OSError, subprocess.SubprocessError):
        return True
    if p.returncode not in (0, 1):
        # 0 = matches found, 1 = no matches; anything else is an lsof error.
        return True
    return bool(p.stdout.strip())


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
    """Shallow check for a nested .git at the entry root or its known
    first-level subtrees — enough to catch a repo or arbitrary directory
    that landed under the cache root by mistake, without an expensive full
    walk of a multi-gigabyte build tree."""
    candidates = [entry_path]
    for sub in ("root", "desktop"):
        candidates.append(os.path.join(entry_path, sub))
    for c in candidates:
        try:
            names = os.listdir(c)
        except OSError:
            continue
        if ".git" in names:
            return True
    return False


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


# Step 1: Forward pass - parse git worktree list --porcelain
wt_out = git_probe(["git", "worktree", "list", "--porcelain"])
if wt_out is None:
    fatal("git worktree list --porcelain failed; cannot safely determine live worktrees.")

worktrees = []
live_worktrees_by_path = {}
for block in wt_out.strip().split("\n\n"):
    lines = block.strip().splitlines()
    if not lines:
        continue
    wt = {"path": None, "head": None, "branch": None}
    for line in lines:
        if line.startswith("worktree "):
            wt["path"] = line[9:].strip()
        elif line.startswith("HEAD "):
            wt["head"] = line[5:].strip()
        elif line.startswith("branch "):
            ref = line[7:].strip()
            if ref.startswith("refs/heads/"):
                ref = ref[11:]
            wt["branch"] = ref
        elif line == "detached":
            wt["branch"] = "detached"
    if wt["path"]:
        worktrees.append(wt)
        live_worktrees_by_path[wt["path"]] = wt

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

    if has_unrecognized_content(entry_path, {"root", "desktop", ".worktree-path"}):
        rows.append(skip_row(entry, entry_path, "unrecognized-content"))
        continue

    if contains_git(entry_path):
        rows.append(skip_row(entry, entry_path, "contains-git"))
        continue

    sidecar = os.path.join(entry_path, ".worktree-path")
    recorded_path = None
    if os.path.isfile(sidecar) and not os.path.islink(sidecar):
        try:
            with open(sidecar, "r", encoding="utf-8") as sf:
                recorded_path = sf.read().strip()
        except OSError:
            recorded_path = None

    if not recorded_path:
        # Missing, unreadable, or empty marker: a build in progress can
        # leave exactly this state (the sidecar is written right after
        # mkdir). Never treat it as orphaned — fail closed.
        rows.append(skip_row(entry, entry_path, "missing-marker"))
        continue

    expected_key = hashlib.sha256(recorded_path.encode()).hexdigest()[:12]
    if expected_key != entry:
        # The marker doesn't hash back to this directory's own key: don't
        # trust it to authenticate anything about this entry.
        rows.append(skip_row(entry, entry_path, "marker-mismatch"))
        continue

    if not looks_like_build_output(entry_path):
        rows.append(skip_row(entry, entry_path, "no-build-marker"))
        continue

    if recorded_path in live_worktrees_by_path:
        live_wt_new_cache[recorded_path] = (entry, entry_path, canonical_entry)
        continue

    try:
        bytes_count, iso_mtime, _mtime_epoch = get_dir_stats(entry_path)
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

# --- Apply: revalidate each candidate immediately before removing it -------
# The scan above can be arbitrarily stale by the time we get here (this loop
# itself takes time, and nothing but this same process's own sequencing
# stands between "scanned" and "deleted"). Re-check live state, right before
# each individual rm, rather than trusting the earlier snapshot.


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

    if row["status"] == "orphaned":
        wt_out_now = git_probe(["git", "worktree", "list", "--porcelain"])
        if wt_out_now is None:
            return False, "probe-failed"
        live_now = {
            line[9:].strip() for line in wt_out_now.splitlines() if line.startswith("worktree ")
        }
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
    try:
        _, _, mtime_epoch = get_dir_stats(path)
    except StatError:
        return False, "stat-error"
    if mtime_epoch >= time.time() - FOURTEEN_DAYS:
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
