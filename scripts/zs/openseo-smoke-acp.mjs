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
//   through shutdown(): SIGTERM the group, wait out a grace period, SIGKILL
//   what is left, and await the child's `exit` before this process exits.

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

  // Signal the whole tree, not just the supervisor.
  const signalTree = (signal) => {
    if (childExited || child.pid === undefined) return;
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
    try {
      process.kill(-child.pid, signal);
    } catch {
      // ESRCH: the group is already gone. Fall back to the child alone in
      // case the group id was never established.
      try {
        child.kill(signal);
      } catch {
        // Already reaped.
      }
    }
  };

  const waitForExit = async (limitMs) => {
    const deadline = Date.now() + limitMs;
    while (!childExited && Date.now() < deadline) {
      await sleep(50);
    }
    return childExited;
  };

  // The single exit path. Terminates the tree, waits for it, then exits.
  const shutdown = async (code, message) => {
    if (shuttingDown) return;
    shuttingDown = true;
    if (message) {
      process.stderr.write(`${message}\n`);
      if (stderr.trim()) {
        process.stderr.write(`--- adapter stderr ---\n${stderr.slice(-4000)}\n`);
      }
    }
    signalTree("SIGTERM");
    if (!(await waitForExit(TERM_GRACE_MS))) {
      signalTree("SIGKILL");
      if (!(await waitForExit(KILL_GRACE_MS))) {
        // Reaping failed: say so rather than exiting 0 over a leaked tree.
        process.stderr.write(
          `FAIL: the adapter process tree (pid ${child.pid}) did not exit after SIGKILL\n`,
        );
        process.exit(code === 0 ? 1 : code);
      }
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
        // The run has to be able to actually call the fixture's tool, and the
        // adapter asks before it does. Approve the request the caller made —
        // the sandbox holds one fake MCP server and nothing else — and record
        // it so the caller can see the grant in the log.
        const options = msg.params?.options ?? [];
        const allow =
          options.find((o) => o.kind === "allow_always") ??
          options.find((o) => o.kind === "allow_once") ??
          options[0];
        if (allow?.optionId) {
          process.stdout.write(
            `  granted: ${msg.params?.toolCall?.title ?? "tool call"} (${allow.optionId})\n`,
          );
          child.stdin.write(
            `${JSON.stringify({
              jsonrpc: "2.0",
              id: msg.id,
              result: { outcome: { outcome: "selected", optionId: allow.optionId } },
            })}\n`,
          );
        } else {
          child.stdin.write(
            `${JSON.stringify({
              jsonrpc: "2.0",
              id: msg.id,
              result: { outcome: { outcome: "cancelled" } },
            })}\n`,
          );
        }
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
