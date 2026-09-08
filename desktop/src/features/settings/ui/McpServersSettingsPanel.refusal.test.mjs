/** Refusals survive authoritative refetch and clear when convergence resolves. */

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

const queryClients = new Set();

afterEach(async () => {
  const { cleanup } = await import("@testing-library/react");
  cleanup();
  for (const client of queryClients) {
    // QueryClient.clear removes mutation records but does not stop their GC timers.
    for (const mutation of client.getMutationCache().getAll())
      mutation.destroy();
    client.clear();
  }
  queryClients.clear();
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

  let saved = false;
  invokeHandler = (command) => {
    if (command === "save_mcp_registry_server") {
      saved = true;
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
      return Promise.resolve(
        registryView(
          saved
            ? [["buzz-agent", "buzz-agent cannot use srv-1 over stdio"]]
            : [],
        ),
      );
    }
    return Promise.reject(new Error(`unmocked: ${command}`));
  };

  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  queryClients.add(queryClient);
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

async function renderPanel(view = registryView([])) {
  const { render } = await import("@testing-library/react");
  const React = await import("react");
  const { QueryClient, QueryClientProvider } = await import(
    "@tanstack/react-query"
  );
  const { McpServersSettingsPanel } = await import(
    "./McpServersSettingsPanel.tsx"
  );
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  queryClients.add(client);
  client.setQueryData(["mcp-registry"], view);
  render(
    React.createElement(
      QueryClientProvider,
      { client },
      React.createElement(McpServersSettingsPanel),
    ),
  );
  return client;
}

test("stdio credentials can be entered and saved through the form", async () => {
  const { screen, fireEvent, waitFor } = await import("@testing-library/react");
  let saved;
  invokeHandler = (command, args) => {
    if (command === "save_mcp_registry_server") saved = args;
    return Promise.resolve(registryView([]));
  };
  await renderPanel();
  fireEvent.click(screen.getByRole("button", { name: "Edit" }));
  fireEvent.click(
    screen.getByRole("button", { name: "Add environment variable" }),
  );
  fireEvent.change(screen.getByLabelText("Variable name 1"), {
    target: { value: "API_KEY" },
  });
  fireEvent.change(screen.getByLabelText("Credential name 1"), {
    target: { value: "api-key" },
  });
  fireEvent.change(screen.getByLabelText("Credential value 1"), {
    target: { value: "test-secret-value" },
  });
  fireEvent.click(screen.getByRole("button", { name: "Review" }));
  assert.ok(
    !screen
      .getByTestId("mcp-server-approve")
      .textContent.includes("test-secret-value"),
  );
  fireEvent.click(screen.getByRole("button", { name: "Approve and save" }));
  await waitFor(() =>
    assert.deepEqual(saved?.entry.env, { API_KEY: "mcp:api-key" }),
  );
  assert.deepEqual(saved.secrets, { "api-key": "test-secret-value" });
});

test("switching stdio with env to HTTP clears hidden env and pending values", async () => {
  const { screen, fireEvent, waitFor } = await import("@testing-library/react");
  let saved;
  invokeHandler = (command, args) => {
    if (command === "save_mcp_registry_server") saved = args;
    return Promise.resolve(registryView([]));
  };
  await renderPanel({
    ...registryView([]),
    servers: [
      {
        ...SERVER,
        env: [{ name: "API_KEY", reference: "mcp:local", literal: null }],
      },
    ],
  });
  fireEvent.click(screen.getByRole("button", { name: "Edit" }));
  fireEvent.click(screen.getByRole("radio", { name: "HTTP endpoint" }));
  fireEvent.change(screen.getByLabelText("Upstream URL"), {
    target: { value: "https://example.com/mcp" },
  });
  fireEvent.click(screen.getByRole("button", { name: "Review" }));
  assert.ok(
    !screen.getByTestId("mcp-server-approve").textContent.includes("API_KEY"),
  );
  fireEvent.click(screen.getByRole("button", { name: "Approve and save" }));
  await waitFor(() => assert.deepEqual(saved?.entry.env, {}));
  assert.deepEqual(saved.secrets, {});
});

test("successful deletion clears an earlier refusal and refetch can resolve it", async () => {
  const { screen, fireEvent, waitFor, act } = await import(
    "@testing-library/react"
  );
  let current = registryView([]);
  invokeHandler = (command) => {
    if (command === "save_mcp_registry_server")
      current = registryView([["agent", "cannot use server"]]);
    if (command === "delete_mcp_registry_server")
      current = { ...registryView([]), servers: [] };
    return Promise.resolve(current);
  };
  const client = await renderPanel();
  fireEvent.click(screen.getByRole("button", { name: "Edit" }));
  fireEvent.click(screen.getByRole("button", { name: "Review" }));
  fireEvent.click(screen.getByRole("button", { name: "Approve and save" }));
  await waitFor(() => assert.ok(screen.queryByTestId("mcp-registry-refusals")));
  fireEvent.click(screen.getByRole("button", { name: "Delete srv-1" }));
  await waitFor(() =>
    assert.ok(
      screen.queryByTestId("mcp-registry-refusals") === null,
      "stale refusal must be cleared",
    ),
  );
  // An external mutation followed by an authoritative refetch can report or
  // clear a refusal too, without re-mounting Settings.
  await act(async () =>
    client.setQueryData(
      ["mcp-registry"],
      registryView([["agent", "new refusal"]]),
    ),
  );
  await waitFor(() => assert.ok(screen.queryByTestId("mcp-registry-refusals")));
  await act(async () =>
    client.setQueryData(["mcp-registry"], registryView([])),
  );
  await waitFor(() =>
    assert.ok(
      screen.queryByTestId("mcp-registry-refusals") === null,
      "stale refusal must be cleared",
    ),
  );
});

test("removing an environment row discards its unsaved credential", async () => {
  const { screen, fireEvent, waitFor } = await import("@testing-library/react");
  let saved;
  invokeHandler = (command, args) => {
    if (command === "save_mcp_registry_server") saved = args;
    return Promise.resolve(registryView([]));
  };
  await renderPanel();
  fireEvent.click(screen.getByRole("button", { name: "Edit" }));
  fireEvent.click(
    screen.getByRole("button", { name: "Add environment variable" }),
  );
  fireEvent.change(screen.getByLabelText("Variable name 1"), {
    target: { value: "API_KEY" },
  });
  fireEvent.change(screen.getByLabelText("Credential name 1"), {
    target: { value: "unused" },
  });
  fireEvent.change(screen.getByLabelText("Credential value 1"), {
    target: { value: "do-not-retain" },
  });
  fireEvent.click(screen.getByRole("button", { name: "Remove variable 1" }));
  fireEvent.click(screen.getByRole("button", { name: "Review" }));
  fireEvent.click(screen.getByRole("button", { name: "Approve and save" }));
  await waitFor(() => assert.ok(saved));
  assert.deepEqual(saved.secrets, {});
});
