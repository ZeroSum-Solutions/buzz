import assert from "node:assert/strict";
import test from "node:test";

import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import {
  AgentMcpServersField,
  isSelectionSwitchDisabled,
  supportBadge,
} from "./AgentMcpServersField";
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
  mcpRegistryAvailable: true,
};

const CLAUDE = {
  id: "claude",
  label: "Claude",
  mcpTransports: ["stdio", "http"],
  mcpRegistryAvailable: true,
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

test("isSelectionSwitchDisabled returns true for loading or error and false for loaded", () => {
  assert.equal(isSelectionSwitchDisabled({ status: "loading" }), true);
  assert.equal(
    isSelectionSwitchDisabled({ status: "error", error: "boom" }),
    true,
  );
  assert.equal(
    isSelectionSwitchDisabled({ status: "loaded", enabled: [] }),
    false,
  );
});

test("switches are disabled in the DOM when selection state is loading or error", () => {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  queryClient.setQueryData(["mcp-registry"], {
    servers: [entry()],
    document_path: "/test/doc.json",
    refused: [],
  });

  const renderField = (selection) =>
    renderToStaticMarkup(
      React.createElement(
        QueryClientProvider,
        { client: queryClient },
        React.createElement(AgentMcpServersField, {
          selection,
          onEnabledChange: () => {},
          pubkey: "pk1",
          runtime: BUZZ_AGENT,
        }),
      ),
    );

  const loadingHtml = renderField({ status: "loading" });
  assert.match(loadingHtml, /data-disabled/);

  const errorHtml = renderField({ status: "error", error: "network failed" });
  assert.match(errorHtml, /data-disabled/);
  assert.match(errorHtml, /network failed/);

  const loadedHtml = renderField({ status: "loaded", enabled: [] });
  assert.doesNotMatch(loadedHtml, /data-disabled/);
});
