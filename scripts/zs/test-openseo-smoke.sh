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

echo "== the write-set containment guard =="
# The smoke script no longer hashes the operator's ~/.claude.json: a live Claude
# Code session rewrites that file every few seconds, so the bracket failed
# spuriously. The guard binds to the generator's own `wrote<TAB>path` report
# instead, and these cases prove it still catches an escape. Removing the
# containment test in openseo-smoke.sh, or widening it to accept any path,
# leaves them red.
guard() { # guard <root> <report> -> sets OUT, RC
  local root="$1" report="$2"
  set +e
  OUT=$( { OPENSEO_SMOKE_LIB=1 . "$SMOKE"; assert_writes_under_root "$root" "$report"; } 2>&1 )
  RC=$?
  set -e
}

GUARD_ROOT="$STUBS/sandbox"
guard "$GUARD_ROOT" "$(printf 'wrote\t%s/.mcp.json\nwrote\t%s/.claude/settings.local.json\n' "$GUARD_ROOT" "$GUARD_ROOT")"
check "a write set wholly inside the root passes" 0 "$RC"

guard "$GUARD_ROOT" "$(printf 'wrote\t%s/.mcp.json\nwrote\t%s/.claude.json\n' "$GUARD_ROOT" "$HOME")"
check "a write that escapes the root fails" 1 "$RC"
check_says "the message names the escaping path" "$HOME/.claude.json" "$OUT"

guard "$GUARD_ROOT" "$(printf 'wrote\t%s/../.claude.json\n' "$GUARD_ROOT")"
check "a write reached through '..' fails" 1 "$RC"
check_says "the message names the traversal" "'..' component" "$OUT"

guard "$GUARD_ROOT" ""
check "an empty write set fails rather than passing vacuously" 1 "$RC"
check_says "the message says the guard proved nothing" "proved nothing" "$OUT"

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

echo "== a descendant that ignores SIGTERM is still reaped =="
# The adapter exiting is NOT evidence its tree is gone. This supervisor exits
# promptly on SIGTERM while its own grandchild ignores it — exactly the state a
# shutdown that stops once the leader is reaped reports as success, with a live
# orphan left behind. The driver must therefore always hard-kill the group and
# probe the GROUP, not the leader. Dropping the unconditional SIGKILL in
# openseo-smoke-acp.mjs leaves this case red.
if command -v node >/dev/null 2>&1; then
  STUBBORN_DIR="$STUBS/stubborn"
  mkdir -p "$STUBBORN_DIR"
  cat > "$STUBBORN_DIR/fake-adapter" <<'ADAPTER'
#!/bin/sh
# A supervisor whose grandchild ignores SIGTERM. The supervisor itself exits on
# SIGTERM, so the leader is reaped while its process group is still populated.
sh -c 'trap "" TERM; echo $$ > "$GRANDCHILD_PID_FILE"; while :; do sleep 1; done' &
trap 'exit 0' TERM
wait
ADAPTER
  chmod +x "$STUBBORN_DIR/fake-adapter"
  set +e
  OUT=$(node "$DRIVER" \
    --command "$STUBBORN_DIR/fake-adapter" \
    --cwd "$STUBBORN_DIR" \
    --prompt "never answered" \
    --expect "never matched" \
    --timeout 3000 \
    --env "PATH=$PATH" \
    --env "GRANDCHILD_PID_FILE=$STUBBORN_DIR/pid" 2>&1)
  RC=$?
  set -e
  check "a run whose adapter never answers still fails" 1 "$RC"
  check_says "the timeout is reported" "no reply within" "$OUT"
  check_lacks "and the group is not reported as leaked" "is not empty after SIGKILL" "$OUT"
  STUBBORN=$(cat "$STUBBORN_DIR/pid" 2>/dev/null || echo "")
  if [ -z "$STUBBORN" ]; then
    echo "  FAIL: the fake adapter never recorded a TERM-ignoring grandchild pid"
    fail=1
  else
    sleep 1
    if kill -0 "$STUBBORN" 2>/dev/null; then
      echo "  FAIL: TERM-ignoring grandchild $STUBBORN survived the driver — the process group leaked"
      kill -9 "$STUBBORN" 2>/dev/null || true
      fail=1
    else
      echo "  PASS: a grandchild that ignores SIGTERM is killed with the group"
    fi
  fi
fi

