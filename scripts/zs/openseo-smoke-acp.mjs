#!/usr/bin/env node
// ACP driver for scripts/zs/openseo-smoke.sh.
//
// Spawns one ACP adapter exactly the way the managed-agent spawn path does —
// an explicit environment, an explicit working directory, newline-delimited
// JSON-RPC on stdio — runs initialize / session/new / one session/prompt per
// --prompt with the same parameter shapes buzz-acp sends
// (crates/buzz-acp/src/acp.rs:142, :648, :2060), and asserts every --expect
// string appears in the session transcript.
//
// It never touches a relay: there is no key, no relay URL, and no channel.
//
// Usage:
//   openseo-smoke-acp.mjs --command ADAPTER --cwd DIR
//                         --prompt TEXT [--prompt TEXT]...
//                         --expect SUBSTRING [--expect SUBSTRING]...
//                         [--env NAME=VALUE]... [--timeout MS]
//
// The child's environment is built from empty: only the NAME=VALUE pairs passed
// with --env are set. Nothing is inherited from this process.
//
// PROCESS TREE OWNERSHIP
//
//   The adapter is a supervisor: it spawns the runtime CLI and every MCP server
//   the session declares, and reaps them from its own exit handler. SIGKILLing
//   it therefore orphans that whole tree — a live CLI session plus its MCP
//   children — on every failing run. AGENTS.md Review-Proven Rule 4 names this
//   class. So this driver owns the tree instead:
//
//     * POSIX: the child is spawned `detached`, which makes it the leader of a
//       new process group, and every signal goes to the group (`-pid`).
//     * Windows has no process groups to signal; `taskkill /T` walks the child
//       tree, which is the equivalent reach.
//
//   Every exit path — assertion failure, timeout, spawn error, success — goes
//   through shutdown(): SIGTERM the group, wait out a grace period, then
//   ALWAYS SIGKILL the group, then probe the GROUP — not the leader — until it
//   is empty. The adapter exiting proves nothing: a descendant that ignores
//   SIGTERM outlives it, and a shutdown that stopped once the leader was reaped
//   would return success over a live orphan. A group that is still populated
//   after SIGKILL is reported and exits non-zero.
//
// TOOL PERMISSION
//
//   The adapter asks before it calls a tool. This driver approves exactly one:
//   the per-run fake tool named by --allow-tool, with `allow_once` — never
//   `allow_always`, which would leave a persistent grant behind. Every other
//   request is rejected and its tool name printed to stderr, and any rejection
//   fails the run: a smoke run that had to deny a tool is not a run whose pass
//   means anything.

import { spawn, spawnSync } from "node:child_process";

const MAX_REPLY_CHARS = 20000; // bound the transcript we keep in memory
const DEFAULT_TIMEOUT_MS = 180000;
const TERM_GRACE_MS = 5000; // time the tree gets to exit on SIGTERM
const KILL_GRACE_MS = 5000; // time to reap after SIGKILL before giving up
const IS_WINDOWS = process.platform === "win32";

function parseArgs(argv) {
  const out = { env: {}, prompts: [], expects: [], timeout: DEFAULT_TIMEOUT_MS };
  for (let i = 0; i < argv.length; i += 1) {
    const flag = argv[i];
    const value = argv[i + 1];
    if (value === undefined) {
      throw new Error(`${flag} needs a value`);
    }
    i += 1;
    switch (flag) {
      case "--command":
        out.command = value;
        break;
      case "--cwd":
        out.cwd = value;
        break;
      case "--prompt":
        out.prompts.push(value);
        break;
      case "--expect":
        out.expects.push(value);
        break;
      case "--allowTool":
      case "--allow-tool":
        out.allowTool = value;
        break;
      case "--timeout":
        out.timeout = Number.parseInt(value, 10);
        break;
      case "--env": {
        const eq = value.indexOf("=");
        if (eq <= 0) {
          throw new Error(`--env wants NAME=VALUE, got ${value}`);
        }
        out.env[value.slice(0, eq)] = value.slice(eq + 1);
        break;
      }
      default:
        throw new Error(`unknown flag ${flag}`);
    }
  }
  for (const required of ["command", "cwd"]) {
    if (!out[required]) {
      throw new Error(`--${required} is required`);
    }
  }
  if (out.prompts.length === 0) {
    throw new Error("--prompt is required (repeatable)");
  }
  if (out.expects.length === 0) {
    throw new Error("--expect is required (repeatable)");
  }
  if (!Number.isFinite(out.timeout) || out.timeout <= 0) {
    throw new Error("--timeout must be a positive number of milliseconds");
  }
  return out;
}

