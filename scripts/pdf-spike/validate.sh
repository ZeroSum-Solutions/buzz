#!/usr/bin/env bash
# T8 (spike/pdf) orchestrator: builds the harness and sentinel, renders the
# fixture through headless_chrome in online mode (local hash-pinned image)
# and offline mode (image pointed at the local sentinel, which logs and
# refuses every request), then delegates per-PDF structural checks to
# scripts/zs/pdf-validate.sh <pdf> — the validator contract named in the
# T8 ticket text. Also asserts the renderer's own `image_state` field
# (online must be "loaded", offline must not be) — a measured event-based
# signal, not an inferred one — that pdftoppm actually succeeds, and (via
# run_negative) that the validator's row/code checks are falsifiable: a
# page-count-preserving fixture with those tokens removed must fail them.
# Throwaway spike script — not production code.
set -euo pipefail

cd "$(dirname "$0")"
REPO_ROOT="$(cd ../.. && pwd)"
VALIDATE_PDF="$REPO_ROOT/scripts/zs/pdf-validate.sh"
BIN=./target/release/render_headless_chrome
SENTINEL_BIN=./target/release/sentinel
OUT=out
SENTINEL_PORT=18391
mkdir -p "$OUT"

# Always rebuild: an executability check (`[ -x "$BIN" ]`) tests only that a
# binary exists, not that it reflects the current source -- editing
# src/*.rs and re-running ./validate.sh alone would otherwise validate a
# stale executable while still printing VALIDATION PASSED. --locked keeps
# this reproducible against the committed Cargo.lock.
echo "building release binaries..."
cargo build --release --locked

fail=0

# Script-scope sentinel pid, guarded by a script-scope trap rather than a
# function-local RETURN trap. A RETURN trap only fires when the function it
# is set in returns normally; under `set -e`, an unguarded failing command
# (e.g. the renderer invocation below) unwinds the whole shell via the ERR
# path instead, so a RETURN trap never runs and the sentinel is left
# listening on $SENTINEL_PORT, breaking the next run's bind. EXIT/INT/TERM
# fire on every exit path, including a `set -e` abort.
sentinel_pid=""
cleanup_sentinel() {
  if [ -n "$sentinel_pid" ]; then
    kill "$sentinel_pid" 2>/dev/null || true
    wait "$sentinel_pid" 2>/dev/null || true
    sentinel_pid=""
  fi
}
trap cleanup_sentinel EXIT INT TERM

# 8 random bytes as hex, baked into each offline run's sentinel URL so a
# "the log is non-empty" or "the log mentions this path" check can't be
# satisfied by a stray connection from something else on the machine during
# the run's window (Sol audit finding 2, probe C: a bare TCP connect with no
# HTTP request at all produced a log line that the old `[ -s "$sentinel_log" ]`
# check accepted as proof).
gen_nonce() {
  od -An -tx1 -N8 /dev/urandom | tr -d ' \n'
}

# Writes a copy of an offline-mode fixture template with this run's nonce
# baked into the sentinel <img> URL. Only the query string changes -- every
# other byte of the template is preserved.
render_offline_fixture() {
  local template="$1" out_html="$2" nonce="$3"
  sed "s#/remote-image.png\"#/remote-image.png?nonce=${nonce}\"#" "$template" > "$out_html"
}

# Falsifiability control for the "offline restricts one URL, not egress"
# finding (Sol audit finding 6, kept in its narrowed form): as of this
# commit `grep`ing both fixtures for any external-reference pattern
# (http(s) URL, @font-face, <link>, <script>, CSS url()) turns up exactly
# the one sentinel <img> in the offline fixture and nothing in the others --
# nothing enforces that staying true, so a later fixture edit could add an
# external resource and silently stop being offline. Enforced here rather
# than only asserted in the memo.
check_external_reference_count() {
  local fixture="$1" want="$2"
  local got
  got=$(grep -cE 'https?://|@font-face|<link|<script|url\(' "$fixture" || true)
  if [ "$got" -eq "$want" ]; then
    echo "  PASS: $fixture has exactly $want external-reference pattern hit(s)"
  else
    echo "  FAIL: $fixture has $got external-reference pattern hit(s), want exactly $want -- offline mode only intercepts the sentinel <img>; any other external reference would leak silently past this harness"
    fail=1
  fi
}

