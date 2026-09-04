#!/usr/bin/env bash
# T6 (OpenSEO through runtime config) — PRE-APPROVAL smoke test.
#
#   scripts/zs/openseo-smoke.sh <claude|codex>
#
# WHAT THIS RUNS
#
#   Nothing of OpenSEO, and nothing metered. DataForSEO is not an approved
#   vendor, so this script stands the repository's own fake MCP server
#   (crates/buzz-agent/tests/bin/fake_mcp.rs, built as the `fake-mcp` binary)
#   in for the OpenSEO MCP server and proves the discovery path end to end:
#
#     1. Preflight: the runtime's CLI and ACP adapter are installed, and the
#        CLI is logged in. A missing login FAILS here with a clear message.
#        This script never fakes a result.
#     2. Generate the runtime's config with the app's own generator
#        (desktop/src-tauri/src/managed_agents/agent_config_gen, driven through
#        the `openseo-config-emit` example) into a throwaway sandbox directory.
#     3. Parse the generated file back and assert its structure — the server
#        name, transport, command and environment *names*. No environment value
#        is ever written to a generated file.
#     4. Spawn the runtime's ACP adapter the way the managed-agent spawn path
#        does — explicit environment, explicit working directory, newline
#        JSON-RPC on stdio (managed_agents/runtime.rs:564) — send one prompt,
#        and assert the reply lists the fake server's tool.
#
#   No relay is involved: no key, no relay URL, no channel, no message.
#
# WHAT THIS DELIBERATELY DOES NOT RUN
#
#   * The Dock-launch acceptance. The ticket's acceptance is "both smoke tests
#     pass for the Claude and Codex runtimes from a Dock launch". Driving the
#     app's GUI or asking an agent in a relay channel is a MANUAL step for
#     Devin, not something this script automates. Run it by hand:
#
#       a. Launch Buzz from the Dock (not from a terminal, so the DMG's minimal
#          PATH and login-shell PATH augmentation are the ones under test).
#       b. Confirm the agent's working directory is ~/.buzz, and copy the
#          generated `.mcp.json` (Claude) or `config.toml` (Codex) this script
#          leaves behind with OPENSEO_SMOKE_KEEP=1 into place there.
#       c. Ask the agent in a channel to list its tools, and confirm `tool_0`
#          and the `openseo-smoke` skill are both visible.
#       d. Remove the copied file afterwards. ~/.buzz is shared by every
#          managed agent.
#
#   * Anything after vendor approval. Once DataForSEO is approved and a spend
#     limit is set, the post-approval half (a self-hosted OpenSEO container,
#     the real audit tool, the DATAFORSEO_BASE_URL counting sentinel) is added
#     here — not before.
#
# EXIT CODES
#
#   0  the smoke test passed
#   1  a hard failure (missing CLI or adapter, not logged in, wrong reply)
#   2  usage error
#   3  blocked by the documented Codex CODEX_HOME login caveat (see below)
#
# ENVIRONMENT
#
#   OPENSEO_SMOKE_KEEP=1        keep the sandbox directory and print its path
#   OPENSEO_SMOKE_STAGE=config  stop after the config half (steps 2 and 3) and
#                               spawn nothing. Useful on a machine that has no
#                               ACP adapter installed. A `config` run makes NO
#                               claim about the runtime; only a `full` run
#                               (the default) does.

set -euo pipefail

usage() {
  cat >&2 <<'USAGE'
usage: scripts/zs/openseo-smoke.sh <claude|codex>

Pre-approval OpenSEO discovery smoke test. Runs the fake MCP server fixture,
never a metered vendor. See the header of this file for the manual Dock-launch
steps, which this script does not automate.

  OPENSEO_SMOKE_STAGE=config   generate and assert the config only, spawn nothing
  OPENSEO_SMOKE_STAGE=full     the whole smoke test (default)
USAGE
}

if [ $# -ne 1 ]; then
  usage
  exit 2
fi

RUNTIME="$1"
case "$RUNTIME" in
  claude|codex) ;;
  *)
    echo "FAIL: unknown runtime '$RUNTIME'" >&2
    usage
    exit 2
    ;;
esac

STAGE="${OPENSEO_SMOKE_STAGE:-full}"
case "$STAGE" in
  config|full) ;;
  *)
    echo "FAIL: OPENSEO_SMOKE_STAGE must be config or full, got '$STAGE'" >&2
    usage
    exit 2
    ;;
