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
printf '%s' "$worktree_root" > "$cache_dir/.worktree-path"
printf '%s\n' "$cache_dir/$subtree"