run_online() {
  local pdf="$OUT/approval-headless-chrome-online.pdf"
  local pngdir="$OUT/png-online"
  rm -rf "$pngdir"; mkdir -p "$pngdir"
  echo "== mode: online =="
  local json
  if ! json=$("$BIN" fixtures/approval.html "$pdf"); then
    echo "  FAIL: renderer failed in online mode"
    fail=1
    return
  fi
  echo "  render: $json"
  local image_state
  image_state=$(echo "$json" | grep -o '"image_state":"[a-z]*"' | cut -d'"' -f4)
  if [ "$image_state" = "loaded" ]; then
    echo "  PASS: online render image_state=loaded (local hash-pinned <img> confirmed via its load event, not inferred from a sleep)"
  else
    echo "  FAIL: online render image_state=$image_state (expected loaded) — the local hash-pinned reference image did not load within the 5s in-page timeout; the memo's size-drop claim would be unsupported"
    fail=1
  fi
  "$VALIDATE_PDF" "$pdf" || fail=1
  if ! pdftoppm -png -r 100 "$pdf" "$pngdir/page"; then
    echo "  FAIL: pdftoppm exited non-zero rendering $pdf"
    fail=1
  fi
  for p in "$pngdir"/page-*.png; do
    echo "  png: $p sha256=$(shasum -a 256 "$p" | awk '{print $1}') bytes=$(stat -f%z "$p" 2>/dev/null || stat -c%s "$p")"
  done
  # Cheap per-mode floor (Sol audit finding 3's last suggestion): online must
  # embed the real reference image, which alone accounts for most of the
  # size gap to offline (~491KB vs ~108KB measured). A stripped/hidden image
  # (confirmed: `display:none` on the same fixture drops this to 105,374
  # bytes) trips this floor even before the image-binding check in
  # scripts/zs/pdf-validate.sh runs.
  local pdf_bytes
  pdf_bytes=$(stat -f%z "$pdf" 2>/dev/null || stat -c%s "$pdf")
  if [ "$pdf_bytes" -ge 300000 ]; then
    echo "  PASS: online PDF size >= 300000 bytes (got $pdf_bytes; reference image embedded)"
  else
    echo "  FAIL: online PDF size $pdf_bytes < 300000 bytes — the reference image may not be embedded"
    fail=1
  fi
}

run_offline() {
  local pdf="$OUT/approval-headless-chrome-offline.pdf"
  local pngdir="$OUT/png-offline"
  local sentinel_log="$OUT/sentinel.log"
  local fixture_html="$OUT/approval-offline-run.html"
  local nonce
  nonce=$(gen_nonce)
  rm -rf "$pngdir"; mkdir -p "$pngdir"
  rm -f "$sentinel_log"
  render_offline_fixture fixtures/approval-offline.html "$fixture_html" "$nonce"
  echo "== mode: offline (nonce=$nonce) =="

  "$SENTINEL_BIN" "$SENTINEL_PORT" "$sentinel_log" > "$OUT/sentinel-stdout.log" 2>&1 &
  sentinel_pid=$!

  local waited=0
  until grep -q "sentinel listening" "$OUT/sentinel-stdout.log" 2>/dev/null; do
    sleep 0.1
    waited=$((waited + 1))
    if [ "$waited" -ge 50 ]; then
      echo "  FAIL: sentinel did not report ready within 5s"
      fail=1
      cleanup_sentinel
      return
    fi
  done

  local json
  if ! json=$("$BIN" "$fixture_html" "$pdf"); then
    echo "  FAIL: renderer failed in offline mode"
    fail=1
    cleanup_sentinel
    return
  fi
  echo "  render: $json"
  local image_state
  image_state=$(echo "$json" | grep -o '"image_state":"[a-z]*"' | cut -d'"' -f4)

  cleanup_sentinel

  echo "  sentinel log:"
  sed 's/^/    /' "$sentinel_log" 2>/dev/null || true

  # Requires a well-formed GET line carrying THIS run's nonce -- not merely
  # a non-empty log file. sentinel.rs only ever writes a "refused ...
  # request=GET ..." line for a syntactically valid GET it actually parsed;
  # a bare TCP connect that sends no bytes logs "empty-request" instead (Sol
  # audit finding 2, probe C), and a stray connection from something else on
  # the machine can't carry this run's nonce even if it did send a GET.
  local expected_request="request=GET /remote-image.png?nonce=${nonce} HTTP/"
  if grep -qF "$expected_request" "$sentinel_log" 2>/dev/null; then
    echo "  PASS: sentinel log has a well-formed GET carrying this run's nonce (direct proof the renderer issued the request)"
  else
    echo "  FAIL: sentinel log has no well-formed, nonce-bearing GET for this run (nonce=$nonce) — not direct proof the renderer issued the request"
    fail=1
  fi

  # Requires the in-page load/error promise to have settled to exactly
  # "failed". "missing" (the <img class="remote"> selector found nothing)
  # and "timeout" (the promise never settled) are instrumentation failures,
  # not evidence the image failed to load offline -- see
  # run_offline_negative_control below, which proves this distinction is
  # falsifiable.
  if [ "$image_state" = "failed" ]; then
    echo "  PASS: offline render image_state=failed (the in-page load/error promise measured a genuine failure)"
  else
    echo "  FAIL: offline render image_state=$image_state, want exactly 'failed' — 'missing'/'timeout' mean the instrumentation measured nothing, not that the image failed to load offline"
    fail=1
  fi

  "$VALIDATE_PDF" "$pdf" || fail=1
  if ! pdftoppm -png -r 100 "$pdf" "$pngdir/page"; then
    echo "  FAIL: pdftoppm exited non-zero rendering $pdf"
    fail=1
  fi
  for p in "$pngdir"/page-*.png; do
    echo "  png: $p sha256=$(shasum -a 256 "$p" | awk '{print $1}') bytes=$(stat -f%z "$p" 2>/dev/null || stat -c%s "$p")"
  done
  local pdf_bytes
  pdf_bytes=$(stat -f%z "$pdf" 2>/dev/null || stat -c%s "$pdf")
  if [ "$pdf_bytes" -lt 300000 ]; then
    echo "  PASS: offline PDF size < 300000 bytes (got $pdf_bytes; reference image not embedded)"
  else
    echo "  FAIL: offline PDF size $pdf_bytes >= 300000 bytes — the reference image may have leaked into an offline render"
    fail=1
  fi
}

