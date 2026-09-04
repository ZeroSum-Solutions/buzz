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
# All four row tokens, not just the first and last — a table with only its
# middle rows dropped would otherwise still pass.
check "table cell token (alpha-fixture-row) present" 1 "$(echo "$text" | grep -c "alpha-fixture-row")"
check "table cell token (bravo-fixture-row) present" 1 "$(echo "$text" | grep -c "bravo-fixture-row")"
check "table cell token (charlie-fixture-row) present" 1 "$(echo "$text" | grep -c "charlie-fixture-row")"
check "table cell token (delta-fixture-row) present" 1 "$(echo "$text" | grep -c "delta-fixture-row")"
# The full comment line, not the bare marker token, so a comment fragment
# elsewhere in the document can't accidentally satisfy this check.
check "code marker line (full text) present" 1 "$(echo "$text" | grep -c "# PDF_SPIKE_CODE_MARKER_7f3a")"

pngdir=$(mktemp -d)
trap 'rm -rf "$pngdir"' EXIT
pdftoppm -png -r 100 "$PDF" "$pngdir/page"
pngcount=$(ls "$pngdir"/page-*.png 2>/dev/null | wc -l | tr -d ' ')
check_eq "pdftoppm produced 3 PNGs without error" 3 "$pngcount"

# Bind the remote <img> to what actually rendered, not to a JS event fired
# in the page (AGENTS.md Review-Proven Rule 3). image_state=="loaded" only
# proves the DOM fired a load event; it says nothing about whether an image
# XObject exists in the printed PDF. Reproduced: a scratch copy of the
# online fixture with only `display:none` set on the <img> still reports
# image_state=="loaded" (the browser still loads the resource even though
# it isn't laid out) while the PDF drops from ~491KB to 105,374 bytes and
# `pdfimages -list` shows zero embedded images — none of the checks above
# would have caught that. Distinguish "the real reference image is
# embedded" from "a broken-image placeholder icon is embedded" from
# "nothing is embedded" by reading the PDF's own image XObjects.
image_rows=$(pdfimages -list "$PDF" | awk 'NR>2 && $3=="image"{print $4, $5}')
if [ -z "$image_rows" ]; then
  echo "  FAIL: no embedded image XObject found in the PDF (the remote <img> did not bind to rendered output, or was stripped/hidden)"
  fail=1
else
  max_width=$(echo "$image_rows" | awk '{print $1}' | sort -rn | head -1)
  if [ "$max_width" -ge 100 ]; then
    # A real photo, not the browser's small broken-image glyph: must match
    # the known reference image's native dimensions exactly, not merely
    # "some image exists".
    match=$(echo "$image_rows" | awk '$1==800 && $2==600{print; exit}')
    if [ -n "$match" ]; then
      echo "  PASS: embedded reference image present at its native 800x600 (image rows: $(echo "$image_rows" | tr '\n' ';'))"
    else
      echo "  FAIL: embedded image XObject present but not the 800x600 reference image (image rows: $(echo "$image_rows" | tr '\n' ';'))"
      fail=1
    fi
  else
    # Small enough to be the browser's broken-image glyph, not a real photo
    # (measured: 14x16 for this Chrome build) -- this is the expected
    # offline shape, so confirm it degrades to a visible, labeled
    # placeholder rather than a blank rectangle a reviewer can't tell apart
    # from a rendering bug.
    echo "  (info) no full-size image embedded (max width ${max_width}px) -- expected for an offline placeholder, checking alt text is extracted"
    check "offline placeholder alt text (remote reference image) present in extracted text" 1 "$(echo "$text" | grep -c "remote reference image")"
  fi
fi

size=$(stat -f%z "$PDF" 2>/dev/null || stat -c%s "$PDF")
sha=$(shasum -a 256 "$PDF" | awk '{print $1}')
echo "  pdf: bytes=$size sha256=$sha"

if [ "$fail" -ne 0 ]; then
  echo "VALIDATION FAILED"
  exit 1
fi
echo "VALIDATION PASSED"
