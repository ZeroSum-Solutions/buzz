import assert from "node:assert/strict";
import test from "node:test";

import {
  MCP_REGISTRY_LIMITS,
  approvalSummary,
  draftProblem,
  draftToInput,
  emptyDraft,
  entryToDraft,
  serverSupport,
  toggleServer,
} from "./mcpRegistryLogic";

const NUL = String.fromCharCode(0);

function stdioDraft(overrides = {}) {
  return {
    ...emptyDraft(),
    id: "fake",
    name: "fake",
    transport: "stdio",
    command: "/usr/local/bin/fake-mcp",
    argsText: "--stdio\n--port\n7777",
    ...overrides,
  };
}

function httpDraft(overrides = {}) {
  return {
    ...emptyDraft(),
    id: "remote",
    name: "remote",
    transport: "http",
    url: "https://mcp.example/v1",
    authScheme: "bearer",
    authSecretName: "remote-token",
    secrets: { "remote-token": "sk-live-do-not-render" },
    ...overrides,
  };
}

function entry(overrides = {}) {
  return {
    id: "fake",
    name: "fake",
    transport: "stdio",
    command: "/usr/local/bin/fake-mcp",
    args: ["--stdio"],
    url: null,
    auth_scheme: null,
    env: [],
    rejection: null,
    ...overrides,
  };
}

const BUZZ_AGENT = {
  id: "buzz-agent",
  label: "Buzz Agent",
  mcpTransports: ["stdio"],
  mcpRegistryAvailable: true,
};

const CLAUDE = {
  id: "claude",
  label: "Claude",
  mcpTransports: ["stdio", "http"],
  mcpRegistryAvailable: true,
};

// ── The approve step ──────────────────────────────────────────────────────

test("the approve step shows the exact command line that will run", () => {
  const summary = approvalSummary(stdioDraft());
  assert.equal(
    summary.target,
    "/usr/local/bin/fake-mcp --stdio --port 7777",
    "the operator must approve the command as it will actually be spawned, not a summary of it",
  );
  assert.match(summary.headline, /starts a process/);
});

test("the approve step shows the exact URL for an http entry", () => {
  const summary = approvalSummary(httpDraft());
  assert.equal(summary.target, "https://mcp.example/v1");
  assert.match(summary.headline, /sends requests/);
});

test("the approve step names references and never a secret value", () => {
  const draft = httpDraft({
    env: [{ name: "API_KEY", reference: "fake-api-key" }],
    secrets: {
      "remote-token": "sk-live-do-not-render",
      "fake-api-key": "another-secret-value",
    },
  });
  const summary = approvalSummary(draft);
  const rendered = JSON.stringify(summary);

  assert.ok(
    !rendered.includes("sk-live-do-not-render"),
    `a secret value reached the approve step: ${rendered}`,
  );
  assert.ok(
    !rendered.includes("another-secret-value"),
    `a secret value reached the approve step: ${rendered}`,
  );
  assert.deepEqual(summary.references, [
    "API_KEY = mcp:fake-api-key",
    "Authorization: bearer mcp:remote-token",
  ]);
  assert.deepEqual(
    summary.newSecrets,
    ["fake-api-key", "remote-token"],
    "the operator is told which credentials this save stores, by name",
  );
});

test("editing a stored entry carries no secret value back into the form", () => {
  const draft = entryToDraft(
    entry({
      env: [{ name: "API_KEY", reference: "mcp:fake-api-key", literal: null }],
    }),
  );
  assert.deepEqual(draft.env, [{ name: "API_KEY", reference: "fake-api-key" }]);
  assert.deepEqual(
    draft.secrets,
    {},
    "no command returns a stored value, so an edit starts with none",
  );
});

// ── Draft validation, at the caps the backend enforces ────────────────────

test("a draft is refused for the same reasons the loader refuses an entry", () => {
  assert.equal(draftProblem(stdioDraft()), null);

  const cases = [
    [{ id: "" }, /id is empty/],
    [{ id: "A".repeat(2) }, /id may only use/],
    [{ id: "a".repeat(MCP_REGISTRY_LIMITS.idLength + 1) }, /over the/],
    [{ name: "has_underscore" }, /name may only use/],
    [{ name: "a".repeat(MCP_REGISTRY_LIMITS.nameLength + 1) }, /over the/],
    [{ name: "buzz-thing" }, /reserved/],
    [{ command: "fake-mcp" }, /absolute path/],
    [{ command: `/usr/bin/${NUL}x` }, /NUL/],
    [
      { argsText: Array.from({ length: 70 }, (_, n) => `--a${n}`).join("\n") },
      /over the 64 cap/,
    ],
    [{ argsText: "x".repeat(MCP_REGISTRY_LIMITS.argLength + 1) }, /over the/],
    [{ env: [{ name: "A=B", reference: "token" }] }, /equals sign/],
    [{ env: [{ name: "A", reference: "Bad Ref" }] }, /usable secret/],
    [
      {
        env: Array.from({ length: 40 }, (_, n) => ({
          name: `V${n}`,
          reference: "token",
        })),
      },
      /over the 32 cap/,
    ],
  ];
  for (const [overrides, expected] of cases) {
    const problem = draftProblem(stdioDraft(overrides));
    assert.ok(
      problem !== null && expected.test(problem),
      `${JSON.stringify(overrides)} must be refused by ${expected}, got ${problem}`,
    );
  }
});

