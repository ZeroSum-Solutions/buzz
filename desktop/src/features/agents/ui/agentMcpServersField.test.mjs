import assert from "node:assert/strict";
import test from "node:test";

import { supportBadge } from "./AgentMcpServersField";
import { serverSupport } from "@/features/settings/ui/mcpRegistryLogic";

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
};

const CLAUDE = {
  id: "claude",
  label: "Claude",
  mcpTransports: ["stdio", "http"],
};

test("a usable stdio entry on a stdio runtime gets no badge", () => {
  assert.equal(supportBadge(serverSupport(entry(), BUZZ_AGENT)), null);
});

test("an http entry on a buzz-agent runtime is badged Unsupported", () => {
  const http = entry({ id: "remote", name: "remote", transport: "http" });
  const badge = supportBadge(serverSupport(http, BUZZ_AGENT));
  assert.deepEqual(badge, { label: "Unsupported", tone: "warn" });
});

test("the same http entry on a runtime whose catalog declares http is not badged", () => {
  const http = entry({ id: "remote", name: "remote", transport: "http" });
  assert.equal(supportBadge(serverSupport(http, CLAUDE)), null);
});

test("a loader-rejected entry is badged Disabled whatever the runtime", () => {
  const rejected = entry({ rejection: "its command is not an absolute path" });
  assert.deepEqual(supportBadge(serverSupport(rejected, CLAUDE)), {
    label: "Disabled",
    tone: "warn",
  });
});

test("an agent whose harness the registry cannot configure is badged, not hidden", () => {
  assert.deepEqual(supportBadge(serverSupport(entry(), null)), {
    label: "Not configurable",
    tone: "warn",
  });
});
