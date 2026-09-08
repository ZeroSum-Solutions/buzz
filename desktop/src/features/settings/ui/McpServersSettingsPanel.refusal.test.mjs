/**
 * [PRIOR F9] regression: `save_mcp_registry_server` can return
 * `Ok(McpRegistryView)` whose `refused` array names an agent the save could
 * not converge — the document write succeeded, but a real backend answers
 * every following `list_mcp_registry_servers` call with an EMPTY `refused`
 * array (that command's own contract; it never recomputes refusals). The
 * Settings save path must render the save mutation's own response, not the
 * post-invalidate refetch, or the refusal disappears the instant the panel
 * revalidates its query.
 *
 * Mutation proof: reverting `McpServersSettingsPanel.tsx`'s `save` mutation to
 * discard its response and rely on `registry.data?.refused` after
 * `invalidate()` turns this RED — the refusal renders, then vanishes once the
 * mocked `list_mcp_registry_servers` refetch (which always answers `refused:
 * []`, matching the real command) resolves.
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

function registryView(refused) {
  return {
    servers: [SERVER],
    document_path: "/test/doc.json",
    refused,
  };
}

test("a save whose response refuses an agent keeps that refusal visible through the post-save refetch", async () => {
  const { render, screen, fireEvent, waitFor } = await import(
    "@testing-library/react"
  );
  const React = await import("react");
  const { QueryClient, QueryClientProvider } = await import(
    "@tanstack/react-query"
  );
  const { McpServersSettingsPanel } = await import(
    "./McpServersSettingsPanel.tsx"
  );

  invokeHandler = (command) => {
    if (command === "save_mcp_registry_server") {
      return Promise.resolve(
        registryView([
          ["buzz-agent", "buzz-agent cannot use srv-1 over stdio"],
        ]),
      );
    }
    // The real command never recomputes refusals; a plain list always answers
    // with an empty array. The test must prove the panel does not depend on
    // this call for what it renders after a save.
    if (command === "list_mcp_registry_servers") {
      return Promise.resolve(registryView([]));
    }
    return Promise.reject(new Error(`unmocked: ${command}`));
  };

  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  queryClient.setQueryData(["mcp-registry"], registryView([]));

  render(
    React.createElement(
      QueryClientProvider,
      { client: queryClient },
      React.createElement(McpServersSettingsPanel),
    ),
  );

  fireEvent.click(screen.getByRole("button", { name: "Edit" }));
  fireEvent.click(screen.getByRole("button", { name: "Review" }));
  fireEvent.click(screen.getByRole("button", { name: "Approve and save" }));

  await waitFor(() => {
    const alert = document.querySelector(
      '[data-testid="mcp-registry-refusals"]',
    );
    assert.ok(alert, "the save response's refusal must render after save");
    assert.match(alert.textContent, /buzz-agent cannot use srv-1 over stdio/);
  });

  // Let the invalidate-triggered refetch (answering `refused: []`, as the
  // real list command always does) resolve, then prove the refusal survives
  // it — it must come from the save response, not the query cache.
  await new Promise((resolve) => setTimeout(resolve, 20));
  await waitFor(() => {
    const alert = document.querySelector(
      '[data-testid="mcp-registry-refusals"]',
    );
    assert.ok(
      alert,
      "the refusal must still be visible after the post-save refetch overwrites the query cache's own (always-empty) refused array",
    );
    assert.match(alert.textContent, /buzz-agent cannot use srv-1 over stdio/);
  });
});
