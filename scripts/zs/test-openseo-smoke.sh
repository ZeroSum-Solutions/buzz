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
  if printf '%s' "$output" | grep -qF -- "$needle"; then
    echo "  PASS: $desc"
  else
    echo "  FAIL: $desc — output did not mention: $needle"
    printf '%s\n' "$output" | sed 's/^/      | /'
    fail=1
  fi
}
check_lacks() {
  local desc="$1" needle="$2" output="$3"
  if printf '%s' "$output" | grep -qF -- "$needle"; then
    echo "  FAIL: $desc — output should not mention: $needle"
    printf '%s\n' "$output" | sed 's/^/      | /'
    fail=1
  else
    echo "  PASS: $desc"
  fi
}

# Every case runs against a data directory that holds no Buzz-managed npm
# prefix unless the case builds one, so a real installation on the machine
# running these tests cannot decide their outcome.
EMPTY_DATA="$STUBS/data-empty"
mkdir -p "$EMPTY_DATA"
export OPENSEO_SMOKE_DATA_DIR="$EMPTY_DATA"
unset OPENSEO_SMOKE_ACP_BIN_DIR

run() { # run <PATH> <args...> -> sets OUT, RC
  local path="$1"; shift
  set +e
  OUT=$(PATH="$path" "$SMOKE" "$@" 2>&1)
  RC=$?
  set -e
}

run_in() { # run_in <PATH> <DATA_DIR> <OVERRIDE_DIR|-> <args...> -> sets OUT, RC
  local path="$1" data="$2" override="$3"; shift 3
  set +e
  if [ "$override" = "-" ]; then
    OUT=$(PATH="$path" OPENSEO_SMOKE_DATA_DIR="$data" "$SMOKE" "$@" 2>&1)
  else
    OUT=$(PATH="$path" OPENSEO_SMOKE_DATA_DIR="$data" \
      OPENSEO_SMOKE_ACP_BIN_DIR="$override" "$SMOKE" "$@" 2>&1)
  fi
  RC=$?
  set -e
}

