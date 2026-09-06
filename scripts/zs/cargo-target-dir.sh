#!/usr/bin/env bash
set -euo pipefail

subtree="${1:?usage: cargo-target-dir.sh \{root|desktop\}}"
case "$subtree" in
    root|desktop) ;;
    *) echo "usage: cargo-target-dir.sh {root|desktop}" >&2; exit 1 ;;
esac

worktree_root="$(git rev-parse --show-toplevel)"
key="$(printf '%s' "$worktree_root" | shasum -a 256 | cut -c1-12)"
cache_dir="$HOME/.cache/zs/buzz-cargo-targets/$key"
mkdir -p "$cache_dir/$subtree"
# Atomic write: a reader (the GC) must never observe a partially written or
# truncated marker. Write to a sibling temp file and rename (same
# filesystem, so the rename is atomic) rather than truncating in place.
tmp_sidecar="$(mktemp "$cache_dir/.worktree-path.XXXXXX")"
printf '%s' "$worktree_root" > "$tmp_sidecar"
mv -f "$tmp_sidecar" "$cache_dir/.worktree-path"
printf '%s\n' "$cache_dir/$subtree"
