# Buzz Disk-Growth Prevention Plan (fork-only)

Scope: prevention design only, no implementation. Repo: `/Users/zero-suminc./projects/buzz`
(zs/main). Worktrees under `/Users/zero-suminc./projects/buzz-wt/`. Incident:
`~/Inbox/notes/buzz-storage-debug-init-2026-09-06.md`.

**Revision note (2026-09-06, first pass):** an independent review found that Section 5's
injection point does not work, for two separate reasons, and found one entry point in
Section 1's own fact list that Section 5 drops. See "Refutations considered" (end of
file) for the full record of what was checked and why. This revision replaces the
injection mechanism (Section 5), adds the dropped entry point to the migration list,
corrects the GC mitigation claim (Section 7), and folds in the pre-existing ungoverned
cache and stale worktree count found during re-verification.

**Revision note (2026-09-06, second pass):** a second independent review found one more
concrete break the first pass's own fix introduced (`scripts/bundle-sidecars.sh` ignores
`CARGO_TARGET_DIR` even after `desktop-demo-build`'s cargo line is migrated), one design
bug in the GC recipe (it cannot detect a fully `git worktree remove`d worktree because
its only enumeration source is `git worktree list --porcelain`, which no longer lists
that worktree at all), and one internal contradiction in the cache-key rationale. See
"Refutations considered — second pass" (end of file). This revision adds
`bundle-sidecars.sh` to the migration list with the concrete fix, adds a reverse-pass
enumeration to the GC recipe, corrects the cache-key claim, refreshes the now-stale
worktree/dirty-state facts in Section 1, and folds in the smaller findings (defensive
delete-path check, "pushed" definition, main-checkout runtime guard).

## 1. Confirmed facts

- Two independent Cargo workspaces. Root `Cargo.toml:1-36` lists 33 members;
  `Cargo.toml:37` excludes `desktop/src-tauri`; `:38` `resolver = "2"`.
  `desktop/src-tauri/Cargo.toml:1-6` declares its own `[workspace] members =
  ["crates/buzz-terminal"]`, package `buzz-desktop` (`:9`). Each workspace gets its own
  default `target/` (root: `<repo>/target/`; Tauri: `<repo>/desktop/src-tauri/target/`).
- `CARGO_TARGET_DIR`/`target-dir` unset everywhere. Root `.cargo/config.toml:1-11` sets
  only `[profile.dev] debug` and one env var, no `target-dir`.
  `desktop/src-tauri/.cargo/config.toml` does not exist. Repo grep found no project
  setting of either variable; the one hit, `scripts/test-k8s-provider-release.sh:10`,
  only *reads* `CARGO_TARGET_DIR` as a fallback. `~/.cargo/config.toml` also absent.
- Eight worktrees registered (`git worktree list --porcelain`, re-checked 2026-09-06):
  main + 7 under `buzz-wt/`, all porcelain-clean today. This corrects the original count
  of "main + 6" — the additional worktree is `buzz-wt/agent-health`
  (`feat/agent-health`), which has no upstream and shares HEAD `b42de461c` with
  `harness-fixtures`, a second no-upstream WIP checkout this plan must also treat as
  "must never auto-clean." `git rev-parse --git-common-dir` from main and from
  `buzz-wt/harness-fixtures` both resolve to the same
  `/Users/zero-suminc./projects/buzz/.git`.
- `harness-fixtures` has no upstream (`git rev-parse --abbrev-ref
  --symbolic-full-name @{u}` → `fatal: no upstream configured`) — originally reported as
  the only worktree meeting a "must never auto-clean" bar; corrected above,
  `agent-health` also has no upstream.
- **Correction (second pass, re-checked 2026-09-06 later the same day): both no-upstream
  worktrees are now dirty, and `agent-health` has moved past the HEAD this plan
  originally recorded.** `harness-fixtures` shows 5 modified files under
  `crates/buzz-acp/src/`; `agent-health` shows a modified
  `crates/buzz-acp/src/reliability/ledger.rs` and HEAD `deff99c19`, past the
  `b42de461c` it shared with `harness-fixtures` when this plan was first written — an
  agent was actively building there (a `.fingerprint` file under
  `~/.cache/zs/buzz-targets/agent-health/root/debug/` had an mtime seconds old at
  re-check). This does not change the design: GC re-queries `git status`/upstream live
  at run time (Section 5, steps 1-2), not from this document's snapshot, and
  dirty-or-no-upstream both land on "skip" either way. It does mean this document's
  "confirmed facts" are a point-in-time snapshot, not a source of truth to re-check
  against at implementation time in an actively multi-agent-worked repo — and that
  acceptance test (vi) should assert the skip reason is one of `{dirty, no-upstream}`
  rather than a specific string, since whichever is true at test time is legitimate.
- `.hermit/rust` is Hermit's per-checkout `CARGO_HOME`
  (`.../sources/6682e3c.../rustup.hcl:3-6`: `CARGO_HOME = "${HERMIT_ENV}/.hermit/rust"`),
  a real copy (59,464 files, one stray symlink in main), ~988M-1.2G per checkout
  (7 checkouts ≈ 7.6G). `RUSTUP_HOME` is not remapped by any Hermit manifest and stays
  the shared `~/.rustup` (2.3G, one copy) — toolchains are not part of this problem.
- `Justfile:865-867` (`clean`) already runs `cargo clean` for both workspaces — the only
  existing place treating them as a pair.
- **A pre-existing, currently-active, ungoverned cache already exists today, with TWO
  live consumers, not one.** `~/.cache/zs/buzz-targets/harness-fixtures/{root,tauri}`
  (3.2G at re-check) and **`~/.cache/zs/buzz-targets/agent-health/{root,tauri}` (2.1G at
  re-check, second pass) — a second concurrent consumer this plan's original
  fact-gathering missed, found only on second-pass re-verification.** Both have file
  mtimes minutes old (both worktrees are actively using this path as a worktree-scoped
  `CARGO_TARGET_DIR`/`target-dir` override right now). Both are keyed by worktree
  **basename** (`harness-fixtures`, `agent-health`), not the hashed-absolute-path scheme
  this plan selects in Section 4(b) specifically to avoid name collisions, and `grep -r
  buzz-targets` across the tracked repo returns nothing — it is not produced or governed
  by anything in-repo. The cache root and manifest naming below (Section 5) are chosen
  to avoid colliding with this path; the GC recipe (Section 5) is scoped to know about it
  so it does not keep growing ungoverned once this plan ships. The original single-consumer
  count understated the scale of this pre-existing informal workaround by roughly half.
- ~20+ call sites hard-code the default `target/` path: `Justfile:628,323,328`;
  `scripts/bundle-sidecars.sh:22`; `scripts/start-isolated-test-relay.sh:158`;
  `scripts/start-relay-for-tests.sh:139-180`; `scripts/e2e-git-perms.sh:92-574`;
  `scripts/e2e-large-channel-roster.sh:18-19`; `scripts/zs/openseo-smoke.sh:394`;
  `scripts/pdf-spike/validate.sh:18-19`; `scripts/run-desktop-release-smoke.sh:103`;
  `scripts/instance-env.sh:85`; `.github/workflows/_ci-relay.yml`,
  `release.yml`, linux/windows/macos canary workflows,
  `scripts/test-desktop-release-cache-workflow.sh:24`. Three Justfile recipes
  (`desktop-standalone`, `staging`, `production`; `:668,708,743`) already resolve the
  binary path via `cargo metadata --no-deps` (run from repo root, against the root
  workspace only — verified unaffected by anything below) and are
  `CARGO_TARGET_DIR`-agnostic.