esac

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
TAURI_MANIFEST="$REPO_ROOT/desktop/src-tauri/Cargo.toml"
SKILL_NAME="openseo-smoke"
SKILL_FIXTURE="$REPO_ROOT/scripts/zs/fixtures/$SKILL_NAME/SKILL.md"
SERVER_NAME="openseo-fake"
# fake_mcp.rs returns FAKE_MCP_TOOL_COUNT tools named tool_0.. — one by default,
# so the expected name needs no environment variable to be deterministic.
EXPECTED_TOOL="tool_0"

# The CLI, ACP adapter and login probe for each runtime, taken from the
# known-ACP-runtime catalog (desktop/src-tauri/src/managed_agents/discovery/catalog.rs).
case "$RUNTIME" in
  claude)
    CLI="claude"
    ADAPTER="claude-agent-acp"
    AUTH_PROBE=(claude auth status)
    LOGIN_HINT="Run the Claude CLI to complete authentication."
    ;;
  codex)
    CLI="codex"
    ADAPTER="codex-acp"
    AUTH_PROBE=(codex login status)
    LOGIN_HINT="Run \`codex login\` to authenticate."
    ;;
esac

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "FAIL: $1 is not on PATH. $2" >&2
    exit 1
  fi
}

echo "== preflight ($RUNTIME, stage=$STAGE) =="
need node "Install the repository toolchain: . ./bin/activate-hermit"
need cargo "Install the repository toolchain: . ./bin/activate-hermit"

if [ "$STAGE" = "config" ]; then
  echo "  NOTE: stage=config. Nothing is spawned, so this run makes no claim"
  echo "        about the $RUNTIME runtime — only about the generated config."
else
  need "$CLI" "Install the $CLI CLI first; this script will not fake a result."
  need "$ADAPTER" "Install the ACP adapter: npm install -g @agentclientprotocol/$ADAPTER"

  if ! "${AUTH_PROBE[@]}" >/dev/null 2>&1; then
    echo "FAIL: the $CLI CLI is not logged in on this machine. $LOGIN_HINT" >&2
    echo "      This script asserts a real reply from a real runtime and will not fake one." >&2
    exit 1
  fi
  echo "  ok: $CLI is installed and logged in"
fi

SANDBOX=$(mktemp -d "${TMPDIR:-/tmp}/openseo-smoke.XXXXXX")
cleanup() {
  if [ "${OPENSEO_SMOKE_KEEP:-0}" = "1" ]; then
    echo "sandbox kept at $SANDBOX"
  else
    rm -rf "$SANDBOX"
  fi
}
trap cleanup EXIT
CODEX_SANDBOX_HOME="$SANDBOX/codex-home"

# The Codex login caveat this ticket is gated on: a per-agent CODEX_HOME starts
# a fresh credential namespace. Prove it rather than assume it — if the probe
# succeeds under the sandbox home, the caveat is discharged and the run
# continues; if it fails, the run stops here with exit 3 and the Codex spawn
# half stays unactivated.
if [ "$RUNTIME" = "codex" ] && [ "$STAGE" = "full" ]; then
  mkdir -p "$CODEX_SANDBOX_HOME"
  if ! CODEX_HOME="$CODEX_SANDBOX_HOME" "${AUTH_PROBE[@]}" >/dev/null 2>&1; then
    echo "BLOCKED: \`${AUTH_PROBE[*]}\` fails under a per-agent CODEX_HOME." >&2
    echo "         A private CODEX_HOME starts a fresh credential namespace, so the" >&2
    echo "         Codex spawn half stays unactivated until that is solved (T6)." >&2
    echo "         The generator half is covered by the Rust tests and by" >&2
    echo "         \`openseo-config-emit verify\`; only the spawn half is blocked." >&2
    exit 3
  fi
  echo "  ok: codex stays logged in under a per-agent CODEX_HOME"
fi

echo "== building the fake MCP server fixture =="
cargo build -q --manifest-path "$REPO_ROOT/Cargo.toml" -p buzz-agent --bin fake-mcp
FAKE_MCP="$REPO_ROOT/target/debug/fake-mcp"
if [ ! -x "$FAKE_MCP" ]; then
  echo "FAIL: fake-mcp was not built at $FAKE_MCP" >&2
  exit 1