# Falsifiability control for the tightened offline checks above (Sol audit
# finding 2, probe A): fixtures/approval-offline-noinstrument.html is
# byte-for-byte fixtures/approval-offline.html with only `class="remote"`
# removed from the <img> -- its `src` is untouched, so the browser still
# issues the request (the sentinel still sees a well-formed, nonce-bearing
# GET) but `wait_for_remote_image`'s `document.querySelector('img.remote')`
# can no longer find the element, so `image_state` comes back "missing"
# instead of "failed". The tightened `image_state == "failed"` gate in
# run_offline above must FAIL against this fixture -- if it doesn't, that
# gate isn't actually measuring anything, exactly the failure mode the
# finding reproduced against the old `!= "loaded"` check.
run_offline_negative_control() {
  local pdf="$OUT/approval-offline-noinstrument.pdf"
  local sentinel_log="$OUT/sentinel-noinstrument.log"
  local fixture_html="$OUT/approval-offline-noinstrument-run.html"
  local nonce
  nonce=$(gen_nonce)
  rm -f "$sentinel_log" "$pdf"
  render_offline_fixture fixtures/approval-offline-noinstrument.html "$fixture_html" "$nonce"
  echo "== mode: offline negative control (img.remote class removed, src untouched; nonce=$nonce) =="

  "$SENTINEL_BIN" "$SENTINEL_PORT" "$sentinel_log" > "$OUT/sentinel-noinstrument-stdout.log" 2>&1 &
  sentinel_pid=$!

  local waited=0
  until grep -q "sentinel listening" "$OUT/sentinel-noinstrument-stdout.log" 2>/dev/null; do
    sleep 0.1
    waited=$((waited + 1))
    if [ "$waited" -ge 50 ]; then
      echo "  FAIL: sentinel did not report ready within 5s (negative control)"
      fail=1
      cleanup_sentinel
      return
    fi
  done

  local json
  if ! json=$("$BIN" "$fixture_html" "$pdf"); then
    echo "  FAIL: renderer failed on the offline negative-control fixture"
    fail=1
    cleanup_sentinel
    return
  fi
  echo "  render: $json"
  local image_state
  image_state=$(echo "$json" | grep -o '"image_state":"[a-z]*"' | cut -d'"' -f4)
  cleanup_sentinel

  local expected_request="request=GET /remote-image.png?nonce=${nonce} HTTP/"
  if grep -qF "$expected_request" "$sentinel_log" 2>/dev/null; then
    echo "  (expected) sentinel log shows the fetch was still attempted — this control isolates the image_state gate, not the sentinel-log gate"
  else
    echo "  FAIL: negative-control fixture never reached the sentinel — the control itself is broken, not proving anything"
    fail=1
    return
  fi

  if [ "$image_state" = "failed" ]; then
    echo "  FAIL: negative control got image_state=failed, expected 'missing' (querySelector('img.remote') should find nothing once the class is removed) — the control no longer isolates what it claims to"
    fail=1
  else
    echo "  PASS: negative control got image_state=$image_state (not 'failed') — confirms the tightened image_state=='failed' gate in run_offline would correctly FAIL this broken-instrumentation case"
  fi
}