- **`desktop-demo-build` (`Justfile:299-329`) is a fourth hard-coded-path recipe this
  plan's own fact list already names but must not drop from remediation.** It runs a
  cross-compiled (`$TARGET` triple) `pnpm tauri build --features mesh-llm --target
  "$TARGET" --bundles app` (`:317`, a Tauri/Cargo build against the `desktop/src-tauri`
  workspace), then hard-codes `Justfile:323` `APP_PATH="desktop/src-tauri/target/$TARGET/release/bundle/macos/$PRODUCT_NAME.app"`
  and `Justfile:328` the matching `.../bundle/dmg/...dmg` path to run PlistBuddy and
  codesign against. It is neither in the dev-only migration list nor the CI-only,
  unchanged bucket below — see "Refutations considered" for why this breaks once any
  target-dir change ships, and Section 5 for the fix.
- No worktree-creation script exists in-repo; `buzz-wt/` is a documented convention only
  (incident note + throughput/implementation plan docs), not enforced by tooling.
- `scripts/zs/with-gate-lock.sh:1-49` is a machine-wide `flock` serializing heavy local
  gates (`desktop-test`, `desktop-tauri-test`, `cargo test`/`nextest`) across concurrent
  worktrees; fast gates (fmt/clippy/check) are not covered — direct evidence that
  concurrent-worktree contention is a real, previously-observed hazard class.
- No `sccache`, `cargo-sweep`, or `cargo-cache` is installed locally (`which`: not
  found, all three). CI-only sccache exists for one crate subset
  (`.github/workflows/_ci-relay.yml:72-116`, scoped "bounded trial" for PR #5224).
- `scripts/mobile-worktree-clean.sh` is a working, dry-run-capable template: matches
  only worktree-suffixed bundle ids, never touches unsuffixed production installs — the
  closest existing prior art for a worktree-scoped cleanup pattern.

## 2. Inferred causes

- Root cause: unset `CARGO_TARGET_DIR` (default Cargo behavior) × two independent
  workspaces per checkout × worktree count. Each of 7 checkouts independently grew a
  full `target/` and `desktop/src-tauri/target/`, deleted this morning to reclaim
  ~137 GiB machine-wide (~144G → ~9.8G for Buzz).
- Smaller, same-shape contributor: Hermit's per-checkout `CARGO_HOME` re-downloads the
  crates.io registry/git cache per checkout (~7.6G today) — unkeyed, checkout-local,
  multiplied by worktree count, just an order of magnitude smaller than the incident.
- The large hard-coded-`target/`-path surface (1.above) let the growth persist
  silently: nothing needed to know where `target/` lived, so nothing caught it.

## 3. Unresolved questions

- Whether to fold the Hermit `CARGO_HOME` duplication (~7.6G) into this change or defer
  it — this plan defers (Section 4).
- Whether any process outside this repo creates worktrees programmatically and would
  need updating to inject the cache key at creation time (no in-repo script does today).
- Exact eviction age/size thresholds Devin wants — no existing policy to anchor a
  default; Section 5 proposes conservative starting values pending sign-off.
- Whether CI (ephemeral runners, not long-lived worktrees) should adopt the keyed
  scheme — this plan scopes it to local/fork dev only and leaves CI untouched.

## 4. Options considered

**(a) Shared sccache.** Caches compiled objects by content hash; doesn't bound
`target/` size on its own (target holds final artifacts + incremental state, not just
object cache) and isn't installed locally. Complementary follow-on, not the fix.

**(b) Deterministic per-worktree keyed `CARGO_TARGET_DIR` under a bounded cache root,
for BOTH workspaces. Selected — see Section 5.** Key = stable hash of the worktree's
absolute path (stable across branch renames/moves, unlike branch name).

**(c) Keep default isolated targets (status quo).** Zero migration risk, but reproduces
the incident on the next multi-worktree stretch — rejected, fails "prevention."

**Rejected: a single unkeyed target directory shared by concurrent worktrees.** Cargo
target dirs are not built for concurrent multi-checkout writers: build-script output
(`OUT_DIR`), fingerprints, and incremental state are keyed by absolute source path and
profile, not checkout identity — two worktrees on different branches building
concurrently into one shared dir risk stale/incorrect incremental artifacts, file-lock
contention (`with-gate-lock.sh` already documents cross-worktree contention as a real
observed hazard, one layer up, at the test-runner level), and one worktree's
`cargo clean` deleting another's in-flight build. The incident note explicitly rejects
this; nothing here shows it safe. Not adopted under any configuration.

## 5. Recommended change (minimal, fork-only)

**Superseded design note:** the original version of this section injected a single
`CARGO_TARGET_DIR` export via `bin/activate-hermit`, plus a generated
`desktop/src-tauri/.cargo/config.toml`. Review found two independent, verified breaks —
see "Refutations considered" — and building the fix surfaced a third, related Cargo
behavior that would have broken the generated-config half even without those two:
`.cargo/config.toml` discovery walks **up from the invoking process's current working
directory**, not from `--manifest-path`'s directory. Confirmed empirically (scratch
repro, two nested Cargo projects): `cargo metadata --manifest-path
<inner>/Cargo.toml` run from an unrelated CWD reports the *default* `<inner>/target`,
ignoring `<inner>/.cargo/config.toml`'s `target-dir`; the same command run with CWD
inside `<inner>` correctly reports the configured directory. `desktop-tauri-clippy` and
`desktop-tauri-check` (`Justfile:214-219`) invoke cargo with `--manifest-path
{{desktop_tauri_manifest}}` from repo-root CWD, so a generated
`desktop/src-tauri/.cargo/config.toml` would have silently been ignored by exactly
those two lanes even if the env-var collision were fixed. A single config-file-only
design can't cover every invocation style in this repo (`--manifest-path` from root,
`cd` then bare `cargo`, and `pnpm tauri`'s own internal cargo invocation). The
replacement below drops config-file generation and the `bin/activate-hermit` edit
entirely, in favor of an explicit, per-call env-var scope that is correct regardless of
CWD or manifest-path style (confirmed by the same repro: an explicit `CARGO_TARGET_DIR=`
prefix on the command reports the intended directory even with a mismatched CWD).

**Cache root:** `~/.cache/zs/buzz-cargo-targets/<key>/{root,desktop}`, `<key>` = first
12 hex chars of `sha256(<absolute worktree path>)`.

**Correction (second pass): the original "path-based so a worktree move or branch
rename doesn't orphan the cache" claim was self-contradictory and is corrected here.**
Because `<key>` is a hash of the absolute path, only the branch-rename half was ever
true — renaming a branch does not change the worktree's filesystem path, so the key is
stable across it. A worktree **move** changes the absolute path by definition, so it
necessarily changes the hash and orphans the old keyed entry — the design cannot claim
otherwise. This is treated as an accepted, bounded cost rather than fixed by a different
keying scheme (worktree moves are rare and the incident note's concurrency scenario is
about worktree *count*, not moves): the reverse-pass GC step added below (Section 5,
Cleanup recipe, step 0.5) detects and reclaims a moved-away entry the same way it
detects a fully removed worktree's entry, since a move leaves the old cache directory's
recorded path no longer matching any currently registered worktree.

