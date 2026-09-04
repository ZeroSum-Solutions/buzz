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
# WHERE THE ACP ADAPTER COMES FROM
#
#   The app does not require the adapters on PATH: it installs them under its
#   own npm prefix (`<data dir>/Buzz/node-tools/bin`) and consults that prefix
#   *before* PATH — `managed_node_paths.rs::buzz_managed_command_path` is the
#   first thing `discovery.rs::resolve_command` calls. A PATH-only lookup here
#   would report "not installed" on a machine where the app itself resolves the
#   adapter fine, so this script resolves in the app's order:
#
#     1. $OPENSEO_SMOKE_ACP_BIN_DIR — an explicit override, for an install in
#        neither of the two places below. Set but adapter-less is a hard
#        failure, never a silent fallback.
#     2. the Buzz-managed npm prefix bin dir.
#     3. PATH.
#
#   The resolved path and which of the three it came from are printed. The
#   adapters are `#!/usr/bin/env node` scripts, so the child's PATH leads with
#   the adapter's own directory and the Buzz-managed node bin dir (the same two
#   entries, in the same order, that `runtime/path.rs::build_augmented_path`
#   puts ahead of the login-shell PATH) before the inherited PATH.
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
#   OPENSEO_SMOKE_ACP_BIN_DIR   directory holding the ACP adapter, tried before
#                               the Buzz-managed npm prefix and PATH.
#   OPENSEO_SMOKE_DATA_DIR      test seam: stand in for the platform data
#                               directory that holds `Buzz/node-tools/bin` and
#                               `Buzz/runtimes/node` (macOS `~/Library/
#                               Application Support`, otherwise
#                               `$XDG_DATA_HOME` or `~/.local/share`).

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

# ── Where the app itself looks for an ACP adapter ────────────────────────────
# `dirs::data_dir()`, the base `managed_node_paths.rs` builds every managed path
# from. OPENSEO_SMOKE_DATA_DIR is the test seam for it.
if [ -n "${OPENSEO_SMOKE_DATA_DIR:-}" ]; then
  BUZZ_DATA_DIR="$OPENSEO_SMOKE_DATA_DIR"
elif [ "$(uname -s)" = "Darwin" ]; then
  BUZZ_DATA_DIR="$HOME/Library/Application Support"
else
  BUZZ_DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}"
fi
# buzz_managed_npm_bin_dir(): <data dir>/Buzz/node-tools/bin
MANAGED_NPM_BIN="$BUZZ_DATA_DIR/Buzz/node-tools/bin"

# buzz_managed_node_bin_dir(): the version is read out of the production source
# rather than copied, so a bump there cannot leave this script pointing at a
# directory the app no longer uses.
MANAGED_NODE_PATHS_RS="$REPO_ROOT/desktop/src-tauri/src/managed_agents/managed_node_paths.rs"
MANAGED_NODE_VERSION=$(sed -n 's/^const BUZZ_MANAGED_NODE_VERSION: &str = "\(.*\)";$/\1/p' "$MANAGED_NODE_PATHS_RS")
if [ -z "$MANAGED_NODE_VERSION" ]; then
  echo "FAIL: cannot read BUZZ_MANAGED_NODE_VERSION from $MANAGED_NODE_PATHS_RS." >&2
  echo "      The managed-node path in this script is bound to that constant; fix the binding." >&2
  exit 1
fi
case "$(uname -s)/$(uname -m)" in
  Darwin/arm64) MANAGED_NODE_PLATFORM="darwin-arm64" ;;
  Darwin/x86_64) MANAGED_NODE_PLATFORM="darwin-x64" ;;
  Linux/x86_64) MANAGED_NODE_PLATFORM="linux-x64" ;;
  Linux/aarch64|Linux/arm64) MANAGED_NODE_PLATFORM="linux-arm64" ;;
  *) MANAGED_NODE_PLATFORM="" ;;
esac
MANAGED_NODE_BIN=""
if [ -n "$MANAGED_NODE_PLATFORM" ]; then
  MANAGED_NODE_BIN="$BUZZ_DATA_DIR/Buzz/runtimes/node/$MANAGED_NODE_VERSION/$MANAGED_NODE_PLATFORM/bin"
fi

