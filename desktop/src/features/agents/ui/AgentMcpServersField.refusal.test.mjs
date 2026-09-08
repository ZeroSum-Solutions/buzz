/**
 * [PRIOR F9] regression: `set_agent_mcp_servers` can return `Ok(McpRegistryView)`
 * whose `refused` array names THIS agent — the selection was persisted, but the
 * generation it adopted could not include the server for this agent. `apply()`
 * must inspect that response and keep the refusal visible, not call
 * `setRefusal(null)` on every response that did not throw.
 *
 * Mutation proof: reverting `AgentMcpServersField.tsx`'s `apply` to
 * `setRefusal(null)` unconditionally after a successful `setAgentMcpServers`
 * call turns this RED — the toggle would clear the refusal alert instead of
 * rendering the reason the response carried for this agent's own pubkey.
 */

import assert from "node:assert/strict";
import { after, afterEach, before, test } from "node:test";

import { JSDOM } from "jsdom";

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});

before(() => {
  Object.assign(globalThis, {
    document: dom.window.document,
    HTMLElement: dom.window.HTMLElement,
    IS_REACT_ACT_ENVIRONMENT: true,
    window: dom.window,
  });
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    value: dom.window.navigator,
  });
});

after(() => dom.window.close());

afterEach(async () => {
  const { cleanup } = await import("@testing-library/react");
  cleanup();
});

let invokeHandler = () => Promise.reject(new Error("unmocked invoke"));

globalThis.__TAURI_INTERNALS__ = {
  invoke: (command, args) => invokeHandler(command, args),
  transformCallback: () => 1,
};
dom.window.__TAURI_INTERNALS__ = globalThis.__TAURI_INTERNALS__;

const PUBKEY = "pk-agent-1";

const SERVER = {
  id: "srv-1",
  name: "srv-1",
  transport: "stdio",
  command: "/usr/local/bin/fake-mcp",
  args: [],
  url: null,
  auth_scheme: null,
  env: [],
  rejection: null,
};

const RUNTIME = {
  id: "buzz-agent",
  label: "Buzz Agent",
  mcpTransports: ["stdio"],
  mcpRegistryAvailable: true,
};

test("a toggle whose response refuses this agent renders the refusal, not a cleared alert", async () => {
  const { render, screen, fireEvent, waitFor } = await import(
    "@testing-library/react"
  );
  const React = await import("react");
  const { QueryClient, QueryClientProvider } = await import(
    "@tanstack/react-query"
  );
  const { AgentMcpServersField } = await import("./AgentMcpServersField.tsx");

  invokeHandler = (command) => {
    if (command === "set_agent_mcp_servers") {
      return Promise.resolve({
        servers: [SERVER],
        document_path: "/test/doc.json",
        refused: [[PUBKEY, "buzz-agent cannot run srv-1 over stdio"]],
      });
    }
    // The subsequent invalidate-triggered refetch. The real command never
    // recomputes refusals from stored state, so it always answers empty.
    if (command === "list_mcp_registry_servers") {
      return Promise.resolve({
        servers: [SERVER],
        document_path: "/test/doc.json",
        refused: [],
      });
    }
    return Promise.reject(new Error(`unmocked: ${command}`));
  };

  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  queryClient.setQueryData(["mcp-registry"], {
    servers: [SERVER],
    document_path: "/test/doc.json",
    refused: [],
  });

  render(
    React.createElement(
      QueryClientProvider,
      { client: queryClient },
      React.createElement(AgentMcpServersField, {
        selection: { status: "loaded", enabled: [] },
        onEnabledChange: () => {},
        pubkey: PUBKEY,
        runtime: RUNTIME,
      }),
    ),
  );

  const toggle = screen.getByRole("switch", {
    name: `Enable ${SERVER.name}`,
  });
  fireEvent.click(toggle);

  await waitFor(() => {
    const alert = document.querySelector(
      '[data-testid="agent-mcp-servers-refusal"]',
    );
    assert.ok(
      alert,
      "a refusal named for this agent in a successful response must render",
    );
    assert.match(alert.textContent, /buzz-agent cannot run srv-1 over stdio/);
  });
});

test("a toggle whose response refuses a DIFFERENT agent does not render a refusal here", async () => {
  const { render, screen, fireEvent, waitFor } = await import(
    "@testing-library/react"
  );
  const React = await import("react");
  const { QueryClient, QueryClientProvider } = await import(
    "@tanstack/react-query"
  );
  const { AgentMcpServersField } = await import("./AgentMcpServersField.tsx");

  let setCallSettled = false;
  invokeHandler = (command) => {
    if (command === "set_agent_mcp_servers") {
      return Promise.resolve({
        servers: [SERVER],
        document_path: "/test/doc.json",
        refused: [["some-other-agent", "unrelated refusal"]],
      }).then((view) => {
        setCallSettled = true;
        return view;
      });
    }
    if (command === "list_mcp_registry_servers") {
      return Promise.resolve({
        servers: [SERVER],
        document_path: "/test/doc.json",
        refused: [],
      });
    }
    return Promise.reject(new Error(`unmocked: ${command}`));
  };

  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  queryClient.setQueryData(["mcp-registry"], {
    servers: [SERVER],
    document_path: "/test/doc.json",
    refused: [],
  });

  render(
    React.createElement(
      QueryClientProvider,
      { client: queryClient },
      React.createElement(AgentMcpServersField, {
        selection: { status: "loaded", enabled: [] },
        onEnabledChange: () => {},
        pubkey: PUBKEY,
        runtime: RUNTIME,
      }),
    ),
  );

  const toggle = screen.getByRole("switch", {
    name: `Enable ${SERVER.name}`,
  });
  fireEvent.click(toggle);

  // Wait for the mocked set_agent_mcp_servers call to resolve, then give the
  // apply() continuation a turn to finish updating refusal state from it.
  await waitFor(() => {
    assert.equal(setCallSettled, true);
  });
  await new Promise((resolve) => setTimeout(resolve, 0));

  const alert = document.querySelector(
    '[data-testid="agent-mcp-servers-refusal"]',
  );
  assert.equal(
    alert,
    null,
    "a refusal naming a different agent must not surface on this field",
  );
});