echo "== a descendant that LEAVES the process group is still reaped =="
# `setsid` puts a process in a new session and a new process group, so the
# adapter's group id no longer names it: `kill(-pgid, …)` never reaches it and
# `kill(-pgid, 0)` reports the group empty while the escapee runs on. Its ppid
# still leads back to the adapter while the adapter lives, which is the only
# evidence macOS offers — there is no cgroup — so the driver must snapshot the
# descendant tree from the process table BEFORE signalling and kill those pids
# too. Dropping the snapshotted-pid kill in openseo-smoke-acp.mjs leaves this
# case red.
if command -v node >/dev/null 2>&1; then
  ESCAPE_DIR="$STUBS/escaped"
  mkdir -p "$ESCAPE_DIR"
  # The escapee records its OWN pid after the escape, so the pid checked below
  # is the one that actually left the group, whatever forking happened on the
  # way. macOS ships no setsid(1); perl's POSIX::setsid is the same call.
  ESCAPEE=""
  if command -v setsid >/dev/null 2>&1; then
    ESCAPEE="$ESCAPE_DIR/escape"
    cat > "$ESCAPEE" <<'ESC'
#!/bin/sh
exec setsid sh -c 'echo $$ > "$ESCAPED_PID_FILE"; exec sleep 600'
ESC
  elif command -v perl >/dev/null 2>&1; then
    ESCAPEE="$ESCAPE_DIR/escape"
    cat > "$ESCAPEE" <<'ESC'
#!/usr/bin/env perl
use POSIX ();
POSIX::setsid() or die "setsid: $!";
open(my $fh, '>', $ENV{ESCAPED_PID_FILE}) or die "open: $!";
print $fh "$$\n";
close $fh;
exec('sleep', '600') or die "exec: $!";
ESC
  fi
  if [ -z "$ESCAPEE" ]; then
    echo "  SKIP: neither setsid nor perl is available to build an escaped descendant"
  else
    chmod +x "$ESCAPEE"
    cat > "$ESCAPE_DIR/fake-adapter" <<'ADAPTER'
#!/bin/sh
# A supervisor that never answers, holding one grandchild that leaves the
# adapter's process group outright.
"$ESCAPEE_PATH" &
wait
ADAPTER
    chmod +x "$ESCAPE_DIR/fake-adapter"
    rm -f "$ESCAPE_DIR/pid"
    set +e
    OUT=$(node "$DRIVER" \
      --command "$ESCAPE_DIR/fake-adapter" \
      --cwd "$ESCAPE_DIR" \
      --prompt "never answered" \
      --expect "never matched" \
      --timeout 3000 \
      --env "PATH=$PATH" \
      --env "ESCAPEE_PATH=$ESCAPEE" \
      --env "ESCAPED_PID_FILE=$ESCAPE_DIR/pid" 2>&1)
    RC=$?
    set -e
    check "a run whose adapter never answers still fails" 1 "$RC"
    check_lacks "and no survivor is reported" "is not empty after SIGKILL" "$OUT"
    ESCAPED=$(cat "$ESCAPE_DIR/pid" 2>/dev/null || echo "")
    if [ -z "$ESCAPED" ]; then
      echo "  FAIL: the fake adapter never recorded an escaped grandchild pid"
      fail=1
    else
      sleep 1
      if kill -0 "$ESCAPED" 2>/dev/null; then
        echo "  FAIL: escaped grandchild $ESCAPED survived the driver — a setsid descendant leaked"
        kill -9 "$ESCAPED" 2>/dev/null || true
        fail=1
      else
        echo "  PASS: a descendant that left the process group is killed from the snapshot"
      fi
    fi
  fi
fi

echo "== the driver approves only the tool it was told to allow =="
# The adapter asks before it calls a tool, so the driver's answer IS the
# authorization. Approving whatever is asked hands a deviating model — or a
# hostile MCP server — the operator's account. This fake adapter asks for one
# named tool, reports the outcome it was given, and then answers the prompt
# with the expected text either way, so only the permission decision can change
# the run's result. Its request TITLE carries the allowed name while its tool
# identity does not: a driver that matched on the title (or on rawInput, which
# echoes the prompt) would be fooled by it.
#
# REQUEST_SHAPE picks the wire shape of session/request_permission. The driver
# negotiates protocol 2, whose tool call lives under params.subject.toolCall
# (crates/buzz-agent/src/wire.rs:176); the installed claude-agent-acp was
# observed answering the same initialize with the legacy top-level
# params.toolCall. Both shapes are exercised below, and the driver must reach
# the same decision on each — a driver that reads only one shape sees the other
# as an unnamed tool and denies the tool the run depends on.
if command -v node >/dev/null 2>&1; then
  PERM_DIR="$STUBS/permission"
  mkdir -p "$PERM_DIR"
  cat > "$PERM_DIR/fake-adapter" <<'ADAPTER'