**Key injection — a `scripts/zs/cargo-target-dir.sh` shim, called explicitly at each
cargo/tauri call site, not via shell activation.** `scripts/zs/cargo-target-dir.sh
{root|desktop}`: pure function — `git rev-parse --show-toplevel` to find the worktree
root, hash it, `mkdir -p` the requested subtree, print the keyed path. **Also writes (or
refreshes) a small sidecar file, `<key>/.worktree-path`, containing the absolute
worktree path** — this is the only durable record tying a hash back to the path that
produced it, and it is what the GC reverse pass (Section 5, step 0.5) reads to detect an
orphaned entry after a worktree is removed or moved, since the hash itself cannot be
reversed. No `bin/activate-hermit` edit (that file is Hermit-generated and
self-documents "DO NOT MODIFY" — this design does not need it) and no generated
`.cargo/config.toml` for either workspace (avoids editing the tracked root
`.cargo/config.toml` at all).

- Every `just` recipe that invokes cargo directly for the **root** workspace
  (`fmt`, `check-compile`, `clean`'s first `cargo clean`, `test-unit`, `dev`'s cargo
  build, the `cargo build`/`cargo build --release` lines inside `desktop-standalone`,
  `staging`, `production`, and `desktop-demo-build`) gets one line added: prefix that
  recipe's cargo invocation with `CARGO_TARGET_DIR="$(scripts/zs/cargo-target-dir.sh
  root)"`, scoped to that command only (`VAR=val cmd`, or an `export` inside the
  recipe's own `#!/usr/bin/env bash` body — either way it lives inside that recipe's own
  subshell/process and never leaks to a sibling recipe or an unrelated interactive
  command).
