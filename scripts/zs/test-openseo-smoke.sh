#!/usr/bin/env bash
# Guard tests for scripts/zs/openseo-smoke.sh.
#
# Each case below fails if the guard it names is removed from the smoke script,
# so the preflight is a tested contract rather than a comment. None of these
# cases starts a runtime, builds anything, or reaches the network: every one
# stops inside the preflight.
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SMOKE="$HERE/openseo-smoke.sh"
STUBS=$(mktemp -d "${TMPDIR:-/tmp}/openseo-smoke-test.XXXXXX")
trap 'rm -rf "$STUBS"' EXIT

fail=0
check() {
  local desc="$1" expected="$2" actual="$3"
  if [ "$expected" = "$actual" ]; then
    echo "  PASS: $desc (exit $actual)"
  else
    echo "  FAIL: $desc (exit $actual, want $expected)"
    fail=1
  fi
}
check_says() {
  local desc="$1" needle="$2" output="$3"
  if printf '%s' "$output" | grep -qF "$needle"; then
    echo "  PASS: $desc"
  else
    echo "  FAIL: $desc — output did not mention: $needle"
    printf '%s\n' "$output" | sed 's/^/      | /'
    fail=1
  fi
}

run() { # run <PATH> <args...> -> sets OUT, RC
  local path="$1"; shift
  set +e
  OUT=$(PATH="$path" "$SMOKE" "$@" 2>&1)
  RC=$?
  set -e
}

echo "== usage guards =="
run "$PATH"
check "no argument is a usage error" 2 "$RC"
check_says "usage names the two runtimes" "<claude|codex>" "$OUT"

run "$PATH" claude codex
check "two arguments is a usage error" 2 "$RC"

run "$PATH" goose
check "an unsupported runtime is a usage error" 2 "$RC"
check_says "the message names the rejected runtime" "unknown runtime 'goose'" "$OUT"

echo "== stage guard =="
set +e
OUT=$(OPENSEO_SMOKE_STAGE=bogus "$SMOKE" claude 2>&1)
RC=$?
set -e
check "an unknown stage is a usage error" 2 "$RC"
check_says "the message names the two stages" "must be config or full" "$OUT"

echo "== missing-tool guard =="
# A PATH with the base utilities but no `claude`: the script must stop, not
# continue into a run it cannot make.
BARE_PATH="/usr/bin:/bin"
run "$BARE_PATH" claude
check "a missing runtime CLI fails" 1 "$RC"
check_says "the message says the CLI is missing" "is not on PATH" "$OUT"

echo "== login guard =="
# Stubs for everything the preflight needs, with an auth probe that fails —
# the state of a machine where the CLI is installed but not logged in.
for tool in node cargo claude-agent-acp; do
  printf '#!/bin/sh\nexit 0\n' > "$STUBS/$tool"
  chmod +x "$STUBS/$tool"
done
printf '#!/bin/sh\n# `claude auth status` exits non-zero when logged out.\nexit 1\n' > "$STUBS/claude"
chmod +x "$STUBS/claude"
run "$STUBS:$BARE_PATH" claude
check "a logged-out CLI fails" 1 "$RC"
check_says "the message says the CLI is not logged in" "is not logged in on this machine" "$OUT"
check_says "the message refuses to fake a result" "will not fake one" "$OUT"

echo "== logged-out codex fails before any spawn =="
printf '#!/bin/sh\nexit 1\n' > "$STUBS/codex"
chmod +x "$STUBS/codex"
printf '#!/bin/sh\nexit 0\n' > "$STUBS/codex-acp"
chmod +x "$STUBS/codex-acp"
run "$STUBS:$BARE_PATH" codex
check "a logged-out codex fails" 1 "$RC"

if [ "$fail" -ne 0 ]; then
  echo "FAILED"
  exit 1
fi
echo "OK"
