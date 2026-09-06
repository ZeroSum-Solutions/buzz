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

# Step 0: Guard
main_worktree="${BUZZ_MAIN_WORKTREE:-$(git worktree list --porcelain | awk '/^worktree /{print $2; exit}')}"
current_worktree="$(git rev-parse --show-toplevel)"
if [[ "$current_worktree" != "$main_worktree" ]]; then
    echo "Error: cargo-target-gc must be run from main checkout ($main_worktree), currently in $current_worktree" >&2
    exit 1
fi

delete_list_file="$(mktemp "${TMPDIR:-/tmp}/buzz-gc-candidates.XXXXXX")"
trap 'rm -f "$delete_list_file"' EXIT

python3 - "$MODE" "$main_worktree" "$delete_list_file" << 'PY'
import datetime
import hashlib
import json
import os
import subprocess
import sys
import time

mode = sys.argv[1]
main_worktree = sys.argv[2]
delete_list_file = sys.argv[3]

new_cache_root = os.path.expanduser("~/.cache/zs/buzz-cargo-targets")
legacy_cache_root = os.path.expanduser("~/.cache/zs/buzz-targets")
os.makedirs(new_cache_root, exist_ok=True)

# Step 1: Forward pass - parse git worktree list --porcelain
try:
    wt_out = subprocess.check_output(
        ["git", "worktree", "list", "--porcelain"], text=True
    )
except subprocess.CalledProcessError as e:
    print(f"Error executing git worktree list: {e}", file=sys.stderr)
    sys.exit(1)

worktrees = []
live_worktrees_by_path = {}
for block in wt_out.strip().split("\n\n"):
    lines = block.strip().splitlines()
    if not lines:
        continue
    wt = {"path": None, "head": None, "branch": None, "upstream": None}
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


def get_dir_stats(dir_path):
    total_bytes = 0
    newest_mtime = 0
    if not os.path.isdir(dir_path):
        return 0, None, 0
    try:
        newest_mtime = os.lstat(dir_path).st_mtime
    except OSError:
        pass
    for root, dirs, files in os.walk(dir_path):
        for d in dirs:
            dp = os.path.join(root, d)
            try:
                st = os.lstat(dp)
                if st.st_mtime > newest_mtime:
                    newest_mtime = st.st_mtime
            except OSError:
                pass
        for f in files:
            fp = os.path.join(root, f)
            try:
                st = os.lstat(fp)
                total_bytes += st.st_size
                if st.st_mtime > newest_mtime:
                    newest_mtime = st.st_mtime
            except OSError:
                pass
    iso_mtime = (
        datetime.datetime.fromtimestamp(
            newest_mtime, datetime.timezone.utc
        ).strftime("%Y-%m-%dT%H:%M:%SZ")
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


rows = []
processed_cache_paths = set()
live_wt_new_cache = {}
live_wt_legacy_cache = {}

# Step 2: Reverse pass - new cache dirs
if os.path.isdir(new_cache_root):
    for entry in sorted(os.listdir(new_cache_root)):
        entry_path = os.path.join(new_cache_root, entry)
        if not os.path.isdir(entry_path):
            continue
        sidecar = os.path.join(entry_path, ".worktree-path")
        recorded_path = None
        if os.path.isfile(sidecar):
            try:
                with open(sidecar, "r", encoding="utf-8") as sf:
                    recorded_path = sf.read().strip()
            except OSError:
                pass

        if not recorded_path or recorded_path not in live_worktrees_by_path:
            bytes_count, iso_mtime, _ = get_dir_stats(entry_path)
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
                "delete_path": entry_path,
            })
            processed_cache_paths.add(entry_path)
        else:
            live_wt_new_cache[recorded_path] = (entry, entry_path)

# Step 2: Reverse pass - legacy cache dirs
if os.path.isdir(legacy_cache_root):
    for entry in sorted(os.listdir(legacy_cache_root)):
        entry_path = os.path.join(legacy_cache_root, entry)
        if not os.path.isdir(entry_path):
            continue
        expected_wt_path = os.path.expanduser(f"~/projects/buzz-wt/{entry}")
        if (
            not os.path.exists(expected_wt_path)
            or expected_wt_path not in live_worktrees_by_path
        ):
            bytes_count, iso_mtime, _ = get_dir_stats(entry_path)
            rows.append({
                "worktree_path": expected_wt_path,
                "branch": None,
                "head_sha": None,
                "upstream": None,
                "status": "orphaned",
                "reason": "worktree-removed-or-moved",
                "cache_key": entry,
                "cache_bytes": bytes_count,
                "last_mtime": iso_mtime,
                "size_flag": bytes_count > 5 * 1024 * 1024 * 1024,
                "delete_path": entry_path,
            })
            processed_cache_paths.add(entry_path)
        else:
            live_wt_legacy_cache[expected_wt_path] = (entry, entry_path)