#!/usr/bin/env node
let buf = "";
let promptId = null;
const PERMISSION_ID = 9001;
const send = (o) => process.stdout.write(`${JSON.stringify(o)}\n`);
const reply = (id, result) => send({ jsonrpc: "2.0", id, result });
const permissionParams = () => {
  const toolCall = {
    toolCallId: "t1",
    name: process.env.ASK_TOOL,
    title: process.env.EXPECT_TEXT,
    rawInput: { note: process.env.EXPECT_TEXT },
    status: "pending",
  };
  const options = [
    { optionId: "once", name: "Yes", kind: "allow_once" },
    { optionId: "always", name: "Yes, and don't ask again", kind: "allow_always" },
    { optionId: "no", name: "No", kind: "reject_once" },
  ];
  if (process.env.REQUEST_SHAPE === "v1") {
    return { sessionId: "s1", toolCall, options };
  }
  // ACP v2: the tool call hangs off `subject`, with the title at the top level.
  return {
    sessionId: "s1",
    title: process.env.EXPECT_TEXT,
    subject: { type: "tool_call", toolCall },
    options,
  };
};
process.stdin.on("data", (chunk) => {
  buf += chunk.toString();
  let nl;
  while ((nl = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, nl).trim();
    buf = buf.slice(nl + 1);
    if (!line) continue;
    const msg = JSON.parse(line);
    if (msg.method === "initialize") {
      reply(msg.id, { protocolVersion: 2 });
    } else if (msg.method === "session/new") {
      reply(msg.id, { sessionId: "s1" });
    } else if (msg.method === "session/prompt") {
      promptId = msg.id;
      send({
        jsonrpc: "2.0",
        id: PERMISSION_ID,
        method: "session/request_permission",
        params: permissionParams(),
      });
    } else if (msg.id === PERMISSION_ID) {
      process.stderr.write(`outcome=${JSON.stringify(msg.result?.outcome ?? msg.error)}\n`);
      send({
        jsonrpc: "2.0",
        method: "session/update",
        params: { sessionId: "s1", update: { text: process.env.EXPECT_TEXT } },
      });
      reply(promptId, { stopReason: "end_turn" });
    }
  }
});
ADAPTER
  chmod +x "$PERM_DIR/fake-adapter"
  ALLOWED_TOOL="smoke_deadbeef01"
  QUALIFIED_TOOL="mcp__openseo-fake__$ALLOWED_TOOL"

  permission_run() { # permission_run <shape> <asked tool> -> sets OUT, RC
    set +e
    OUT=$(node "$DRIVER" \
      --command "$PERM_DIR/fake-adapter" \
      --cwd "$PERM_DIR" \
      --prompt "call it" \
      --expect "$ALLOWED_TOOL" \
      --allow-tool "$QUALIFIED_TOOL" \
      --timeout 20000 \
      --env "PATH=$PATH" \
      --env "REQUEST_SHAPE=$1" \
      --env "ASK_TOOL=$2" \
      --env "EXPECT_TEXT=$ALLOWED_TOOL" 2>&1)
    RC=$?
    set -e
  }

  for SHAPE in v2 v1; do
    permission_run "$SHAPE" "$QUALIFIED_TOOL"
    check "[$SHAPE] the one allowed tool is approved and the run passes" 0 "$RC"
    check_says "[$SHAPE] the grant is allow_once, never allow_always" "(allow_once, once)" "$OUT"
    check_lacks "[$SHAPE] no persistent grant was taken" "always" "$OUT"

    permission_run "$SHAPE" "mcp__hostile__shell_exec"
    check "[$SHAPE] a request for another tool fails the run" 1 "$RC"
    check_says "[$SHAPE] the denied tool is named on stderr" "mcp__hostile__shell_exec" "$OUT"
    check_says "[$SHAPE] the run says why it failed" "asked for a tool this run does not allow" "$OUT"
    check_says "[$SHAPE] the adapter was told no" '"optionId":"no"' "$OUT"
    check_lacks "[$SHAPE] nothing was granted" "granted:" "$OUT"

    # The same BARE tool under a different server is a different program. A
    # matcher that strips the server prefix — or accepts any `mcp__*__<tool>`
    # suffix — approves this one, so it must be denied byte-for-byte.
    permission_run "$SHAPE" "mcp__hostile__$ALLOWED_TOOL"
    check "[$SHAPE] the same bare tool under another server is refused" 1 "$RC"
    check_says "[$SHAPE] the impostor is named on stderr" "mcp__hostile__$ALLOWED_TOOL" "$OUT"
    check_lacks "[$SHAPE] the impostor was not granted" "granted:" "$OUT"
  done
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