- Every `just` recipe that invokes cargo, or a tool that itself shells out to cargo, for
  the **Tauri** workspace (`desktop-tauri-fmt`, `desktop-tauri-fmt-check`,
  `desktop-tauri-clippy`, `desktop-tauri-check`, `desktop-tauri-test`'s
  `cargo test --workspace`, `clean`'s second `cargo clean --manifest-path
  desktop/src-tauri/Cargo.toml`, and the `pnpm tauri build`/`pnpm exec tauri dev` lines
  in `desktop-standalone`, `staging`, `production`, `desktop-demo-build`) gets the same
  one-line treatment with `scripts/zs/cargo-target-dir.sh desktop`. Env-var scoping
  covers `pnpm tauri`'s internally-invoked cargo the same as a bare `cargo` call, since
  it is a child process inheriting the recipe's exported variable regardless of what CWD
  or `--manifest-path` the tauri CLI uses internally — the one mechanism in this design
  that is not sensitive to invocation style.
- Because every `just`-invoked recipe now carries its own correctly-scoped export, and
  every cargo-invoking lefthook lane already runs through `just` (`lefthook.yml:47-52`
  `run: just fmt` / `just desktop-tauri-fmt`; `:94-97` `run: just test-unit`; `:113-119`
  `run: just desktop-tauri-clippy && just desktop-tauri-test`), lefthook's hook shell
  needs no changes and no dependency on `bin/.lefthookrc` sourcing anything beyond its
  existing PATH prepend — the fix lives entirely in the Justfile, which lefthook already
  calls into for every one of these lanes.

**Named scope boundary, not silently dropped:** this covers every `just`- and
lefthook-invoked cargo call — the paths the incident review found actually drove the
growth (worktree-count × Justfile/lefthook automation, not ad hoc typing). It does
**not** cover a bare `cargo build`/`cargo check` typed directly in an activated shell
outside `just`. Cargo has no per-workspace-safe way to make a shell-wide env var cover
two independent workspaces at once without the same collision this section exists to
avoid (Section 7 risk list), so this plan does not attempt a shell-activation fallback
for the raw-command case. A regression there is visible, not silent: acceptance test
(iii) below asserts no default `target/` reappears for the `just`-driven paths, and this
document records the boundary explicitly rather than claiming blanket coverage.

**Every entry point picks it up** via the Justfile, which is what every dev shell,
every lefthook lane, and every documented workflow already calls into for these
recipes — not via `bin/activate-hermit` or any shell-activation step. CI does **not**
route through `just` the same way for the affected recipes' underlying cargo calls (it
uses `cashapp/activate-hermit@...`, confirmed at `.github/workflows/_ci-rust.yml:37`,
`ci.yml:38`, `_ci-desktop.yml:33`, not `source bin/activate-hermit`); this plan leaves
CI on Cargo's default (untouched) rather than risk the ~20+ hard-coded CI paths above.

**Hard-coded `target/` paths:**
- Dev-only scripts invoked via `just` (`Justfile:628` `dev`;
  `start-isolated-test-relay.sh:158`; `start-relay-for-tests.sh:139-180`;
  `e2e-git-perms.sh`; `e2e-large-channel-roster.sh:18-19`; `zs/openseo-smoke.sh:394`;
  `pdf-spike/validate.sh:18-19`) — migrate each to resolve the binary path via
  `cargo metadata --no-deps | <parse> target_directory`, run from the same CWD as the
  cargo build it follows (the pattern already proven at `Justfile:668,708,743`;
  `cargo metadata`, like any cargo command, needs the matching `CARGO_TARGET_DIR` set on
  the same invocation to agree with where the preceding build actually wrote — covered
  automatically once the preceding recipe line carries the env-var prefix above).
  ~8-10 files, one line each, mechanical.
- **`desktop-demo-build` (`Justfile:299-329`) — added to this migration list; the
  original version of this plan named it in Section 1's fact list but dropped it here.**
  Its `pnpm tauri build ... --target "$TARGET"` line (`:317`) gets the same
  `CARGO_TARGET_DIR="$(scripts/zs/cargo-target-dir.sh desktop)"` prefix as the other
  Tauri-workspace call sites above. `APP_PATH` (`:323`) and the DMG path (`:328`) change
  from the hard-coded `desktop/src-tauri/target/$TARGET/...` to resolve from that same
  variable: `"$(scripts/zs/cargo-target-dir.sh desktop)/$TARGET/release/bundle/macos/$PRODUCT_NAME.app"`
  (cross-compiled artifacts still land under `<target-dir>/<triple>/<profile>/...`, only
  the base changes). Untested by this plan — flag for a real cross-target demo build
  before relying on it.
- **`scripts/bundle-sidecars.sh` (second pass) — a second, distinct break inside the
  same `desktop-demo-build` recipe, one step earlier than the APP_PATH/DMG fix above,
  and the plan's own Section 1 fact list already names it but this migration list had
  dropped it.** The recipe's root-workspace `cargo build --release --target "$TARGET" -p
  buzz-acp ...` line (`Justfile:313-315`) gets the standard
  `CARGO_TARGET_DIR="$(scripts/zs/cargo-target-dir.sh root)"` prefix per the bullet
  above, but the very next line, `./scripts/bundle-sidecars.sh "$TARGET"` (`:316`), calls
  a script that hard-codes `SRC_DIR="target/${TARGET}/release"` (`bundle-sidecars.sh:20`,
  or `SRC_DIR="target/release"` at `:22` with no target arg) as a literal string,
  independent of any env var, checks for each sidecar binary there (`:34`), and exits 1
  with `Error: missing release binaries in $SRC_DIR` (`:37`) if any are absent — which
  they always will be once the preceding build writes to the keyed cache dir instead.
  This holds regardless of whether the `CARGO_TARGET_DIR` export on the cargo line is
  scoped as a one-off `VAR=val cmd` prefix or a whole-recipe `export`, because the
  script itself never reads the variable at all. **Two changes are both required:**
  (1) `bundle-sidecars.sh` must resolve `SRC_DIR` from `${CARGO_TARGET_DIR:-target}`
  instead of the literal `target`, i.e. `SRC_DIR="${CARGO_TARGET_DIR:-target}/${TARGET}/release"`
  and `SRC_DIR="${CARGO_TARGET_DIR:-target}/release"` — a two-line change, and backward
  compatible for any caller that leaves `CARGO_TARGET_DIR` unset (falls back to today's
  `target/` default, so this does not affect CI or any other unmigrated call site); (2)
  because this fix depends on the *variable* being visible to `bundle-sidecars.sh`'s own
  process, not just to the preceding cargo invocation, `desktop-demo-build`'s
  root-workspace build line specifically **must** use the whole-recipe `export` form of
  the injection, not a one-off `VAR=val cmd` prefix scoped to only the `cargo build`
  line — the general "either way" flexibility stated earlier in this section does not
  hold for this one recipe, precisely because a later line in the same recipe body
  (`bundle-sidecars.sh`) also needs to see the value. Acceptance test (viii) below checks
  this end to end.
- CI-only workflows and CI-only scripts (`.github/workflows/*.yml`,
  `run-desktop-release-smoke.sh:103`, `instance-env.sh:85`,
  `test-desktop-release-cache-workflow.sh:24`) — **unchanged**, since CI stays on
  Cargo's default.

**Stays unchanged:** CI workflows/scripts; `~/.rustup`/`RUSTUP_HOME`; root and Tauri
`.cargo/config.toml` (neither file is touched by this design — root's tracked
`[profile.dev]`/`[env]` settings stay exactly as committed, and `desktop/src-tauri/.cargo/`
continues not to exist); the `Justfile:865-867` `clean` pairing (still two `cargo
clean` calls, only the resolved dir each targets changes); Hermit's `CARGO_HOME`
behavior (deferred, Section 3); `scripts/instance-env.sh:85`'s dev-icon cache — flag for
Devin before folding it in.

**Cleanup recipe — new `scripts/zs/cargo-target-gc.sh`, wired as `just cargo-target-gc`:**
0. Also enumerate the pre-existing `~/.cache/zs/buzz-targets/<worktree-basename>/`
   cache (Section 1) alongside the new `~/.cache/zs/buzz-cargo-targets/<hash>/` root, so
   it is governed by the same skip/eviction rules below rather than left to grow
   ungoverned forever. Basename-keyed entries whose worktree no longer exists by that
   basename are eviction candidates under the same age/size rules as (iv); an entry
   whose basename still matches a live, non-skipped worktree is treated like any other
   candidate, not auto-preferred or auto-excluded.
0.5. **Second-pass addition — a reverse pass, run before or alongside step 1, to close
   a design gap the forward-only enumeration below cannot reach.** `git worktree remove`
   deletes both the worktree's directory and Git's own administrative entry for it, so a
   fully removed worktree never appears in `git worktree list --porcelain` again — an
   algorithm whose *only* enumeration source is that command structurally cannot detect
   this case, making step 4's "keyed entries whose worktree is gone" eviction rule dead
   code for exactly the worktrees it claims to cover. The reverse pass instead starts
   from the **cache directories themselves**: for every entry under
   `~/.cache/zs/buzz-cargo-targets/<key>/`, read the `<key>/.worktree-path` sidecar file
   (written by `cargo-target-dir.sh`, see Section 5 above) to recover the absolute path
   that produced the hash, then check whether that exact path still appears in the
   step-1 worktree listing. A path that no longer appears — because the worktree was
   removed, or because it was moved (Section 5's cache-key correction above) and now
   builds under a new key — is an **orphaned** entry: write it to the same
   gc-manifest with `"status":"orphaned","reason":"worktree-removed-or-moved"`, and it is
   an eviction candidate under the same age/size rules as (iv), without needing the
   dirty/no-upstream checks (there is no live worktree left to query). For the legacy
   basename-keyed `~/.cache/zs/buzz-targets/<basename>/` entries (no hash, no sidecar
   needed), the equivalent check is a plain filesystem test — does
   `~/projects/buzz-wt/<basename>` still exist — independent of the git worktree list,
   so it already correctly detects a removed worktree today without further change.
1. Enumerate worktrees via `git worktree list --porcelain` from the main checkout only
   (never runs from inside a worktree) — **this is the forward pass, covering every
   currently-registered worktree; step 0.5 is the reverse pass, covering worktrees that
   are no longer registered at all.** Both passes write to the same manifest run.
2. Skip (never touch) if: (i) main checkout, (ii) `git status --porcelain=v1`
   non-empty (dirty), (iii) `git rev-parse --abbrev-ref --symbolic-full-name @{u}`
   fails (no upstream), (iv) worktree path no longer exists on disk (this remaining
   disk-level check covers a worktree whose *directory* was deleted by hand without
   `git worktree remove` — Git still lists such an entry, typically marked
   "prunable", so it is reachable by the forward pass; a worktree removed the proper
   way is only reachable via the step 0.5 reverse pass above).
3. For every worktree or cache entry NOT skipped, write one line to
   `~/.cache/zs/buzz-cargo-targets/gc-manifest-<timestamp>.jsonl` BEFORE deleting
   anything: `{worktree_path, branch, head_sha, upstream, status, cache_key,
   cache_bytes, last_mtime}` (orphaned entries from step 0.5 carry the recovered path
   and `null` for branch/head_sha/upstream, since there is no live worktree to read
   them from).
4. Eviction candidates: keyed entries whose worktree is gone (covered by step 0.5's
   reverse pass, corrected above), OR clean+idle past an age threshold (default 14 days
   no build activity) where "idle" additionally requires the worktree be **pushed**,
   defined precisely as: local HEAD has no commits ahead of its configured upstream
   (`git rev-list @{u}..HEAD` is empty) — a stricter test than skip rule (iii)'s "has an
   upstream configured at all," so the word is not redundant with that rule. Size >5G
   per entry is surfaced as a candidate, never auto-picked.
5. `--dry-run` is the default; `--apply` is required to delete. Dry-run prints
   candidates and bytes reclaimed, changes nothing.
6. `--apply` deletes only paths literally under `~/.cache/zs/buzz-cargo-targets/` (and,
   per step 0, the legacy `~/.cache/zs/buzz-targets/`) that matched a manifest row from
   the same run (no path re-derived at delete time). **Before any `rm -rf`, the script
   asserts the constructed delete path is non-empty and starts with one of those two
   literal cache-root prefixes, aborting the whole run otherwise** — an ordinary
   defensive check for any script that builds a delete path from a variable, cheap to
   add and worth having even though the design's stated intent already scopes deletion
   to these two roots. Never touches `~/.claude`, any `.git` directory, or any path
   inside a worktree — the keyed cache lives outside every worktree by design.
   `--apply` takes the same machine-wide flock (`~/.cache/zs/buzz-gate.lock`) that
   `with-gate-lock.sh` uses — this only prevents a race against another process that
   itself chose to wrap its command in `with-gate-lock.sh`. Confirmed by re-reading the
   Justfile and grepping the tree (`grep -rn with-gate-lock`): nothing in `Justfile`,
   `lefthook.yml`, or CI currently wraps any command in it — `desktop-test` is plain
   `pnpm test`, `desktop-tauri-test` is plain `cargo test --workspace`, `check-compile`
   is plain `cargo check --workspace --all-targets`, `dev` is plain cargo invocations.
   So the flock does **not**, by itself, protect an ordinary `just
   check-compile`/`just dev`/bare `cargo build` from a concurrent `--apply` — see
   Section 7 for the actual guard (the age/size heuristic) and an honest statement of
   what it does and doesn't cover.
7. `harness-fixtures` is guaranteed skipped by rule (iii) — matches the task's
   explicit "must never be cleaned" criterion.
8. **Second-pass addition — enforce the "main checkout only" convention at runtime,
   not just in documentation.** Before step 1 runs, the script asserts
   `git rev-parse --show-toplevel` (from its own invoking CWD) equals the canonical
   main-checkout path it expects; refuse and exit non-zero otherwise. `git worktree
   list --porcelain` output is CWD-independent, so this guard is not required for
   correctness of the listing itself, but it is a cheap, explicit enforcement of a rule
   that was previously stated as a convention with no check — worth having since this
   script performs deletions.

## 6. Acceptance tests

i. **Two worktrees resolve different roots for the same workspace:** `cd buzz &&
   scripts/zs/cargo-target-dir.sh root` vs. the same command from
   `buzz-wt/harness-fixtures` — paths differ and both start with
   `~/.cache/zs/buzz-cargo-targets/`. No shell activation involved; the shim is a pure
   function of `git rev-parse --show-toplevel`.
ii. **Within ONE worktree, root and Tauri resolve to different roots — the case the
   original design's own acceptance tests never covered, and where it actually failed.**
   `cd buzz-wt/harness-fixtures && scripts/zs/cargo-target-dir.sh root` vs.
   `scripts/zs/cargo-target-dir.sh desktop` from the same worktree — paths differ
   (`.../root` vs. `.../desktop`), and neither is empty or unset. This is the direct
   regression check for the CARGO_TARGET_DIR/config-precedence collision.
iii. **A migrated Justfile recipe honours the key with no shell activation:**
   `cd buzz-wt/harness-fixtures && just check-compile` (a plain, non-activated shell) —
   assert `test ! -d target` in that worktree afterward, and the root-keyed cache dir
   gained new files. Repeat for `just desktop-tauri-check` and assert the *Tauri*-keyed
   dir (not the root-keyed one) gained files, and `test ! -d desktop/src-tauri/target`.
iv. **A lefthook lane honours the key:** trigger `lefthook run pre-push` (or a real
   push) in `buzz-wt/harness-fixtures` from a shell that never sourced
   `bin/activate-hermit` — the `rust-tests` lane's `just test-unit` still resolves the
   root-keyed dir, since the export now lives in the Justfile recipe itself, not in
   activation.
v. **Dry-run touches nothing:** snapshot `find ~/.cache/zs/buzz-cargo-targets -type f |
   sort`, run `just cargo-target-gc` (no `--apply`), snapshot again — diff is empty.
vi. **Dirty or no-upstream worktree is skipped:** run `just cargo-target-gc`, grep the
   manifest for `harness-fixtures` and `agent-health` — both must show
   `"status":"skipped"` with `"reason"` equal to **either** `"no-upstream"` or `"dirty"`
   (second pass correction: both worktrees are dirty as well as no-upstream as of
   2026-09-06 re-check, so the exact reason string is state-dependent and not something
   this document can pin down; what must hold is that both are always skipped and never
   appear in eviction candidates, regardless of which reason fires first). Separately,
   `touch` an untracked file in a scratch worktree, rerun, confirm `"reason":"dirty"`.
vii. **`~/.claude` never in scope:** `grep -R '\.claude' scripts/zs/cargo-target-gc.sh
   scripts/zs/cargo-target-dir.sh` returns nothing; a dry-run's candidate list contains
   no path with `.claude`, `goal-state`, `history.jsonl`, or `/.git/`.
viii. **Second pass — `desktop-demo-build` produces a locatable app after the fix, not
   just a build that completes.** After migrating the recipe: run it once for a real
   target triple, then confirm (a) `scripts/bundle-sidecars.sh` exits 0 and reports
   sidecars bundled rather than "missing release binaries," (b) the resulting `.app`
   exists at the keyed path used to construct `APP_PATH`
   (`$(scripts/zs/cargo-target-dir.sh desktop)/$TARGET/release/bundle/macos/...`), and
   (c) `desktop/src-tauri/target/` was never created by this run. This is the direct
   regression check for the concrete break found in the second-pass review: the fix
   requires both the `bundle-sidecars.sh` code change and the whole-recipe `export` form
   of the injection, and this test fails if either half is missing.
ix. **Second pass — a removed worktree's cache is reclaimed by the reverse pass.** In a
   disposable scratch worktree (not `harness-fixtures` or `agent-health`): build once so
   a keyed cache entry and its `.worktree-path` sidecar exist, `git worktree remove` it
   (confirm it no longer appears in `git worktree list --porcelain`), then run `just
   cargo-target-gc` — the now-orphaned entry appears in the manifest with
   `"status":"orphaned"` and is an eviction candidate; a forward-pass-only
   implementation (i.e. the pre-second-pass design) would show no trace of it at all,
   which is the failure this test is written to catch.

## 7. Risk analysis and rollback

- **Missed hard-coded `target/` path, or a missed cargo call site not given the
  env-var prefix.** Mitigation: test (iii) is the regression check for the migrated
  recipes — a missed hard-code or missed prefix fails loudly (either "binary not
  found," or a plain `target/`/`desktop/src-tauri/target/` reappearing in the worktree),
  not silently. Because injection is now one line per call site rather than one shared
  activation edit, the review burden shifts to completeness of the per-recipe edit list
  in Section 5 — mitigate by grepping the final Justfile for every bare `cargo `/`pnpm
  tauri`/`cargo clean --manifest-path` occurrence before landing and confirming each one
  either carries the prefix or is deliberately CI-only/unchanged.
- **Wider edit surface than the superseded design assumed.** The original "no
  Justfile/script edits needed" claim did not hold (see "Refutations considered"); this
  version touches on the order of 10-12 Justfile recipe lines instead of one shared
  activation file. Mitigation: every edit is the same one-line, mechanical shape
  (`CARGO_TARGET_DIR="$(scripts/zs/cargo-target-dir.sh {root|desktop})" <existing
  command>`), reviewable line-by-line against the list in Section 5.
- **Raw, non-`just` cargo commands in an activated shell are out of scope by design**
  (see the named scope boundary in Section 5) — this is a documented, deliberate
  narrowing versus the superseded design's "every entry point" claim, not a silent
  gap. If growth resumes from this path specifically, it is visible as a `target/`
  reappearing under a worktree, not a repeat of the original silent-until-137 GiB
  failure mode, since every automated (`just`/lefthook) path stays covered.
- **GC deletes a cache mid-build.** The actual guard is the age/size heuristic — a
  live build keeps the cache's mtimes fresh, so only entries idle 14+ days or whose
  worktree is gone become eviction candidates. The machine-wide flock (Section 5, GC
  step 6) does **not** independently protect against this for an ordinary build:
  verified by grep that nothing in `Justfile`/`lefthook.yml`/CI wraps `desktop-test`,
  `desktop-tauri-test`, `check-compile`, or `dev` in `with-gate-lock.sh` — those run
  as plain, unwrapped commands, so a concurrent `--apply` is not serialized against
  them by any lock. State this plainly rather than relying on the flock claim: the
  age/size threshold is the real (and only) protection, and it only ever risks deleting
  cache/target output (rebuildable), never source.
- **Second pass — `bundle-sidecars.sh` ignoring `CARGO_TARGET_DIR` was a concrete break
  in `desktop-demo-build`, not a hypothetical one.** Confirmed by reading the script:
  `SRC_DIR` is a literal `target/${TARGET}/release` (or `target/release`), checked at
  line 34, with no reference to `CARGO_TARGET_DIR` anywhere in the file. Once the
  preceding cargo build in the same recipe writes to the keyed dir instead, this script
  always hits its missing-binaries branch and exits 1 before the Tauri build or the
  DMG steps ever run — silently breaking the one recipe this plan's own first pass had
  otherwise fully fixed (APP_PATH/DMG paths), one step earlier in the same recipe body.
  Mitigation: fixed directly (Section 5, migration list) by making the script resolve
  `SRC_DIR` from `${CARGO_TARGET_DIR:-target}`, and by pinning
  `desktop-demo-build`'s root-workspace build line to the whole-recipe `export` form of
  injection specifically so the variable is visible to the later `bundle-sidecars.sh`
  line in the same body. Acceptance test (viii) is the regression check. Residual risk:
  any *other*, not-yet-found script that both (a) is invoked from inside a migrated
  recipe and (b) independently re-derives a hard-coded `target/` path rather than
  taking it as an argument or reading `CARGO_TARGET_DIR`, would fail the same way —
  mitigate the same way this one was found, by grepping the final Justfile recipe
  bodies for every script invocation that follows a migrated cargo/tauri line, not just
  the migrated line itself.
- **Second pass — the GC's forward-only enumeration (`git worktree list --porcelain`)
  could never detect a `git worktree remove`d worktree's cache, reproducing the
  "ungoverned cache" failure mode this plan exists to prevent, for exactly the lifecycle
  event (worktree removal) most likely in a multi-agent worktree fleet.** `git worktree
  remove` deletes both the directory and Git's own administrative entry, so the removed
  worktree never appears in the enumeration source again — step 4's "keyed entries whose
  worktree is gone" eviction rule was unreachable for this case, not merely untested.
  Mitigation: added a reverse pass (Section 5, Cleanup recipe step 0.5) that enumerates
  the cache directories themselves and reverse-checks each hash-keyed entry's recorded
  path (via the new `.worktree-path` sidecar file, Section 5) against the live worktree
  list, and each legacy basename-keyed entry via a plain filesystem existence check.
  Acceptance test (ix) is the regression check. Residual risk: the reverse pass depends
  on the sidecar file surviving — if a keyed directory is created by some path other
  than `cargo-target-dir.sh` (should not happen under this design, since that script is
  the only writer), or the sidecar file is deleted independently of the cache directory
  it lives in, that one entry reverts to being invisible to the reverse pass the same
  way the pre-fix design was invisible to all of them; this is a narrower and more
  detectable failure mode than the one being fixed (one entry, not the whole class), but
  worth a startup self-check in the script (assert every subdirectory under the cache
  root has a `.worktree-path` sidecar, warn if not) at implementation time.
- **Second pass — the cache-key rationale in Section 5 was internally contradictory**
  ("path-based so a worktree move ... doesn't orphan the cache" — false by
  construction, since the key IS a hash of the path). Corrected in Section 5: only the
  branch-rename half of the claim was ever true; a worktree move does orphan the old
  entry, and this is accepted as a bounded cost (worktree moves are rare) rather than
  solved with a different keying scheme, since the same reverse-pass GC fix above
  reclaims a moved-away entry through the identical mechanism it uses for a removed one.
- **Rollback:** additive and reversible in one step — revert the per-recipe
  `CARGO_TARGET_DIR=` prefixes in the Justfile, revert the two-line
  `bundle-sidecars.sh` change, `rm -rf ~/.cache/zs/buzz-cargo-targets`, delete the two
  new `scripts/zs/*.sh` files. No `bin/activate-hermit` edit and no generated
  `.cargo/config.toml` exist under this design, so neither needs reverting. Every
  worktree reverts to Cargo's default isolated `target/` on the next `just` invocation.

## 8. Refutations considered

Re-checked against the live repo on 2026-09-06 (not just against the prior text of this
plan). Every item below cites what was actually read.

**Accepted — verified breaks, folded into Section 5/6/7 above:**

- **`bin/.lefthookrc` never sources `bin/activate-hermit`.** Read `bin/.lefthookrc` in
  full: it is a plain `PATH="$_lefthook_root/bin:$PATH"` prepend plus pinning
  `LEFTHOOK_BIN`, exactly as it self-documents ("the safe subset of `activate-hermit`: a
  plain PATH prepend, no interactive-shell machinery"). It never calls `hermit activate`
  and never evals its output. Confirmed the four named lanes exist as described:
  `lefthook.yml:47-48` (`rust-fmt` → `just fmt`), `:51-52` (`desktop-tauri-fmt`),
  `:94-97` (`rust-tests` → `just test-unit`), `:113-119` (`desktop-tauri-checks` → `just
  desktop-tauri-clippy && just desktop-tauri-test`). The superseded Section 5 design
  (export via `bin/activate-hermit`) would indeed have left every one of these lanes on
  Cargo's default target dir. **This was the deciding factor in moving injection from
  shell activation to the Justfile itself** (Section 5) — a fix that also closes the gap
  without needing any lefthook-specific change, since lefthook already runs `just` for
  every affected lane.
- **`desktop-demo-build` (`Justfile:299-329`) hard-codes
  `desktop/src-tauri/target/$TARGET/...` (`:323`,`:328`) and was named in Section 1's own
  fact list but dropped from Section 5's migration set.** Confirmed by reading the
  recipe: it runs a cross-compiled `pnpm tauri build --target "$TARGET" --bundles app`
  (`:317`), then locates the resulting `.app`/`.dmg` at the hard-coded default path.
  Added to the migration list in Section 5. One refinement to the original wording: under
  the *superseded* design (global env var + desktop-only generated config), the actual
  landing spot after the change would have been `<keyed>/root/$TARGET/...`, not
  `<keyed>/desktop/$TARGET/...` as originally stated — because the env-var/config
  collision (next item) means the globally-exported root env var, not the desktop
  config file, would have governed that build. The practical conclusion is unchanged
  either way: the hard-coded path breaks, and it was missing from remediation. Under
  the corrected design (explicit per-invocation env var, no global export), this recipe
  now resolves correctly to the Tauri-keyed dir, as specified above.
- **The CARGO_TARGET_DIR/config precedence collision is real and was the central
  correctness defect.** Cargo's documented precedence (env var overrides config file)
  means the superseded design's single global `CARGO_TARGET_DIR` export would have
  overridden `desktop/src-tauri/.cargo/config.toml`'s `target-dir` for every cargo call
  in that shell, collapsing both workspaces onto one directory within a worktree —
  reproducing, at worktree-internal scope, the exact hazard class (lock contention,
  cross-workspace `cargo clean`) Section 4(c) already rejects at cross-worktree scope.
  Verified empirically in a scratch repro (two nested Cargo projects, one with a
  `target-dir`-setting `.cargo/config.toml`): with `CARGO_TARGET_DIR` set in the
  environment, `cargo metadata` reports the env value regardless of the inner project's
  own config file. Confirmed also that neither of the original plan's acceptance tests
  (i, ii) would have caught this — both exercise only the root workspace, never root vs.
  Tauri within one worktree. This finding drove the full Section 5 rewrite: no shared
  env var is exported anywhere in the corrected design; each `just` recipe scopes
  `CARGO_TARGET_DIR` explicitly, per invocation, to the one workspace that recipe
  touches. New acceptance test (Section 6, test ii) checks this directly.
- **Section 7's flock claim overclaimed what `with-gate-lock.sh` covers, for ordinary
  builds.** Read `scripts/zs/with-gate-lock.sh` in full: it is a manual, explicit-opt-in
  wrapper (`scripts/zs/with-gate-lock.sh <command>`) — nothing invokes it automatically.
  `grep -rn with-gate-lock` across the tree found it referenced only in two docs plans
  and one other script's comment, never in `Justfile`, `lefthook.yml`, or any CI
  workflow. Re-read the actual recipe bodies: `desktop-test` (`Justfile:141-143`) is
  plain `pnpm test`; `desktop-tauri-test` (`:222-224`) is plain `cargo test
  --workspace`; `check-compile` (`:870-871`) is plain `cargo check --workspace
  --all-targets`; `dev` (`:600ff`) is plain cargo invocations. None takes the flock.
  Section 7's "`--apply` takes the same flock ... so it can't race a live build" is
  false as a general statement for any of these; the real (and only) protection is the
  age/size idle heuristic. Corrected in Section 7 and GC step 6 above. The two
  independent write-ups of this same finding (as its own point, and again inside "minor
  findings … CONCRETE HOLE") agree with each other and with the repo; treated as one
  finding here.
- **Worktree inventory was stale: main + 7, not main + 6, and `agent-health` is a
  second no-upstream WIP checkout the plan hadn't counted.** Re-ran `git worktree list
  --porcelain` live: 8 rows total (main + 7). `buzz-wt/agent-health`
  (`feat/agent-health`) shares HEAD `b42de461c` with `buzz-wt/harness-fixtures`
  (`feat/harness-reliability`, same SHA, different branch). Both are no-upstream by the
  existing GC skip rule (iii), so the design's live-evaluated skip logic already
  handles this correctly without any code change — only Section 1's stated fact was
  stale. Corrected there.
- **A pre-existing, ungoverned, currently-active cache exists at
  `~/.cache/zs/buzz-targets/harness-fixtures/{root,tauri}`, basename-keyed, invisible to
  the proposed GC.** Confirmed: exists, file mtimes within minutes of the check (an
  unrelated harness fixture using it as a worktree-scoped target-dir override right
  now), 3.2G at re-check (grown from the reported 2.8G — same trend, just measured
  later). `grep -rn 'buzz-targets'` across the tracked repo returns nothing — not
  produced or governed by anything in-repo. The new cache root
  (`~/.cache/zs/buzz-cargo-targets/`, hash-keyed) is a different name and keying scheme
  specifically to avoid colliding with it, and the GC recipe (Section 5, step 0) is now
  taught to also enumerate and govern the old path.
- **`bin/activate-hermit` is Hermit-generated and self-documents "THIS FILE IS
  GENERATED; DO NOT MODIFY."** Confirmed by reading its header. The superseded design's
  injection point was exactly this file. The corrected design in Section 5 does not
  edit it at all, which resolves this concern by removing the dependency rather than
  arguing the risk was acceptable.

**Confirmed correct as originally stated — re-verified, no change needed:**

- CI does not pick up any of this: `.github/workflows/_ci-rust.yml:37`, `ci.yml:38`,
  `_ci-desktop.yml:33` (and `:35,142,222`) all use `cashapp/activate-hermit@...`, not
  `source bin/activate-hermit`. Re-grepped and confirmed.
- Root `Cargo.toml:1-36` members / `:37` `exclude = ["desktop/src-tauri"]` / `:38`
  `resolver = "2"`; `desktop/src-tauri/Cargo.toml:1-6` separate `[workspace] members =
  ["crates/buzz-terminal"]`; root `.cargo/config.toml` has no `target-dir` (only
  `[profile.dev] debug` and one `CMAKE_POLICY_VERSION_MINIMUM` env var, both re-read in
  full); `desktop/src-tauri/.cargo/config.toml` does not exist (`ls` confirms). All
  re-verified byte-for-byte against the plan's claims.
- `desktop-standalone`, `staging`, `production` (`Justfile:668,708,743`) resolve their
  binary path via `cargo metadata --no-deps` run from repo root against the root
  workspace only — re-read all three bodies; confirmed none of them touch
  `desktop/src-tauri`'s Cargo project, so they are correctly out of scope for the
  collision above.
- `harness-fixtures` has no upstream, is the only pre-existing "must never auto-clean"
  worktree the original review named, and the design's rule (iii) already handles it
  live. `git worktree list --porcelain` and `git rev-parse --abbrev-ref
  --symbolic-full-name @{u}` re-run and confirmed.

**No refutation in the reviewed set was found to be wrong.** One item (the
`desktop-demo-build` landing-path detail, above) is accepted with a refinement to its
stated mechanism, not a rejection of its conclusion.

**Found during this revision, not in the reviewed set — disclosed for completeness:**
building a corrected mechanism surfaced that Cargo's `.cargo/config.toml` discovery
walks up from the invoking process's **current working directory**, not from
`--manifest-path`'s directory (confirmed empirically: a `--manifest-path`-only
invocation from an unrelated CWD ignores the target project's own config file; the
identical command run with matching CWD honors it). This meant a config-file-only
replacement would have silently failed for `desktop-tauri-clippy`/`desktop-tauri-check`
(`Justfile:214-219`), which invoke cargo via `--manifest-path` from repo-root CWD. It is
why Section 5's replacement uses explicit per-invocation `CARGO_TARGET_DIR=` scoping
(confirmed CWD/manifest-path-independent by the same repro) instead of generating a
second `.cargo/config.toml`.

## 9. Refutations considered — second pass (2026-09-06)

Re-checked against the live repo, not just against the prior text of this plan. Every
item cites what was actually read.

**Accepted — verified breaks or defects, folded into Sections 1/5/6/7 above:**

- **`scripts/bundle-sidecars.sh:22` (and `:20`) hard-codes `SRC_DIR="target/${TARGET}/release"`
  / `"target/release"`, breaking `desktop-demo-build` once its preceding cargo build is
  migrated.** Read the full script (54 lines): `SRC_DIR` is set from a literal `target/`
  string with no reference to `CARGO_TARGET_DIR` or `cargo metadata` anywhere in the
  file; line 34 checks for each sidecar binary under that path; line 37 exits 1 with
  `Error: missing release binaries in $SRC_DIR` if any are absent. Confirmed the file
  appears exactly once in Section 1's fact list (`grep -n bundle-sidecars` on the plan
  before this revision returned exactly one line) and zero times in Sections 5-8 before
  this revision — a genuine drop, not a misreading. This holds regardless of whether the
  preceding `CARGO_TARGET_DIR` export is a one-off prefix or a whole-recipe export,
  since the script never reads the variable either way. Fixed in Section 5 (two-line
  script change plus pinning the recipe to the whole-recipe export form) and Section 7;
  regression test added as (viii).
- **The GC recipe cannot fulfill its own eviction criterion for a fully
  `git worktree remove`d worktree, because its only enumeration source is `git worktree
  list --porcelain`, which no longer lists that worktree once it is removed.** This is a
  design-level fact about documented Git behavior (`git worktree remove` deletes the
  working directory and prunes the administrative entry), not something requiring a live
  repro — and the task's own preservation boundary (no removing worktrees) rules out
  testing it empirically here, which is consistent with relying on documented behavior
  rather than an experiment. The refutation is logically sound: an algorithm that only
  ever asks "for each of these registered worktrees, is it gone" cannot detect the case
  where a worktree is no longer registered at all. Fixed in Section 5 (reverse-pass step
  0.5, reading a new `.worktree-path` sidecar file per keyed cache entry) and Section 7;
  regression test added as (ix).
- **The cache-key rationale ("path-based so a worktree move or branch rename doesn't
  orphan the cache") was self-contradictory.** Re-read the cited text verbatim in the
  plan before this revision: the key is defined as the first 12 hex chars of
  `sha256(<absolute worktree path>)`, and the very next sentence claims a worktree move
  doesn't orphan the cache — but moving a worktree changes its absolute path, which by
  the stated key definition necessarily changes the key and orphans the old entry. Only
  the branch-rename half was ever true (renaming a branch does not change the worktree's
  path). Corrected in Section 5, with the residual cost accepted rather than solved by a
  different keying scheme, since the reverse-pass fix above reclaims a moved-away entry
  through the same mechanism as a removed one.
- **Plan facts had gone stale between the first-pass revision and this review, in a way
  that itself illustrates the concurrency risk the plan is meant to address.** Live
  re-check (2026-09-06, later than the first pass's own re-check) shows both no-upstream
  worktrees dirty: `harness-fixtures` (`crates/buzz-acp/src/lib.rs`, `queue.rs`,
  `reliability/ledger.rs`, `reliability/park.rs`, `reliability/runtime.rs` modified) and
  `agent-health` (`crates/buzz-acp/src/reliability/ledger.rs` modified, HEAD advanced to
  `deff99c19` from the `b42de461c` this plan originally recorded as shared with
  `harness-fixtures`). A build-cache fingerprint file under
  `~/.cache/zs/buzz-targets/agent-health/root/debug/.fingerprint/` had an mtime seconds
  old at check time, i.e. an agent was actively building there during this review.
  Functionally this does not break the design — GC re-queries `git status`/upstream live
  at run time (Section 5 steps 1-2), not from this document, and dirty-or-no-upstream
  both correctly land on "skip" either way — but it does mean acceptance test (vi), as
  originally worded, would have asserted a specific skip reason (`no-upstream`) that no
  longer matches today's live state (`dirty` also applies now). Corrected Section 1's
  "confirmed facts" and loosened test (vi) to accept either valid skip reason.
- **Completeness gap: the pre-existing ungoverned `~/.cache/zs/buzz-targets/` cache has
  a second live consumer, `agent-health`, that the plan's original fact-gathering
  missed.** Confirmed: `~/.cache/zs/buzz-targets/agent-health/{root,tauri}` exists,
  2.1G at re-check, file mtimes within minutes of the check (matches the "actively
  building" evidence above). Only `harness-fixtures` was named in the plan before this
  revision. This does not break the GC mechanism — Section 5 step 0's language already
  covers any basename generically — but it did undercount the scale of the pre-existing
  workaround by roughly half. Corrected in Section 1.

**Confirmed correct as originally stated — re-verified live, no change needed:**

- `git worktree list --porcelain` shows main + 7 (`agent-health`, `assets-facets`,
  `calendar-replay-race`, `harness-fixtures`, `mcp-registry-ui`, `remote-box-account`,
  `wave4-minutes`); `agent-health` and `harness-fixtures` both genuinely have no
  upstream (`git rev-parse --abbrev-ref --symbolic-full-name @{u}` fails for both); all
  8 resolve `git rev-parse --git-common-dir` to the same
  `/Users/zero-suminc./projects/buzz/.git`. Re-run directly against the live repo for
  this revision, not assumed from the prior text.
- `scripts/zs/with-gate-lock.sh` is a manual, explicit-opt-in wrapper never invoked
  automatically: `grep -rn with-gate-lock Justfile lefthook.yml .github/workflows/`
  returns nothing. Matches the plan's Section 7 claim that the flock does not protect an
  ordinary `just check-compile`/`dev`/`desktop-test` build from a concurrent `--apply`.
- Cargo workspace boundary facts re-read byte-for-byte: root `Cargo.toml` `exclude =
  ["desktop/src-tauri"]` / `resolver = "2"`; `desktop/src-tauri/Cargo.toml` declares its
  own `[workspace] members = ["crates/buzz-terminal"]`; root `.cargo/config.toml` has no
  `target-dir` (only `[profile.dev] debug` and one `[env]` var); `desktop/src-tauri/.cargo/config.toml`
  does not exist.
- `scripts/mobile-worktree-clean.sh` matches only worktree-suffixed bundle ids
  (`xyz.block.buzz.dogfood.mobile.<suffix>`, `xyz.block.buzz.mobile.<suffix>`) and never
  touches the two unsuffixed production bundle ids — re-read the script header and
  matching logic; matches the plan's characterization of it as prior art.
- No mechanism in the GC design (Section 5) references or constructs a path under
  `~/.claude` — the cache root lives entirely under `~/.cache/zs/`, a separate tree.
- No new concrete case, beyond what Section 7 already discloses, was found where the
  specified recipe would delete something outside rebuildable cache, or touch a worktree
  failing the dirty/no-upstream skip checks. The one real residual risk this review adds
  (the reverse-pass sidecar file itself being deleted independently of its cache
  directory) is disclosed above in Section 7, and even in that worst case it only leaves
  one entry ungoverned again, never deletes source, `.git`, or `~/.claude`.

**Rejected: none.** Every refutation reviewed this pass held up against the live repo;
none was found to overstate, misread, or fabricate a defect.

**Minor items folded in where cheap, per the reviewer's own framing as non-blocking:**

- Added a defensive check before `rm -rf` in the GC recipe (Section 5, step 6): assert
  the constructed delete path is non-empty and literally prefixed by one of the two
  governed cache roots, abort otherwise.
- Pinned down eviction rule 4's "pushed": local HEAD has no commits ahead of its
  upstream (`git rev-list @{u}..HEAD` empty), distinct from skip rule (iii)'s "has an
  upstream configured at all" (Section 5, step 4).
- Added a runtime guard enforcing "run from the main checkout only" (Section 5, GC step
  8), rather than leaving it a documentation-only convention.