stub() { # stub <path> — an executable that succeeds
  mkdir -p "$(dirname "$1")"
  printf '#!/bin/sh\nexit 0\n' > "$1"
  chmod +x "$1"
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
# the state of a machine where the CLI is installed but not logged in. The
# adapters live in their own directory so a case can drop them from PATH
# without dropping the CLI.
stub "$STUBS/bin/node"
stub "$STUBS/bin/cargo"
stub "$STUBS/pathbin/claude-agent-acp"
stub "$STUBS/pathbin/codex-acp"
printf '#!/bin/sh\n# `claude auth status` exits non-zero when logged out.\nexit 1\n' > "$STUBS/bin/claude"
chmod +x "$STUBS/bin/claude"
printf '#!/bin/sh\nexit 1\n' > "$STUBS/bin/codex"
chmod +x "$STUBS/bin/codex"
CLI_PATH="$STUBS/bin:$BARE_PATH"              # CLI present, no adapter anywhere
FULL_PATH="$STUBS/bin:$STUBS/pathbin:$BARE_PATH"  # adapter on PATH too

run "$FULL_PATH" claude
check "a logged-out CLI fails" 1 "$RC"
check_says "the message says the CLI is not logged in" "is not logged in on this machine" "$OUT"
check_says "the message refuses to fake a result" "will not fake one" "$OUT"

echo "== logged-out codex fails before any spawn =="
run "$FULL_PATH" codex
check "a logged-out codex fails" 1 "$RC"

echo "== adapter discovery order =="
# The app installs its ACP adapters under its own npm prefix and consults that
# prefix before PATH (managed_node_paths.rs::buzz_managed_command_path, called
# first by discovery.rs::resolve_command). These cases pin the same order here.
# Each stops in the preflight: the CLI stub is logged out, so nothing is built
# and no runtime is spawned.
MANAGED_DATA="$STUBS/managed-data"
MANAGED_BIN="$MANAGED_DATA/Buzz/node-tools/bin"
stub "$MANAGED_BIN/claude-agent-acp"
OVERRIDE_DIR="$STUBS/override"
stub "$OVERRIDE_DIR/claude-agent-acp"
EMPTY_OVERRIDE="$STUBS/override-empty"
mkdir -p "$EMPTY_OVERRIDE"

run_in "$FULL_PATH" "$EMPTY_DATA" - claude
check "PATH still resolves the adapter when no managed prefix holds it" 1 "$RC"
check_says "the source printed is PATH" "resolved from PATH -> $STUBS/pathbin/claude-agent-acp" "$OUT"

run_in "$CLI_PATH" "$MANAGED_DATA" - claude
check "the managed npm prefix resolves an adapter that is not on PATH" 1 "$RC"
check_says "the source printed is the managed prefix" "resolved from the Buzz-managed npm prefix ($MANAGED_BIN) -> $MANAGED_BIN/claude-agent-acp" "$OUT"
check_says "the run reached the login probe, so the adapter was found" "is not logged in on this machine" "$OUT"

run_in "$FULL_PATH" "$MANAGED_DATA" - claude
check "the managed prefix wins over PATH" 1 "$RC"
check_says "the managed copy is the one chosen" "-> $MANAGED_BIN/claude-agent-acp" "$OUT"
check_lacks "the PATH copy is not chosen" "-> $STUBS/pathbin/claude-agent-acp" "$OUT"

run_in "$FULL_PATH" "$MANAGED_DATA" "$OVERRIDE_DIR" claude
check "the override wins over both the managed prefix and PATH" 1 "$RC"
check_says "the source printed is the override" "resolved from OPENSEO_SMOKE_ACP_BIN_DIR -> $OVERRIDE_DIR/claude-agent-acp" "$OUT"

run_in "$FULL_PATH" "$MANAGED_DATA" "$EMPTY_OVERRIDE" claude
check "an override that holds no adapter is a hard failure" 1 "$RC"
check_says "the message names the empty override" "holds no executable claude-agent-acp" "$OUT"
check_lacks "it does not fall back to another adapter" "resolved from" "$OUT"

run_in "$CLI_PATH" "$EMPTY_DATA" - claude
check "an adapter in none of the three places fails" 1 "$RC"
check_says "the message names the managed prefix it searched" "$EMPTY_DATA/Buzz/node-tools/bin (the Buzz-managed npm prefix)" "$OUT"
check_says "the message names PATH as the last place searched" "3. PATH" "$OUT"

echo "== the ACP driver owns the process tree it starts =="
# An ACP adapter is a supervisor: it spawns the runtime CLI and every MCP
# server, and reaps them from its own exit handler. A driver that SIGKILLs the
# adapter alone orphans that whole tree on every failing run (AGENTS.md
# Review-Proven Rule 4). This stands a fake supervisor in for the adapter: it
# spawns a long grandchild, records its pid, and never answers a request, so
# the driver must hit its timeout and take the grandchild down with the parent.
DRIVER="$HERE/openseo-smoke-acp.mjs"
if command -v node >/dev/null 2>&1; then
  TREE_DIR="$STUBS/tree"
  mkdir -p "$TREE_DIR"
  cat > "$TREE_DIR/fake-adapter" <<'ADAPTER'
#!/bin/sh
# A supervisor that never answers, holding one long-lived grandchild.
sleep 600 &
echo "$!" > "$GRANDCHILD_PID_FILE"
wait
ADAPTER
  chmod +x "$TREE_DIR/fake-adapter"
  set +e
  OUT=$(GRANDCHILD_PID_FILE="$TREE_DIR/pid" node "$DRIVER" \
    --command "$TREE_DIR/fake-adapter" \
    --cwd "$TREE_DIR" \
    --prompt "never answered" \
    --expect "never matched" \
    --timeout 3000 \
    --env "PATH=$PATH" \
    --env "GRANDCHILD_PID_FILE=$TREE_DIR/pid" 2>&1)
  RC=$?
  set -e
  check "a driver run that times out fails" 1 "$RC"
  check_says "the timeout is reported" "no reply within" "$OUT"
  GRANDCHILD=$(cat "$TREE_DIR/pid" 2>/dev/null || echo "")
  if [ -z "$GRANDCHILD" ]; then
    echo "  FAIL: the fake adapter never recorded a grandchild pid"
    fail=1
  else
    sleep 1
    if kill -0 "$GRANDCHILD" 2>/dev/null; then
      echo "  FAIL: grandchild $GRANDCHILD survived the driver — the process tree leaked"
      kill -9 "$GRANDCHILD" 2>/dev/null || true
      fail=1
    else
      echo "  PASS: the whole process tree died with the adapter"
    fi
  fi
else
  echo "  SKIP: node is not on PATH, so the driver cannot be exercised"
fi

echo "== the driver requires at least one prompt and one expectation =="
if command -v node >/dev/null 2>&1; then
  set +e
  OUT=$(node "$DRIVER" --command /bin/true --cwd "$STUBS" --expect x 2>&1); RC=$?
  set -e
  check "no --prompt is an error" 1 "$RC"
  check_says "the message names --prompt" "--prompt is required" "$OUT"
  set +e
  OUT=$(node "$DRIVER" --command /bin/true --cwd "$STUBS" --prompt x 2>&1); RC=$?
  set -e
  check "no --expect is an error" 1 "$RC"
  check_says "the message names --expect" "--expect is required" "$OUT"
fi

if [ "$fail" -ne 0 ]; then
  echo "FAILED"
  exit 1
fi
echo "OK"