test("an http draft is refused for a non-loopback http url and for userinfo", () => {
  assert.equal(draftProblem(httpDraft()), null);
  assert.equal(
    draftProblem(httpDraft({ url: "http://127.0.0.1:8080/mcp" })),
    null,
    "loopback is the one http exception",
  );
  assert.match(
    draftProblem(httpDraft({ url: "http://mcp.example/v1" })) ?? "",
    /https/,
  );
  assert.match(
    draftProblem(httpDraft({ url: "https://user:token@mcp.example/v1" })) ?? "",
    /userinfo/,
  );
});

test("a secret value over the derived cap is refused before the round trip", () => {
  const problem = draftProblem(
    httpDraft({
      secrets: {
        "remote-token": "x".repeat(MCP_REGISTRY_LIMITS.envValueLength + 1),
      },
    }),
  );
  assert.match(problem ?? "", /over the/);
  assert.ok(
    !(problem ?? "").includes("xxxx"),
    "and the refusal does not echo the value",
  );
});

test("the derived env value cap keeps one NAME=VALUE argument inside the argument cap", () => {
  assert.equal(
    MCP_REGISTRY_LIMITS.envNameLength + 1 + MCP_REGISTRY_LIMITS.envValueLength,
    MCP_REGISTRY_LIMITS.argLength,
  );
});

test("a draft becomes an entry whose env carries references, never values", () => {
  const input = draftToInput(
    stdioDraft({
      env: [{ name: "API_KEY", reference: "fake-api-key" }],
      secrets: { "fake-api-key": "sk-live-do-not-render" },
    }),
  );
  assert.deepEqual(input.env, { API_KEY: "mcp:fake-api-key" });
  assert.ok(
    !JSON.stringify(input).includes("sk-live-do-not-render"),
    "the entry written to the document must never carry a value",
  );
  assert.deepEqual(input.args, ["--stdio", "--port", "7777"]);
});

// ── Capability facts, projected from the runtime catalog ──────────────────

test("an http entry on a stdio-only runtime is unsupported, from the catalog fact", () => {
  const http = entry({ id: "remote", name: "remote", transport: "http" });
  const support = serverSupport(http, BUZZ_AGENT);
  assert.equal(support.kind, "unsupported");
  assert.match(support.reason, /http/);
  assert.match(support.reason, /buzz-agent/);

  assert.equal(
    serverSupport(http, CLAUDE).kind,
    "supported",
    "a runtime whose catalog entry declares http may be offered it",
  );
  assert.equal(serverSupport(entry(), BUZZ_AGENT).kind, "supported");
});

test("a rejected entry reports the loader's own reason, which is what a spawn refuses with", () => {
  const support = serverSupport(
    entry({ rejection: "`fake` is not an absolute path" }),
    BUZZ_AGENT,
  );
  assert.equal(support.kind, "rejected");
  assert.equal(support.reason, "`fake` is not an absolute path");
});

test("an unsupported entry is refused on toggle, never silently left off", () => {
  const http = entry({ id: "remote", name: "remote", transport: "http" });
  const result = toggleServer(
    [],
    "remote",
    true,
    serverSupport(http, BUZZ_AGENT),
  );
  assert.ok("refused" in result, "the click must say why it did nothing");
  assert.match(result.refused, /cannot use/);

  const allowed = toggleServer([], "remote", true, serverSupport(http, CLAUDE));
  assert.deepEqual(allowed, { enabled: ["remote"] });
});

test("toggling off always succeeds, so a refused entry can still be removed", () => {
  const http = entry({ id: "remote", name: "remote", transport: "http" });
  assert.deepEqual(
    toggleServer(
      ["remote", "fake"],
      "remote",
      false,
      serverSupport(http, BUZZ_AGENT),
    ),
    { enabled: ["fake"] },
    "a guard that hides the only way back is a functional failure",
  );
});

test("the per-agent cap is enforced on the toggle with a reason", () => {
  const enabled = Array.from(
    { length: MCP_REGISTRY_LIMITS.serversPerAgent },
    (_, n) => `s${n}`,
  );
  const result = toggleServer(enabled, "one-more", true, { kind: "supported" });
  assert.ok("refused" in result);
  assert.match(result.refused, /at most 16/);
});

test("an agent on a harness the registry cannot configure is told so", () => {
  const support = serverSupport(entry(), null);
  assert.equal(support.kind, "runtime-unavailable");
  const result = toggleServer([], "fake", true, support);
  assert.ok("refused" in result);
  assert.match(result.refused, /not one the registry can configure/);
});

test("an agent on a harness with mcpRegistryAvailable false is told runtime-unavailable", () => {
  const goose = {
    id: "goose",
    label: "Goose",
    mcpTransports: ["stdio"],
    mcpRegistryAvailable: false,
  };
  const support = serverSupport(entry(), goose);
  assert.equal(support.kind, "runtime-unavailable");
  assert.match(support.reason, /not one the registry can configure/);
});
