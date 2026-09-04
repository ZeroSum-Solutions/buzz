#!/usr/bin/env bash
# T8 (spike/pdf) validator contract named in the ticket text:
# scripts/zs/pdf-validate.sh <pdf>. Checks one rendered PDF against the
# spike fixture's structural expectations (headings, table cells, code
# marker, page count, error-free PNG render) and prints size and hash.
# Throwaway spike tooling — not production code.
set -euo pipefail

if [ $# -ne 1 ]; then
  echo "usage: $0 <pdf>" >&2
  exit 2
fi
PDF="$1"
if [ ! -f "$PDF" ]; then
  echo "FAIL: no such file: $PDF" >&2
  exit 1
fi

fail=0
check() {
  local desc="$1"; local min="$2"; local actual="$3"
  if [ "$actual" -ge "$min" ]; then
    echo "  PASS: $desc (got $actual)"
  else
    echo "  FAIL: $desc (got $actual, want >= $min)"
    fail=1
  fi
}
check_eq() {
  local desc="$1"; local expected="$2"; local actual="$3"
  if [ "$actual" = "$expected" ]; then
    echo "  PASS: $desc (got $actual)"
  else
    echo "  FAIL: $desc (got $actual, want $expected)"
    fail=1
  fi
}

echo "== validating $PDF =="

pages=$(pdfinfo "$PDF" | awk '/^Pages:/{print $2}')
check_eq "page count == 3" 3 "$pages"

text=$(pdftotext -layout "$PDF" -)
check "heading 1 (Approval Page One) present" 1 "$(echo "$text" | grep -c "Approval Page One")"
check "heading 2 (Materials Table) present" 1 "$(echo "$text" | grep -c "Materials Table")"
check "heading 3 (Approval ID Generator) present" 1 "$(echo "$text" | grep -c "Approval ID Generator")"
check "table cell token (alpha-fixture-row) present" 1 "$(echo "$text" | grep -c "alpha-fixture-row")"
check "table cell token (delta-fixture-row) present" 1 "$(echo "$text" | grep -c "delta-fixture-row")"
check "code marker line present" 1 "$(echo "$text" | grep -c "PDF_SPIKE_CODE_MARKER_7f3a")"

pngdir=$(mktemp -d)
trap 'rm -rf "$pngdir"' EXIT
pdftoppm -png -r 100 "$PDF" "$pngdir/page"
pngcount=$(ls "$pngdir"/page-*.png 2>/dev/null | wc -l | tr -d ' ')
check_eq "pdftoppm produced 3 PNGs without error" 3 "$pngcount"

size=$(stat -f%z "$PDF" 2>/dev/null || stat -c%s "$PDF")
sha=$(shasum -a 256 "$PDF" | awk '{print $1}')
echo "  pdf: bytes=$size sha256=$sha"

if [ "$fail" -ne 0 ]; then
  echo "VALIDATION FAILED"
  exit 1
fi
echo "VALIDATION PASSED"