# Falsifiability control for scripts/zs/pdf-validate.sh (AGENTS.md
# Review-Proven Rule 3): fixtures/approval-negative.html is byte-for-byte
# fixtures/approval.html with the four table-row tokens and the code-block
# marker line replaced by unrelated text of near-identical length — nothing
# else changed, so the page count is preserved. If the validator still
# PASSes the row or code-marker checks against this fixture, those checks
# are not actually bound to the structures they claim to prove.
run_negative() {
  local pdf="$OUT/approval-negative.pdf"
  echo "== mode: negative fixture (row tokens + code marker removed, page count preserved) =="
  local json
  if ! json=$("$BIN" fixtures/approval-negative.html "$pdf"); then
    echo "  FAIL: renderer failed on the negative fixture"
    fail=1
    return
  fi
  echo "  render: $json"

  local pages
  pages=$(pdfinfo "$pdf" | awk '/^Pages:/{print $2}')
  if [ "$pages" != "3" ]; then
    echo "  FAIL: negative fixture renders to $pages page(s), want 3 — not a valid page-count-preserving control (rebalance its filler text)"
    fail=1
    return
  fi
  echo "  PASS: negative fixture still renders to 3 pages"

  local validate_out validate_exit
  validate_out=$("$VALIDATE_PDF" "$pdf" 2>&1) && validate_exit=0 || validate_exit=$?
  echo "$validate_out" | sed 's/^/    /'

  if [ "$validate_exit" -eq 0 ]; then
    echo "  FAIL: scripts/zs/pdf-validate.sh PASSED against the negative fixture — the row/code checks did not catch the structures being removed"
    fail=1
  else
    echo "  PASS: scripts/zs/pdf-validate.sh FAILED against the negative fixture (nonzero exit), as required"
  fi

  local removed_checks=(
    "table cell token (alpha-fixture-row) present"
    "table cell token (bravo-fixture-row) present"
    "table cell token (charlie-fixture-row) present"
    "table cell token (delta-fixture-row) present"
    "code marker line (full text) present"
  )
  for desc in "${removed_checks[@]}"; do
    if echo "$validate_out" | grep -q "FAIL: $desc"; then
      echo "  PASS: '$desc' correctly reports FAIL when its structure is removed"
    else
      echo "  FAIL: '$desc' did not report FAIL against the negative fixture — this check is not falsifiable"
      fail=1
    fi
  done

  local kept_checks=(
    "page count == 3"
    "heading 1 (Approval Page One) present"
    "heading 2 (Materials Table) present"
    "heading 3 (Approval ID Generator) present"
  )
  for desc in "${kept_checks[@]}"; do
    if echo "$validate_out" | grep -q "PASS: $desc"; then
      echo "  PASS: '$desc' still reports PASS (structure was not touched by the negative fixture)"
    else
      echo "  FAIL: '$desc' unexpectedly failed on the negative fixture — the control changed more than intended"
      fail=1
    fi
  done
}

echo "== egress surface check (Sol audit finding 6) =="
check_external_reference_count fixtures/approval.html 0
check_external_reference_count fixtures/approval-offline.html 1
check_external_reference_count fixtures/approval-negative.html 0
check_external_reference_count fixtures/approval-offline-noinstrument.html 1

run_online
run_offline
run_offline_negative_control
run_negative

echo "fixture sha256 (online)=$(shasum -a 256 fixtures/approval.html | awk '{print $1}')"
echo "fixture sha256 (offline)=$(shasum -a 256 fixtures/approval-offline.html | awk '{print $1}')"
echo "fixture sha256 (offline-noinstrument)=$(shasum -a 256 fixtures/approval-offline-noinstrument.html | awk '{print $1}')"
echo "fixture sha256 (negative)=$(shasum -a 256 fixtures/approval-negative.html | awk '{print $1}')"

if [ "$fail" -ne 0 ]; then
  echo "VALIDATION FAILED"
  exit 1
fi
echo "VALIDATION PASSED"
