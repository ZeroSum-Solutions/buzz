#!/usr/bin/env bash
# T8 (spike/pdf) scripted validation: renders the fixture through
# headless_chrome (route b) in online and offline modes, then checks each
# output with pdftotext / pdftoppm and records size, wall time, and hashes.
# Throwaway spike script — not production code.
set -euo pipefail

cd "$(dirname "$0")"
BIN=./target/release/render_headless_chrome
FIXTURE=fixtures/approval.html
OUT=out
mkdir -p "$OUT"

if [ ! -x "$BIN" ]; then
  echo "building release binary..."
  cargo build --release
fi

fail=0
# check DESC MIN_COUNT ACTUAL_COUNT — pass when actual >= min.
check() {
  local desc="$1"; local min="$2"; local actual="$3"
  if [ "$actual" -ge "$min" ]; then
    echo "  PASS: $desc (got $actual)"
  else
    echo "  FAIL: $desc (got $actual, want >= $min)"
    fail=1
  fi
}
# check_eq DESC EXPECTED ACTUAL — pass when actual == expected.
check_eq() {
  local desc="$1"; local expected="$2"; local actual="$3"
  if [ "$actual" = "$expected" ]; then
    echo "  PASS: $desc (got $actual)"
  else
    echo "  FAIL: $desc (got $actual, want $expected)"
    fail=1
  fi
}

run_mode() {
  local mode="$1"; shift
  local pdf="$OUT/approval-headless-chrome-$mode.pdf"
  local pngdir="$OUT/png-$mode"
  rm -rf "$pngdir"; mkdir -p "$pngdir"

  echo "== mode: $mode =="
  local json
  json=$("$BIN" "$FIXTURE" "$pdf" "$@")
  echo "  render: $json"

  local pages
  pages=$(pdfinfo "$pdf" | awk '/^Pages:/{print $2}')
  check_eq "page count == 3" 3 "$pages"

  local text
  text=$(pdftotext -layout "$pdf" -)
  check "heading 1 (Approval Page One) present" 1 "$(echo "$text" | grep -c "Approval Page One")"
  check "heading 2 (Materials Table) present" 1 "$(echo "$text" | grep -c "Materials Table")"
  check "heading 3 (Approval ID Generator) present" 1 "$(echo "$text" | grep -c "Approval ID Generator")"
  check "table cell token (alpha-fixture-row) present" 1 "$(echo "$text" | grep -c "alpha-fixture-row")"
  check "table cell token (delta-fixture-row) present" 1 "$(echo "$text" | grep -c "delta-fixture-row")"
  check "code marker line present" 1 "$(echo "$text" | grep -c "PDF_SPIKE_CODE_MARKER_7f3a")"

  pdftoppm -png -r 100 "$pdf" "$pngdir/page"
  local pngcount
  pngcount=$(ls "$pngdir"/page-*.png 2>/dev/null | wc -l | tr -d ' ')
  check_eq "pdftoppm produced 3 PNGs without error" 3 "$pngcount"

  local size
  size=$(stat -f%z "$pdf" 2>/dev/null || stat -c%s "$pdf")
  local sha
  sha=$(shasum -a 256 "$pdf" | awk '{print $1}')
  echo "  pdf: bytes=$size sha256=$sha"
  for p in "$pngdir"/page-*.png; do
    echo "  png: $p sha256=$(shasum -a 256 "$p" | awk '{print $1}') bytes=$(stat -f%z "$p" 2>/dev/null || stat -c%s "$p")"
  done
}

run_mode online
run_mode offline --offline

echo "fixture sha256=$(shasum -a 256 "$FIXTURE" | awk '{print $1}')"

if [ "$fail" -ne 0 ]; then
  echo "VALIDATION FAILED"
  exit 1
fi
echo "VALIDATION PASSED"