# Resolve the ACP adapter in the app's own order and say which source won.
# Sets ADAPTER_PATH and ADAPTER_SOURCE; exits 1 with every place searched named
# when nothing holds it.
ADAPTER_PATH=""
ADAPTER_SOURCE=""
resolve_adapter() {
  local candidate
  if [ -n "${OPENSEO_SMOKE_ACP_BIN_DIR:-}" ]; then
    candidate="$OPENSEO_SMOKE_ACP_BIN_DIR/$ADAPTER"
    if [ -x "$candidate" ]; then
      ADAPTER_PATH="$candidate"
      ADAPTER_SOURCE="OPENSEO_SMOKE_ACP_BIN_DIR"
      return 0
    fi
    echo "FAIL: OPENSEO_SMOKE_ACP_BIN_DIR=$OPENSEO_SMOKE_ACP_BIN_DIR holds no executable $ADAPTER." >&2
    echo "      An explicit override is not a hint: fix it or unset it. This script will not" >&2
    echo "      quietly fall back to another $ADAPTER than the one you named." >&2
    exit 1
  fi

  candidate="$MANAGED_NPM_BIN/$ADAPTER"
  if [ -x "$candidate" ]; then
    ADAPTER_PATH="$candidate"
    ADAPTER_SOURCE="the Buzz-managed npm prefix ($MANAGED_NPM_BIN)"
    return 0
  fi

  if candidate=$(command -v "$ADAPTER" 2>/dev/null); then
    ADAPTER_PATH="$candidate"
    ADAPTER_SOURCE="PATH"
    return 0
  fi

  echo "FAIL: $ADAPTER was not found. Searched, in the order the app searches:" >&2
  echo "        1. \$OPENSEO_SMOKE_ACP_BIN_DIR (unset)" >&2
  echo "        2. $MANAGED_NPM_BIN (the Buzz-managed npm prefix)" >&2
  echo "        3. PATH" >&2
  echo "      Install the ACP adapter: npm install -g @agentclientprotocol/$ADAPTER" >&2
  echo "      or install it through the app, or point OPENSEO_SMOKE_ACP_BIN_DIR at it." >&2
  exit 1
}

echo "== preflight ($RUNTIME, stage=$STAGE) =="
need node "Install the repository toolchain: . ./bin/activate-hermit"
need cargo "Install the repository toolchain: . ./bin/activate-hermit"

if [ "$STAGE" = "config" ]; then
  echo "  NOTE: stage=config. Nothing is spawned, so this run makes no claim"
  echo "        about the $RUNTIME runtime — only about the generated config."
else
  need "$CLI" "Install the $CLI CLI first; this script will not fake a result."
  resolve_adapter
  echo "  ok: $ADAPTER resolved from $ADAPTER_SOURCE -> $ADAPTER_PATH"

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
# builds a managed agent's: only what is named here is set. Its PATH leads with
# the adapter's own directory and the Buzz-managed node bin dir, in the order
# `runtime/path.rs::build_augmented_path` puts them ahead of the login-shell
# PATH — the adapters are `#!/usr/bin/env node` scripts, so a node the app
# manages must be reachable even when the ambient PATH has none.
CHILD_PATH="$PATH"
if [ -n "$MANAGED_NODE_BIN" ] && [ -x "$MANAGED_NODE_BIN/node" ]; then
  CHILD_PATH="$MANAGED_NODE_BIN:$CHILD_PATH"
  echo "  ok: the Buzz-managed node ($MANAGED_NODE_BIN/node) leads the child PATH"
else
  echo "  note: no Buzz-managed node at ${MANAGED_NODE_BIN:-<unsupported platform>}; the child uses the inherited PATH's node"
fi
CHILD_PATH="$(dirname "$ADAPTER_PATH"):$CHILD_PATH"
DRIVER_ARGS=(
  --command "$ADAPTER_PATH"
  --cwd "$SANDBOX"
  --prompt "List the names of every tool you can call, one per line. Do not call any of them."
  --expect "$EXPECTED_TOOL"
  --env "PATH=$CHILD_PATH"
  --env "HOME=$HOME"
  # USER is not cosmetic here: the Claude Agent SDK looks its subscription
  # credential up in the macOS keychain by account name, so an adapter spawned
  # without USER answers `session/prompt` with "Authentication required" even
  # though `claude auth status` exits 0. Measured on this machine: a child with
  # PATH/HOME/TERM/LANG alone fails; the same child plus USER succeeds. The
  # production spawn inherits the parent environment (crates/buzz-acp/src/acp.rs
  # sets extra vars on an otherwise inherited env), so it never lost USER; this
  # driver, which builds the environment from empty, has to name it.
  --env "USER=${USER:-$(id -un)}"
  --env "LOGNAME=${LOGNAME:-${USER:-$(id -un)}}"
  --env "TERM=dumb"
  --env "LANG=${LANG:-C.UTF-8}"
)
if [ "$RUNTIME" = "codex" ]; then
  DRIVER_ARGS+=(--env "CODEX_HOME=$CODEX_SANDBOX_HOME")
fi
node "$REPO_ROOT/scripts/zs/openseo-smoke-acp.mjs" "${DRIVER_ARGS[@]}"

echo "PASS: $RUNTIME discovered the fake MCP server's $EXPECTED_TOOL and the $SKILL_NAME skill"