function clientCapabilities() {
  // Mirrors buzz-acp's build_client_capabilities (acp.rs:409).
  return {
    auth: { terminal: true },
    _meta: { goose: { customNotifications: true }, "terminal-auth": true },
  };
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const child = spawn(args.command, [], {
    cwd: args.cwd,
    env: args.env,
    stdio: ["pipe", "pipe", "pipe"],
    // POSIX: lead a new process group so the whole tree can be signalled.
    detached: !IS_WINDOWS,
  });

  let nextId = 1;
  const pending = new Map();
  let transcript = "";
  let stderr = "";
  // Expectations are matched as the session runs, not at the end: the
  // transcript is capped at MAX_REPLY_CHARS, so a match that scrolled out of
  // the retained window would otherwise read as a miss.
  const remaining = new Set(args.expects);
  const noteMatches = (haystack) => {
    for (const expected of remaining) {
      if (haystack.includes(expected)) remaining.delete(expected);
    }
  };
  let buffer = "";
  let childExited = false;
  let shuttingDown = false;
  child.on("exit", () => {
    childExited = true;
  });

  // `detached` makes the child the leader of a new group whose id is its pid.
  // This process is NOT in that group, so signalling it cannot reach us.
  const pgid = IS_WINDOWS ? undefined : child.pid;

  // Signal the whole GROUP, not just the supervisor. Deliberately not
  // short-circuited on `childExited`: the leader exiting is exactly the case
  // where a TERM-ignoring descendant is still running under its group id.
  const signalGroup = (signal) => {
    if (child.pid === undefined) return;
    if (IS_WINDOWS) {
      // No process groups to signal; /T walks the child tree, /F is the
      // SIGKILL equivalent. taskkill has no graceful mode for a console-less
      // child, so the SIGTERM pass is a no-op here and the kill pass reaps.
      if (signal === "SIGKILL") {
        spawnSync("taskkill", ["/pid", String(child.pid), "/T", "/F"], {
          stdio: "ignore",
        });
      }
      return;
    }
    let groupSignalled = false;
    try {
      process.kill(-pgid, signal);
      groupSignalled = true;
    } catch {
      // ESRCH: the group is empty (or was never established).
    }
    if (!groupSignalled) {
      // Fall back to the child alone, for a platform or a spawn where the
      // group id was never established.
      try {
        child.kill(signal);
      } catch {
        // Already reaped.
      }
    }
  };

  // Is any process still in the child's group? `kill(-pgid, 0)` succeeds while
  // the group has a member and raises ESRCH once it is empty; EPERM means it
  // exists but is not ours to signal, which is still "not empty".
  const groupAlive = () => {
    if (IS_WINDOWS || pgid === undefined) return !childExited;
    try {
      process.kill(-pgid, 0);
      return true;
    } catch (e) {
      return e.code === "EPERM";
    }
  };

  // Best-effort names for whatever is left, for the failure report only.
  const groupSurvivors = () => {
    if (IS_WINDOWS || pgid === undefined) return "";
    const ps = spawnSync("ps", ["-o", "pid=,ppid=,command=", "-g", String(pgid)], {
      encoding: "utf8",
    });
    return (ps.stdout ?? "").trim();
  };

  const waitForGroupGone = async (limitMs) => {
    const deadline = Date.now() + limitMs;
    for (;;) {
      if (!groupAlive()) return true;
      if (Date.now() >= deadline) return false;
      await sleep(50);
    }
  };

  const waitForExit = async (limitMs) => {
    const deadline = Date.now() + limitMs;
    while (!childExited && Date.now() < deadline) {
      await sleep(50);
    }
    return childExited;
  };

  // The single exit path. Terminates the group, proves it is empty, then exits.
  const shutdown = async (code, message) => {
    if (shuttingDown) return;
    shuttingDown = true;
    if (message) {
      process.stderr.write(`${message}\n`);
      if (stderr.trim()) {
        process.stderr.write(`--- adapter stderr ---\n${stderr.slice(-4000)}\n`);
      }
    }
    signalGroup("SIGTERM");
    await waitForGroupGone(TERM_GRACE_MS);
    // Unconditional. The graceful pass may have taken the supervisor and left
    // a descendant that ignores SIGTERM; the supervisor's exit is not evidence
    // the group is empty, so the hard kill is never skipped.
    signalGroup("SIGKILL");
    const reaped = await waitForGroupGone(KILL_GRACE_MS);
    await waitForExit(100);
    if (!reaped) {
      // Say so rather than exiting 0 over a leaked tree.
      const survivors = groupSurvivors();
      process.stderr.write(
        `FAIL: the adapter process group (pgid ${pgid}) is not empty after SIGKILL\n` +
          (survivors ? `--- survivors ---\n${survivors}\n` : ""),
      );
      process.exit(code === 0 ? 1 : code);
    }
    process.exit(code);
  };

  const fail = (message) => {
    void shutdown(1, message);
  };

  child.on("error", (e) => {
    // No usable pid on a spawn failure; there is no tree to reap.
    childExited = true;
    fail(`FAIL: cannot spawn ${args.command}: ${e.message}`);
  });
  child.stderr.on("data", (chunk) => {
    stderr = (stderr + chunk.toString()).slice(-MAX_REPLY_CHARS);
  });

  // ── tool permission ─────────────────────────────────────────────────────────
  // Exactly one tool may be approved: the per-run fake tool named by
  // --allow-tool. Its name is drawn from /dev/urandom by the caller, so a match
  // on it is unforgeable — but only when the match is made against the
  // request's tool IDENTITY. `title` and `rawInput` are deliberately not
  // identity: the prompt names the tool, so a shell tool call whose title is
  // its command line would echo the allowed name straight back and be approved.
  const bareTool = args.allowTool ? args.allowTool.split("__").pop() : "";
  const namesAllowedTool = (value) => {
    if (typeof value !== "string" || !args.allowTool) return false;
    const v = value.trim();
    if (v === args.allowTool) return true;
    // Claude qualifies an MCP tool as mcp__<server>__<tool>. Accept that shape
    // around the per-run name — the server prefix is the runtime's to choose —
    // and nothing else.
    return bareTool !== "" && v.startsWith("mcp__") && v.endsWith(`__${bareTool}`);
  };
  const toolIdentity = (toolCall) => {
    const parts = [];
    for (const field of ["name", "toolName"]) {
      if (typeof toolCall?.[field] === "string") parts.push(toolCall[field]);
    }
    const meta = toolCall?._meta?.claudeCode?.toolName;
    if (typeof meta === "string") parts.push(meta);
    return parts;
  };
  // Every request this driver refused. A run that had to deny a tool is not a
  // run whose PASS means anything, so this fails the run at the end even when
  // every expectation matched.
  const denied = new Set();
  const answerPermission = (msg) => {
    const toolCall = msg.params?.toolCall ?? {};
    const identity = toolIdentity(toolCall);
    const label = identity[0] ?? toolCall.title ?? "<unnamed tool>";
    const options = msg.params?.options ?? [];
    const reply = (result) =>
      child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id: msg.id, result })}\n`);
    if (identity.some(namesAllowedTool)) {
      // allow_once, never allow_always: a persistent grant would outlive the
      // run and approve the same tool for whatever asks next.
      const allow = options.find((o) => o.kind === "allow_once");
      if (allow?.optionId) {
        process.stdout.write(`  granted: ${label} (allow_once, ${allow.optionId})\n`);
        reply({ outcome: { outcome: "selected", optionId: allow.optionId } });
        return;
      }
      denied.add(`${label} (no allow_once option was offered)`);
    } else {
      denied.add(label);
    }
    process.stderr.write(
      `DENIED: ${label} asked for permission; this run allows only ${args.allowTool ?? "<no --allow-tool>"}\n`,
    );
    const reject = options.find((o) => o.kind === "reject_once");
    if (reject?.optionId) {
      reply({ outcome: { outcome: "selected", optionId: reject.optionId } });
    } else {
      reply({ outcome: { outcome: "cancelled" } });
    }
  };

  const send = (method, params) =>
    new Promise((resolve, reject) => {
      const id = nextId;
      nextId += 1;
      pending.set(id, { resolve, reject });
      child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
    });

  child.stdout.on("data", (chunk) => {
    buffer += chunk.toString();
    let newline = buffer.indexOf("\n");
    while (newline >= 0) {
      const line = buffer.slice(0, newline).trim();
      buffer = buffer.slice(newline + 1);
      newline = buffer.indexOf("\n");
      if (!line) continue;
      let msg;
      try {
        msg = JSON.parse(line);
      } catch {
        continue; // adapters may print non-JSON diagnostics on stdout
      }
      if (msg.id !== undefined && msg.method === undefined) {
        const waiter = pending.get(msg.id);
        pending.delete(msg.id);
        if (!waiter) continue;
        if (msg.error) {
          waiter.reject(new Error(`${JSON.stringify(msg.error)}`));
        } else {
          waiter.resolve(msg.result);
        }
        continue;
      }
      if (msg.method === "session/update") {
        transcript = (transcript + JSON.stringify(msg.params ?? {})).slice(-MAX_REPLY_CHARS);
        noteMatches(transcript);
        continue;
      }
      if (msg.method === "session/request_permission") {
        answerPermission(msg);
        continue;
      }
      if (msg.id !== undefined) {
        // Any other adapter-initiated request. This driver implements no
        // further client methods, so it answers explicitly rather than leaving
        // the adapter waiting on a reply that never comes.
        child.stdin.write(
          `${JSON.stringify({
            jsonrpc: "2.0",
            id: msg.id,
            error: { code: -32601, message: "smoke driver implements no client methods" },
          })}\n`,
        );
      }
    }
  });

  const deadline = setTimeout(
    () => fail(`FAIL: no reply within ${args.timeout} ms`),
    args.timeout,
  );

  try {
    await send("initialize", {
      protocolVersion: 2,
      clientCapabilities: clientCapabilities(),
      clientInfo: { name: "openseo-smoke", version: "1" },
    });
    const session = await send("session/new", { cwd: args.cwd, mcpServers: [] });
    const sessionId = session?.sessionId;
    if (!sessionId) {
      await shutdown(1, `FAIL: session/new returned no sessionId: ${JSON.stringify(session)}`);
      return;
    }
    let replies = "";
    for (const prompt of args.prompts) {
      const reply = await send("session/prompt", {
        sessionId,
        prompt: [{ type: "text", text: prompt }],
      });
      replies = (replies + JSON.stringify(reply ?? {})).slice(-MAX_REPLY_CHARS);
      noteMatches(replies);
    }
    clearTimeout(deadline);
    if (denied.size > 0) {
      await shutdown(
        1,
        `FAIL: the session asked for a tool this run does not allow: ${[...denied].join(", ")}\n` +
          `      allowed: ${args.allowTool ?? "<no --allow-tool>"}`,
      );
      return;
    }
    if (remaining.size > 0) {
      await shutdown(
        1,
        `FAIL: the session never mentioned ${[...remaining].join(", ")}\n` +
          `--- transcript tail ---\n${`${transcript}${replies}`.slice(-6000)}`,
      );
      return;
    }
    process.stdout.write(`PASS: the session lists ${args.expects.join(", ")}\n`);
    await shutdown(0, null);
  } catch (e) {
    clearTimeout(deadline);
    await shutdown(1, `FAIL: ${e.message}`);
  }
}

main().catch((e) => {
  process.stderr.write(`FAIL: ${e.message}\n`);
  process.exit(1);
});