# Step 3 & Step 5: Live worktrees evaluation
now = time.time()
fourteen_days_ago = now - (14 * 86400)

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
        st_proc = subprocess.run(
            ["git", "-C", wt_path, "status", "--porcelain=v1"],
            capture_output=True,
            text=True,
        )
        if st_proc.stdout.strip():
            status = "skipped"
            reason = "dirty"
        else:
            up_proc = subprocess.run(
                [
                    "git",
                    "-C",
                    wt_path,
                    "rev-parse",
                    "--abbrev-ref",
                    "--symbolic-full-name",
                    "@{u}",
                ],
                capture_output=True,
                text=True,
            )
            if up_proc.returncode != 0:
                status = "skipped"
                reason = "no-upstream"
            else:
                upstream = up_proc.stdout.strip()
                rev_proc = subprocess.run(
                    ["git", "-C", wt_path, "rev-list", "@{u}..HEAD"],
                    capture_output=True,
                    text=True,
                )
                if rev_proc.stdout.strip():
                    status = "skipped"
                    reason = "unpushed"

    key = hashlib.sha256(wt_path.encode()).hexdigest()[:12]
    expected_new_cache = os.path.join(new_cache_root, key)

    caches = []
    if wt_path in live_wt_new_cache:
        caches.append(live_wt_new_cache[wt_path])
    elif (
        os.path.isdir(expected_new_cache)
        and expected_new_cache not in processed_cache_paths
    ):
        caches.append((key, expected_new_cache))

    if wt_path in live_wt_legacy_cache:
        caches.append(live_wt_legacy_cache[wt_path])
    else:
        legacy_candidate = os.path.join(
            legacy_cache_root, os.path.basename(wt_path)
        )
        if (
            os.path.isdir(legacy_candidate)
            and legacy_candidate not in processed_cache_paths
        ):
            caches.append((os.path.basename(wt_path), legacy_candidate))

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
            "cache_key": key,
            "cache_bytes": 0,
            "last_mtime": None,
            "size_flag": False,
            "delete_path": None,
        })
    else:
        for c_key, c_path in caches:
            processed_cache_paths.add(c_path)
            c_bytes, c_iso_mtime, c_mtime = get_dir_stats(c_path)
            if status is not None:
                c_status = status
                c_reason = reason
                delete_target = None
            else:
                if c_mtime < fourteen_days_ago:
                    c_status = "candidate"
                    c_reason = None
                    delete_target = c_path
                else:
                    c_status = "skipped"
                    c_reason = "recent"
                    delete_target = None

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
manifest_time = datetime.datetime.now(datetime.timezone.utc).strftime(
    "%Y%m%dT%H%M%SZ"
)
manifest_path = os.path.join(new_cache_root, f"gc-manifest-{manifest_time}.jsonl")
with open(manifest_path, "w", encoding="utf-8") as mf:
    for r in rows:
        entry = {
            "worktree_path": r["worktree_path"],
            "branch": r["branch"],
            "head_sha": r["head_sha"],
            "upstream": r["upstream"],
            "status": r["status"],
            "reason": r["reason"],
            "cache_key": r["cache_key"],
            "cache_bytes": r["cache_bytes"],
            "last_mtime": r["last_mtime"],
            "size_flag": r["size_flag"],
        }
        mf.write(json.dumps(entry) + "\n")

candidates = [r for r in rows if r["status"] in ("candidate", "orphaned")]
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
print(
    f"Total candidates: {len(candidates)} ({format_bytes(total_bytes)}"
    " reclaimable)"
)

with open(delete_list_file, "w", encoding="utf-8") as df:
    for r in candidates:
        if r.get("delete_path"):
            df.write(r["delete_path"] + "\n")
PY

if [[ "$MODE" == "dry-run" ]]; then
    echo "Dry-run complete: no files deleted. Use --apply to execute deletion."
elif [[ "$MODE" == "apply" ]]; then
    if [[ ! -s "$delete_list_file" ]]; then
        echo "No candidates to delete."
        exit 0
    fi

    delete_paths=()
    while IFS= read -r line; do
        [[ -n "$line" ]] && delete_paths+=("$line")
    done < "$delete_list_file"

    allowed_root1="$HOME/.cache/zs/buzz-cargo-targets/"
    allowed_root2="$HOME/.cache/zs/buzz-targets/"

    # Step 7: Assertion check on all delete paths before any deletion
    for path in "${delete_paths[@]}"; do
        if [[ -z "$path" ]]; then
            echo "Fatal: delete path is empty! Aborting run." >&2
            exit 1
        fi
        if [[ "$path" != "$allowed_root1"* && "$path" != "$allowed_root2"* ]]; then
            echo "Fatal: delete path '$path' does not start with allowed cache root! Aborting run." >&2
            exit 1
        fi
        if [[ "$path" == "$allowed_root1" || "$path" == "$allowed_root2" || "$path" == "${allowed_root1%/}" || "$path" == "${allowed_root2%/}" ]]; then
            echo "Fatal: delete path cannot be the cache root itself! Aborting run." >&2
            exit 1
        fi
    done

    # If all assertions pass, perform deletion
    for path in "${delete_paths[@]}"; do
        echo "Deleting: $path"
        rm -rf "$path"
    done
    echo "Successfully deleted ${#delete_paths[@]} candidate directories."
fi