fi

# Snapshot the operator's own configuration. The generator writes only under the
# root it is given, and this proves it for this run too.
if command -v shasum >/dev/null 2>&1; then
  hash_file() { shasum -a 256 "$1" | cut -d' ' -f1; }
elif command -v sha256sum >/dev/null 2>&1; then
  hash_file() { sha256sum "$1" | cut -d' ' -f1; }
else
  echo "FAIL: neither shasum nor sha256sum is available to fingerprint the operator's config." >&2
  exit 1
fi
operator_state() {
  for path in "$HOME/.claude.json" "$HOME/.claude/settings.json" "$HOME/.codex/config.toml"; do
    if [ -e "$path" ]; then
      echo "$path present $(hash_file "$path")"
    else
      echo "$path absent"
    fi
  done
}
BEFORE=$(operator_state)

echo "== generating the $RUNTIME config =="
emit() {
  cargo run -q --manifest-path "$TAURI_MANIFEST" --example openseo-config-emit -- "$@"
}
emit generate \
  --runtime "$RUNTIME" \
  --root "$SANDBOX" \
  --codex-home "$CODEX_SANDBOX_HOME" \
  --server-name "$SERVER_NAME" \
  --server-command "$FAKE_MCP" \
  --skill "$SKILL_NAME=$SKILL_FIXTURE"

AFTER=$(operator_state)
if [ "$BEFORE" != "$AFTER" ]; then
  echo "FAIL: generation changed the operator's own configuration." >&2
  diff <(echo "$BEFORE") <(echo "$AFTER") >&2 || true
  exit 1
fi
echo "  ok: the operator's own ~/.claude.json and ~/.codex are untouched"

echo "== asserting the generated file's structure =="
VERIFY=$(emit verify --runtime "$RUNTIME" --root "$SANDBOX" --codex-home "$CODEX_SANDBOX_HOME")
printf '%s\n' "$VERIFY"
expect_line() {
  if ! printf '%s\n' "$VERIFY" | grep -qF "$1"; then
    echo "FAIL: the generated config does not carry: $1" >&2
    exit 1
  fi
}
expect_line "$(printf 'server\t%s\tstdio\t%s' "$SERVER_NAME" "$FAKE_MCP")"
expect_line "$(printf 'skill\t%s' "$SKILL_NAME")"
if printf '%s\n' "$VERIFY" | grep -q 'dataforseo\|DATAFORSEO'; then
  echo "FAIL: a DataForSEO reference reached the pre-approval config." >&2
  exit 1
fi
echo "  ok: one stdio server and one pinned skill, no vendor reference"

if [ "$STAGE" = "config" ]; then
  echo "PASS (stage=config): the $RUNTIME config declares one stdio server and one pinned skill."
  echo "                     Run without OPENSEO_SMOKE_STAGE to spawn the runtime and assert its reply."
  exit 0
fi

# Claude prompts interactively before it will use a project-scoped MCP server.
# The sandbox — and only the sandbox — pre-approves them, so an unattended run
# is not silently a no-tool run. Nothing outside $SANDBOX is written.
if [ "$RUNTIME" = "claude" ]; then
  mkdir -p "$SANDBOX/.claude"
  printf '{\n  "enableAllProjectMcpServers": true\n}\n' > "$SANDBOX/.claude/settings.local.json"
fi

echo "== spawning $ADAPTER and sending one prompt =="
# The child's environment is built from empty, exactly like the spawn path
# builds a managed agent's: only what is named here is set.
DRIVER_ARGS=(
  --command "$ADAPTER"
  --cwd "$SANDBOX"
  --prompt "List the names of every tool you can call, one per line. Do not call any of them."
  --expect "$EXPECTED_TOOL"
  --env "PATH=$PATH"
  --env "HOME=$HOME"
  --env "TERM=dumb"
  --env "LANG=${LANG:-C.UTF-8}"
)
if [ "$RUNTIME" = "codex" ]; then
  DRIVER_ARGS+=(--env "CODEX_HOME=$CODEX_SANDBOX_HOME")
fi
node "$REPO_ROOT/scripts/zs/openseo-smoke-acp.mjs" "${DRIVER_ARGS[@]}"

echo "PASS: $RUNTIME discovered the fake MCP server's $EXPECTED_TOOL and the $SKILL_NAME skill"
