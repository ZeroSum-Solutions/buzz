#!/usr/bin/env bash
# T8 (spike/pdf) orchestrator: builds the harness and sentinel, renders the
# fixture through headless_chrome in online mode (live remote image) and
# offline mode (remote image pointed at the local sentinel, which logs and
# refuses every request), then delegates per-PDF structural checks to
# scripts/zs/pdf-validate.sh <pdf> — the validator contract named in the
# T8 ticket text. Throwaway spike script — not production code.
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

run_online() {
  local pdf="$OUT/approval-headless-chrome-online.pdf"
  local pngdir="$OUT/png-online"
  rm -rf "$pngdir"; mkdir -p "$pngdir"
  echo "== mode: online =="
  local json
  json=$("$BIN" fixtures/approval.html "$pdf")
  echo "  render: $json"
  "$VALIDATE_PDF" "$pdf" || fail=1
  pdftoppm -png -r 100 "$pdf" "$pngdir/page"
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
  local sentinel_pid=$!
  trap 'kill "$sentinel_pid" 2>/dev/null || true' RETURN

  local waited=0
  until grep -q "sentinel listening" "$OUT/sentinel-stdout.log" 2>/dev/null; do
    sleep 0.1
    waited=$((waited + 1))
    if [ "$waited" -ge 50 ]; then
      echo "  FAIL: sentinel did not report ready within 5s"
      fail=1
      kill "$sentinel_pid" 2>/dev/null || true
      return
    fi
  done

  local json
  json=$("$BIN" fixtures/approval-offline.html "$pdf")
  echo "  render: $json"

  kill "$sentinel_pid" 2>/dev/null || true
  wait "$sentinel_pid" 2>/dev/null || true

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

  "$VALIDATE_PDF" "$pdf" || fail=1
  pdftoppm -png -r 100 "$pdf" "$pngdir/page"
  for p in "$pngdir"/page-*.png; do
    echo "  png: $p sha256=$(shasum -a 256 "$p" | awk '{print $1}') bytes=$(stat -f%z "$p" 2>/dev/null || stat -c%s "$p")"
  done
}

run_online
run_offline

echo "fixture sha256 (online)=$(shasum -a 256 fixtures/approval.html | awk '{print $1}')"
echo "fixture sha256 (offline)=$(shasum -a 256 fixtures/approval-offline.html | awk '{print $1}')"

if [ "$fail" -ne 0 ]; then
  echo "VALIDATION FAILED"
  exit 1
fi
echo "VALIDATION PASSED"
