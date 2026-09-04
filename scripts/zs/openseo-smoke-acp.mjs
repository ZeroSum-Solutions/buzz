#!/usr/bin/env node
// ACP driver for scripts/zs/openseo-smoke.sh.
//
// Spawns one ACP adapter exactly the way the managed-agent spawn path does —
// an explicit environment, an explicit working directory, newline-delimited
// JSON-RPC on stdio — runs initialize / session/new / session/prompt with the
// same parameter shapes buzz-acp sends (crates/buzz-acp/src/acp.rs:142,
// :648, :2060), and asserts the reply mentions an expected string.
//
// It never touches a relay: there is no key, no relay URL, and no channel.
//
// Usage:
//   openseo-smoke-acp.mjs --command ADAPTER --cwd DIR --prompt TEXT
//                         --expect SUBSTRING [--env NAME=VALUE]... [--timeout MS]
//
// The child's environment is built from empty: only the NAME=VALUE pairs passed
// with --env are set. Nothing is inherited from this process.

import { spawn } from "node:child_process";

const MAX_REPLY_CHARS = 20000; // bound the transcript we keep in memory
const DEFAULT_TIMEOUT_MS = 180000;

function parseArgs(argv) {
  const out = { env: {}, timeout: DEFAULT_TIMEOUT_MS };
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
        out.prompt = value;
        break;
      case "--expect":
        out.expect = value;
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
  for (const required of ["command", "cwd", "prompt", "expect"]) {
    if (!out[required]) {
      throw new Error(`--${required} is required`);
    }
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

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const child = spawn(args.command, [], {
    cwd: args.cwd,
    env: args.env,
    stdio: ["pipe", "pipe", "pipe"],
  });

  let nextId = 1;
  const pending = new Map();
  let transcript = "";
  let stderr = "";
  let buffer = "";

  const fail = (message) => {
    child.kill("SIGKILL");
    process.stderr.write(`${message}\n`);
    if (stderr.trim()) {
      process.stderr.write(`--- adapter stderr ---\n${stderr.slice(-4000)}\n`);
    }
    process.exit(1);
  };

  child.on("error", (e) => fail(`FAIL: cannot spawn ${args.command}: ${e.message}`));
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
        continue;
      }
      if (msg.id !== undefined) {
        // An adapter-initiated request. This driver implements no client
        // methods, so it answers explicitly rather than leaving the adapter
        // waiting on a reply that never comes.
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
      fail(`FAIL: session/new returned no sessionId: ${JSON.stringify(session)}`);
    }
    const reply = await send("session/prompt", {
      sessionId,
      prompt: [{ type: "text", text: args.prompt }],
    });
    clearTimeout(deadline);
    const haystack = `${transcript}${JSON.stringify(reply ?? {})}`;
    if (!haystack.includes(args.expect)) {
      fail(
        `FAIL: the reply never mentioned ${args.expect}\n--- transcript ---\n${haystack.slice(-4000)}`,
      );
    }
    process.stdout.write(`PASS: the reply lists ${args.expect}\n`);
    child.kill("SIGTERM");
    process.exit(0);
  } catch (e) {
    clearTimeout(deadline);
    fail(`FAIL: ${e.message}`);
  }
}

main().catch((e) => {
  process.stderr.write(`FAIL: ${e.message}\n`);
  process.exit(1);
});
