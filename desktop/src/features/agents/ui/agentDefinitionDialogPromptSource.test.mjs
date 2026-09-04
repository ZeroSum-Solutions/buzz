/**
 * The prompt-source field inside its real parent, AgentDefinitionDialog.
 *
 * `PromptSourceField.test.mjs` holds the field's own button behaviour. This
 * mounts the production dialog and holds the wiring only the parent can show:
 *
 *   - the field appears in edit mode (an `initialValues` with an `id`) and not
 *     in create mode, because a reload writes to a stored definition;
 *   - a reload replaces the text in the dialog's own "Agent instructions"
 *     textarea — the propagation the feature exists for;
 *   - Clear leaves that textarea alone;
 *   - re-opening the dialog on a bound definition seeds the path field from
 *     the sidecar, so the binding survives a close and is not retyped;
 *   - the two directions of the dialog's own dirty flag, which decides whether
 *     a save republishes the community catalog head: a reload must not drop an
 *     unsaved edit to another field out of that publish, and typing the
 *     machine-local path must not arm one.
 *
 * Mutation proofs: dropping the seed effect from the field → the re-open
 * assertion fails; dropping `setSystemPrompt` from the dialog's
 * `onPromptReloaded` → the propagation assertion fails; rendering the field
 * unconditionally → the create-mode assertion fails; restoring
 * `setHasUserChanges(false)` to `onPromptReloaded` → the reload-keeps-edits
 * assertion fails; dropping the dirty-exempt check from the form's
 * `onChangeCapture` → the typing-a-path assertion fails.
 */

import assert from "node:assert/strict";
import { after, afterEach, before, test } from "node:test";
import { JSDOM } from "jsdom";

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});

Object.assign(globalThis, {
  document: dom.window.document,
  window: dom.window,
  IS_REACT_ACT_ENVIRONMENT: true,
  localStorage: dom.window.localStorage,
  self: dom.window,
  ResizeObserver: class {
    observe() {}
    unobserve() {}
    disconnect() {}
  },
});
Object.defineProperty(globalThis, "navigator", {
  configurable: true,
  value: dom.window.navigator,
  writable: true,
});
dom.window.requestAnimationFrame = (cb) => setTimeout(cb, 0);
globalThis.requestAnimationFrame = dom.window.requestAnimationFrame;
dom.window.matchMedia ??= (query) => ({
  matches: false,
  media: query,
  onchange: null,
  addListener: () => {},
  removeListener: () => {},
  addEventListener: () => {},
  removeEventListener: () => {},
  dispatchEvent: () => false,
});
globalThis.matchMedia = dom.window.matchMedia;
for (const key of Object.getOwnPropertyNames(dom.window)) {
  if (key === "window" || key === "document" || key === "globalThis") continue;
  const value = dom.window[key];
  if (
    typeof value === "function" &&
    /^(HTML|SVG)|Element$|Event$|EventTarget$|^Node|^Document|Observer$/.test(
      key,
    )
  ) {
    globalThis[key] = value;
  }
}
globalThis.getComputedStyle = dom.window.getComputedStyle.bind(dom.window);

let promptSourceCalls = [];
let promptSourceResult = { localUpdated: true };
/** What `get_prompt_source` answers on open: a stored path, or `null`. */
let storedPromptSource = null;

globalThis.__TAURI_INTERNALS__ = {
  invoke: (cmd, payload) => {
    if (cmd === "set_prompt_source_and_reload") {
      promptSourceCalls.push(payload);
      return Promise.resolve(promptSourceResult);
    }
    if (cmd === "get_prompt_source") {
      return Promise.resolve(storedPromptSource);
    }
    if (cmd === "get_global_agent_config") {
      return Promise.resolve({
        env_vars: {},
        provider: null,
        model: null,
        preferred_runtime: null,
      });
    }
    if (cmd === "get_baked_build_env" || cmd === "get_baked_build_env_keys") {
      return Promise.resolve([]);
    }
    if (cmd === "discover_agent_models") {
      return Promise.resolve({ options: [], is_optional: true });
    }
    if (cmd === "get_runtime_file_config") return Promise.resolve(null);
    return Promise.resolve(null);
  },
  transformCallback: () => 1,
};
dom.window.__TAURI_INTERNALS__ = globalThis.__TAURI_INTERNALS__;

const clients = [];
let act, render, screen, cleanup, fireEvent, createElement;
let AgentDefinitionDialog, QueryClient, QueryClientProvider;

before(async () => {
  ({ act, render, screen, cleanup, fireEvent } = await import(
    "@testing-library/react"
  ));
  ({ createElement } = await import("react"));
  ({ AgentDefinitionDialog } = await import("./AgentDefinitionDialog.tsx"));
  ({ QueryClient, QueryClientProvider } = await import(
    "@tanstack/react-query"
  ));
});

afterEach(() => {
  cleanup?.();
  for (const client of clients.splice(0)) {
    client.cancelQueries();
    client.clear();
  }
  promptSourceCalls = [];
  promptSourceResult = { localUpdated: true };
  storedPromptSource = null;
});

after(() => dom.window.close());

function mount(initialValues, overrides = {}) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0 } },
  });
  clients.push(client);
  render(
    createElement(
      QueryClientProvider,
      { client },
      createElement(AgentDefinitionDialog, {
        description: "Edit the agent definition.",
        embedded: true,
        error: null,
        initialValues,
        isPending: false,
        onOpenChange: () => {},
        onSubmit: async () => {},
        open: true,
        runtimes: [],
        runtimesLoading: false,
        submitLabel: "Save",
        title: "Edit agent",
        ...overrides,
      }),
    ),
  );
}

const editValues = {
  id: "pm",
  displayName: "PM",
  systemPrompt: "Old instructions.",
};

