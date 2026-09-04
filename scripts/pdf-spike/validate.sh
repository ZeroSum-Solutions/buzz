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

if [ ! -x "$BIN" ] || [ ! -x "$SENTINEL_BIN" ]; then
  echo "building release binaries..."
  cargo build --release
fi

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
}

run_offline() {
  local pdf="$OUT/approval-headless-chrome-offline.pdf"
  local pngdir="$OUT/png-offline"
  local sentinel_log="$OUT/sentinel.log"
  rm -rf "$pngdir"; mkdir -p "$pngdir"
  rm -f "$sentinel_log"
  echo "== mode: offline =="

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
  if ! json=$("$BIN" fixtures/approval-offline.html "$pdf"); then
    echo "  FAIL: renderer failed in offline mode"
    fail=1
    cleanup_sentinel
    return
  fi
  echo "  render: $json"
  local image_state
  image_state=$(echo "$json" | grep -o '"image_state":"[a-z]*"' | cut -d'"' -f4)

  cleanup_sentinel

  if [ -s "$sentinel_log" ]; then
    local hits
    hits=$(wc -l < "$sentinel_log" | tr -d ' ')
    echo "  PASS: sentinel log shows the remote fetch was attempted (got $hits line(s))"
    echo "  sentinel log:"
    sed 's/^/    /' "$sentinel_log"
  else
    echo "  FAIL: sentinel log is empty — the renderer never reached the sentinel"
    fail=1
  fi

  if [ "$image_state" = "loaded" ]; then
    echo "  FAIL: offline render image_state=loaded — the remote image loaded despite the sentinel; offline is not actually offline"
    fail=1
  else
    echo "  PASS: offline render image_state=$image_state (not loaded, consistent with the sentinel's 403 refusal)"
  fi

  "$VALIDATE_PDF" "$pdf" || fail=1
  if ! pdftoppm -png -r 100 "$pdf" "$pngdir/page"; then
    echo "  FAIL: pdftoppm exited non-zero rendering $pdf"
    fail=1
  fi
  for p in "$pngdir"/page-*.png; do
    echo "  png: $p sha256=$(shasum -a 256 "$p" | awk '{print $1}') bytes=$(stat -f%z "$p" 2>/dev/null || stat -c%s "$p")"
  done
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

run_online
run_offline
run_negative

echo "fixture sha256 (online)=$(shasum -a 256 fixtures/approval.html | awk '{print $1}')"
echo "fixture sha256 (offline)=$(shasum -a 256 fixtures/approval-offline.html | awk '{print $1}')"
echo "fixture sha256 (negative)=$(shasum -a 256 fixtures/approval-negative.html | awk '{print $1}')"

if [ "$fail" -ne 0 ]; then
  echo "VALIDATION FAILED"
  exit 1
fi
echo "VALIDATION PASSED"
