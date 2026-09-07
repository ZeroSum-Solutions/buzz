#!/usr/bin/env bash
set -euo pipefail

subtree="${1:?usage: cargo-target-dir.sh \{root|desktop\}}"
case "$subtree" in
    root|desktop) ;;
    *) echo "usage: cargo-target-dir.sh {root|desktop}" >&2; exit 1 ;;
esac

# Derive the worktree root from THIS SCRIPT's own location, not the caller's
# CWD. A caller that resolves this script by full/relative path but has not
# itself `cd`'d into that worktree (e.g. a script invoked from a different
# worktree's shell) would otherwise key the wrong worktree's build into this
# one's cache slot — `git rev-parse --show-toplevel` alone answers "what
# worktree is the CWD in", not "what worktree is this script part of".
worktree_root="$(git -C "$(dirname -- "$0")" rev-parse --show-toplevel)"
key="$(printf '%s' "$worktree_root" | shasum -a 256 | cut -c1-12)"
cache_dir="$HOME/.cache/zs/buzz-cargo-targets/$key"
mkdir -p "$cache_dir/$subtree"
# Atomic write: a reader (the GC) must never observe a partially written or
# truncated marker. Write to a sibling temp file and rename (same
# filesystem, so the rename is atomic) rather than truncating in place. An
# untrapped leftover `.worktree-path.XXXXXX` file is unrecognized content to
# the GC, which then never treats this entry's marker as valid again — so
# EXIT always cleans it up, and INT/TERM clean up AND terminate explicitly
# (a bare `trap cmd EXIT INT TERM` with no exit in cmd does not itself stop
# the script on a caught signal; it resumes at the interrupted command).
tmp_sidecar="$(mktemp "$cache_dir/.worktree-path.XXXXXX")"
cleanup_tmp_sidecar() { rm -f "$tmp_sidecar"; }
trap cleanup_tmp_sidecar EXIT
trap 'cleanup_tmp_sidecar; trap - EXIT; exit 130' INT
trap 'cleanup_tmp_sidecar; trap - EXIT; exit 143' TERM
printf '%s' "$worktree_root" > "$tmp_sidecar"
mv -f "$tmp_sidecar" "$cache_dir/.worktree-path"
trap - EXIT INT TERM
printf '%s\n' "$cache_dir/$subtree"