const instructions = () => screen.getByLabelText(/Agent instructions/i);
const promptSourceInput = () => screen.getByLabelText(/Instructions file/i);

test("the instructions-file field is edit-mode only", async () => {
  await act(async () => mount({ displayName: "New", systemPrompt: "" }));
  // Compared as a boolean: node:assert would try to render a whole JSDOM node
  // into the failure message, which does not terminate.
  assert.ok(
    screen.queryByLabelText(/Instructions file/i) === null,
    "create mode has no stored definition to reload into",
  );

  cleanup();
  await act(async () => mount(editValues));
  assert.ok(promptSourceInput(), "edit mode offers the field");
});

test("re-opening the dialog shows the file already bound to the agent", async () => {
  // The binding this dialog wrote in an earlier session. Without the read the
  // sidecar would be write-only and the operator would retype the whole
  // absolute path on every reload.
  storedPromptSource = "/Users/me/agent-prompts/pm.md";
  const dirtyReports = [];
  await act(async () =>
    mount(editValues, {
      onDirtyChange: (dirty) => dirtyReports.push(dirty),
      publishCatalogUpdatesOnSave: true,
    }),
  );

  assert.deepEqual(
    dirtyReports.filter(Boolean),
    [],
    "seeding the field is not an edit and must not arm a catalog publish",
  );
  assert.equal(
    promptSourceInput().value,
    "/Users/me/agent-prompts/pm.md",
    "the stored binding must seed the field when the dialog re-opens",
  );
  assert.equal(
    screen.getByRole("button", { name: "Reload" }).disabled,
    false,
    "the seeded path is reloadable with one click",
  );
  assert.deepEqual(
    promptSourceCalls,
    [],
    "seeding reads the binding; it does not reload the file",
  );
});

test("a reload replaces the dialog's instructions text", async () => {
  promptSourceResult = {
    localUpdated: true,
    publish: "published",
    path: "/Users/me/agent-prompts/pm.md",
    prompt: "Ship the roadmap.\n",
  };
  await act(async () => mount(editValues));
  assert.equal(instructions().value, "Old instructions.");

  await act(async () => {
    fireEvent.change(promptSourceInput(), {
      target: { value: "/Users/me/agent-prompts/pm.md" },
    });
  });
  await act(async () => {
    fireEvent.click(screen.getByRole("button", { name: "Reload" }));
  });

  assert.deepEqual(promptSourceCalls, [
    { definitionId: "pm", path: "/Users/me/agent-prompts/pm.md" },
  ]);
  assert.equal(
    instructions().value,
    "Ship the roadmap.\n",
    "the file's text must land in the instructions the dialog shows",
  );
});

test("Clear unbinds without touching the instructions text", async () => {
  promptSourceResult = { localUpdated: false };
  await act(async () => mount(editValues));

  await act(async () => {
    fireEvent.click(screen.getByRole("button", { name: "Clear" }));
  });

  assert.deepEqual(promptSourceCalls, [{ definitionId: "pm", path: null }]);
  assert.equal(instructions().value, "Old instructions.");
});

test("a reload keeps the dialog's other unsaved edits armed for publish", async () => {
  // A shared agent: the save republishes the catalog head only when the dialog
  // reports user changes. The reload's own publish carries the definition as it
  // sits on disk, so it cannot stand in for an edit made in this dialog.
  promptSourceResult = {
    localUpdated: true,
    publish: "published",
    path: "/Users/me/agent-prompts/pm.md",
    prompt: "Ship the roadmap.\n",
  };
  const submissions = [];
  await act(async () =>
    mount(editValues, {
      onSubmit: async (input, options) => {
        submissions.push({ input, options });
      },
      publishCatalogUpdatesOnSave: true,
    }),
  );

  await act(async () => {
    fireEvent.change(screen.getByLabelText(/Agent name/i), {
      target: { value: "Product Manager" },
    });
  });
  await act(async () => {
    fireEvent.change(promptSourceInput(), {
      target: { value: "/Users/me/agent-prompts/pm.md" },
    });
  });
  await act(async () => {
    fireEvent.click(screen.getByRole("button", { name: "Reload" }));
  });

  assert.ok(
    screen.queryByTestId("persona-dialog-catalog-publish-notice"),
    "the unsaved name edit still publishes on save after a reload",
  );

  await act(async () => {
    fireEvent.click(screen.getByTestId("persona-dialog-submit"));
  });

  assert.equal(submissions.length, 1);
  assert.equal(submissions[0].input.displayName, "Product Manager");
  assert.equal(
    submissions[0].options.publishCatalogUpdates,
    true,
    "clearing the dialog's dirty flag on reload would strand the name edit locally",
  );
});

test("typing an instructions-file path does not arm a catalog publish", async () => {
  const submissions = [];
  const dirtyReports = [];
  await act(async () =>
    mount(editValues, {
      onDirtyChange: (dirty) => dirtyReports.push(dirty),
      onSubmit: async (input, options) => {
        submissions.push({ input, options });
      },
      publishCatalogUpdatesOnSave: true,
    }),
  );

  await act(async () => {
    fireEvent.change(promptSourceInput(), {
      target: { value: "/Users/me/agent-prompts/pm.md" },
    });
  });

  assert.ok(
    screen.queryByTestId("persona-dialog-catalog-publish-notice") === null,
    "the path is machine-local and is never part of the published head",
  );
  assert.deepEqual(
    dirtyReports.filter(Boolean),
    [],
    "a machine-local keystroke must not report the dialog dirty",
  );

  await act(async () => {
    fireEvent.click(screen.getByTestId("persona-dialog-submit"));
  });

  assert.equal(submissions.length, 1);
  assert.equal(submissions[0].options.publishCatalogUpdates, false);
});
