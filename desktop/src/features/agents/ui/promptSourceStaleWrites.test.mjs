/**
 * The two ways a prompt reload used to leave the wrong instructions behind.
 * Both are about state that outlives the request, so both drive the real
 * `AgentDefinitionDialog` and a real React Query client rather than the field
 * in isolation — the field alone cannot show either.
 *
 *   1. **A late answer landing on another agent.** The dialog is persistent:
 *      it keeps the prompt-source field across agents and re-mounts it on the
 *      next open. A reload started on agent A and answered after the dialog
 *      re-opened on agent B wrote A's file text into B's instructions
 *      textarea, where Save persisted it onto B. The reload is fenced by
 *      generation, definition id and mount, so the late answer is dropped.
 *
 *   2. **A cache the reload made stale.** The reload writes `system_prompt` on
 *      disk through the ordinary persona update path, and the backend command
 *      emits no `agents-data-changed`. Nothing invalidated `personas`, so the
 *      cached copy kept the pre-reload prompt for its 30s `staleTime`, the next
 *      `openEdit` seeded the dialog from that stale copy, and saving any
 *      unrelated field resubmitted the old instructions over the new ones. The
 *      reload now runs through a mutation that invalidates the same keys a
 *      typed edit does.
 *
 *   3. **An edit typed into the dialog while the reload was in flight.** The
 *      field disabled only its own controls, so the instructions textarea and
 *      Save stayed live through the round trip. The late answer overwrote what
 *      was typed, and a Save in the same window submitted the pre-reload draft
 *      through `update_persona`, which carries no precondition — the reload's
 *      write lost with no error anywhere. The field now reports its pending
 *      state up and the dialog fences both controls.
 *
 *   4. **The community catalog caches.** A reload is a save-and-publish: it
 *      awaits the relay and refreshes the affected team catalog heads. Live
 *      delivery normally covers the catalog views, but `subscribeLive`'s
 *      failure path only logs, so a device whose subscription never
 *      established showed the pre-reload prompt until the 20-minute poll.
 *
 * Mutation proofs: removing either fence check from the field's `run` → test 1
 * fails with A's text in B's textarea; removing the `onSettled` invalidation
 * from `useSetPromptSourceMutation` → test 2 fails with the stale prompt in the
 * submitted payload; dropping `isPromptSourcePending` from the dialog's
 * `Textarea`/`canSubmit` → test 3 fails with both controls live; dropping the
 * catalog keys from `onSettled` → test 4 fails with both caches still fresh.
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

/**
 * A stand-in for the definitions on disk. `list_personas` reads it, so a
 * reload that changes it is only visible to the UI once something invalidates
 * the query — which is exactly what test 2 is about.
 */
let backendPersonas = [];
/** Resolve for an in-flight `set_prompt_source_and_reload`, when held open. */
let holdReload = null;
let releaseReload = null;

const rawPersona = (id, systemPrompt) => ({
  id,
  display_name: id.toUpperCase(),
  avatar_url: null,
  system_prompt: systemPrompt,
  is_builtin: false,
  created_at: "2026-09-04T00:00:00Z",
  updated_at: "2026-09-04T00:00:00Z",
});

globalThis.__TAURI_INTERNALS__ = {
  invoke: (cmd) => {
    if (cmd === "set_prompt_source_and_reload") {
      if (holdReload) {
        return new Promise((resolve) => {
          releaseReload = resolve;
        });
      }
      return Promise.resolve({ localUpdated: true });
    }
    if (cmd === "list_personas") return Promise.resolve(backendPersonas);
    if (cmd === "get_prompt_source") return Promise.resolve(null);
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
let editPersonaDialogState, usePersonasQuery;

before(async () => {
  ({ act, render, screen, cleanup, fireEvent } = await import(
    "@testing-library/react"
  ));
  ({ createElement } = await import("react"));
  ({ AgentDefinitionDialog } = await import("./AgentDefinitionDialog.tsx"));
  ({ editPersonaDialogState } = await import("./personaDialogState.ts"));
  ({ usePersonasQuery } = await import("../hooks.ts"));
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
  backendPersonas = [];
  holdReload = null;
  releaseReload = null;
});

after(() => dom.window.close());

/** Let queries, mutations and their invalidations run to a standstill. */
async function settle() {
  for (let round = 0; round < 6; round += 1) {
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 0));
    });
  }
}

function newClient(queryOverrides = {}) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0, ...queryOverrides } },
  });
  clients.push(client);
  return client;
}

const dialogProps = (initialValues, extra = {}) => ({
  description: "Edit the agent definition.",
  embedded: true,
  error: null,
  initialValues,
  isPending: false,
  onOpenChange: () => {},
  onSubmit: async () => {},
  open: initialValues !== null,
  runtimes: [],
  runtimesLoading: false,
  submitLabel: "Save",
  title: "Edit agent",
  ...extra,
});

const instructions = () => screen.getByLabelText(/Agent instructions/i);

test("a reload answered after the dialog moved to another agent is dropped", async () => {
  const client = newClient();
  const tree = (initialValues) =>
    createElement(
      QueryClientProvider,
      { client },
      createElement(AgentDefinitionDialog, dialogProps(initialValues)),
    );

  holdReload = true;
  let rerender;
  await act(async () => {
    ({ rerender } = render(
      tree({ id: "pm", displayName: "PM", systemPrompt: "A instructions." }),
    ));
  });

  await act(async () => {
    fireEvent.change(screen.getByLabelText(/Instructions file/i), {
      target: { value: "/Users/me/agent-prompts/pm.md" },
    });
  });
  await act(async () => {
    fireEvent.click(screen.getByRole("button", { name: "Reload" }));
  });
  assert.ok(releaseReload, "the reload must still be in flight");

  // The operator closes A's dialog and opens B's. The dialog is persistent, so
  // the parent stays mounted the whole time.
  await act(async () => rerender(tree(null)));
  await act(async () =>
    rerender(
      tree({ id: "qa", displayName: "QA", systemPrompt: "B instructions." }),
    ),
  );
  assert.equal(
    instructions().value,
    "B instructions.",
    "B's dialog starts on B's own prompt",
  );

  // A's reload returns now, for an agent this dialog is no longer showing.
  await act(async () => {
    releaseReload({
      localUpdated: true,
      publish: "published",
      binding: { path: "/Users/me/agent-prompts/pm.md", inSync: true },
      prompt: "A reloaded text.",
    });
    await Promise.resolve();
  });

  assert.equal(
    instructions().value,
    "B instructions.",
    "A's late reload must not replace B's instructions, which Save would persist onto B",
  );
});

test("a reload refreshes the cache the next open seeds the dialog from", async () => {
  backendPersonas = [rawPersona("pm", "Old instructions.")];
  const client = newClient();

  // The Agents view: a live personas query beside the open dialog, exactly as
  // the app has it. `openEdit` snapshots a persona out of this query's data,
  // so whatever it holds when the dialog re-opens is what Save submits.
  let personas = null;
  function Harness({ initialValues, onSubmit }) {
    personas = usePersonasQuery();
    return createElement(
      AgentDefinitionDialog,
      dialogProps(initialValues, { onSubmit }),
    );
  }

  const submitted = [];
  const tree = (initialValues) =>
    createElement(
      QueryClientProvider,
      { client },
      createElement(Harness, {
        initialValues,
        onSubmit: async (payload) => {
          submitted.push(payload);
        },
      }),
    );

  let rerender;
  await act(async () => {
    ({ rerender } = render(tree(null)));
  });
  await settle();
  assert.equal(
    personas.data?.[0]?.systemPrompt,
    "Old instructions.",
    "the cache starts on the stored prompt",
  );

  // `openEdit` snapshots the persona out of the query's data; the dialog seeds
  // itself from that snapshot and never re-reads it while it is open.
  await act(async () =>
    rerender(
      tree(editPersonaDialogState(personaFromCache("pm")).initialValues),
    ),
  );

  // The reload lands on disk, as the backend command does.
  backendPersonas = [rawPersona("pm", "Reloaded from the file.")];
  await act(async () => {
    fireEvent.change(screen.getByLabelText(/Instructions file/i), {
      target: { value: "/Users/me/agent-prompts/pm.md" },
    });
  });
  await act(async () => {
    fireEvent.click(screen.getByRole("button", { name: "Reload" }));
  });
  await settle();

  assert.equal(
    personas.data?.[0]?.systemPrompt,
    "Reloaded from the file.",
    "the reload must invalidate the personas cache, or every later read of it is stale",
  );

  // Close, re-open from the cache the way `openEdit` does, edit an unrelated
  // field and save.
  await act(async () => rerender(tree(null)));
  await act(async () =>
    rerender(
      tree(editPersonaDialogState(personaFromCache("pm")).initialValues),
    ),
  );
  await act(async () => {
    fireEvent.change(screen.getByLabelText(/Agent name/i), {
      target: { value: "PM renamed" },
    });
  });
  await act(async () => {
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
  });

  assert.equal(submitted.length, 1, "the unrelated edit is saved once");
  assert.equal(
    submitted[0].systemPrompt,
    "Reloaded from the file.",
    "saving an unrelated field must not resubmit the prompt the reload replaced",
  );

  function personaFromCache(id) {
    const found = personas?.data?.find((persona) => persona.id === id);
    assert.ok(found, `the cache must hold ${id}`);
    return found;
  }
});

test("the instructions and Save are fenced while a reload is in flight", async () => {
  const client = newClient();
  holdReload = true;

  await act(async () => {
    render(
      createElement(
        QueryClientProvider,
        { client },
        createElement(
          AgentDefinitionDialog,
          dialogProps({
            id: "pm",
            displayName: "PM",
            systemPrompt: "Old instructions.",
          }),
        ),
      ),
    );
  });

  const submit = () => screen.getByTestId("persona-dialog-submit");
  assert.equal(
    instructions().disabled,
    false,
    "the instructions are editable before a reload starts",
  );
  assert.equal(submit().disabled, false, "and so is Save");

  await act(async () => {
    fireEvent.change(screen.getByLabelText(/Instructions file/i), {
      target: { value: "/Users/me/agent-prompts/pm.md" },
    });
  });
  await act(async () => {
    fireEvent.click(screen.getByRole("button", { name: "Reload" }));
  });
  assert.ok(releaseReload, "the reload must still be in flight");

  // The window the fence exists for: the answer will replace the instructions
  // outright, so anything typed here is discarded, and a Save here submits the
  // pre-reload draft through a command with no precondition — last writer wins.
  assert.equal(
    instructions().disabled,
    true,
    "typing during the round trip would be silently discarded by the answer",
  );
  assert.equal(
    submit().disabled,
    true,
    "saving during the round trip would overwrite the reload with the old draft",
  );

  await act(async () => {
    releaseReload({
      localUpdated: true,
      publish: "published",
      binding: { path: "/Users/me/agent-prompts/pm.md", inSync: true },
      prompt: "Reloaded from the file.",
    });
    await Promise.resolve();
  });
  await settle();

  assert.equal(
    instructions().value,
    "Reloaded from the file.",
    "the file's text lands once the answer arrives",
  );
  assert.equal(
    instructions().disabled,
    false,
    "and the fence lifts, or the dialog is stuck read-only",
  );
  assert.equal(submit().disabled, false, "Save is usable again");
});

test("a reload refreshes the community catalog caches it just changed", async () => {
  // A retained cache: the default `gcTime: 0` would evict an observer-less
  // entry the instant it is seeded, and there would be nothing left to
  // invalidate.
  const client = newClient({ gcTime: 5 * 60_000 });
  // Two catalog caches with data and no mounted reader — the state of a device
  // whose live subscription never established. Nothing else will refresh them.
  const personaCatalogKey = ["persona-catalog", "community-1"];
  const teamCatalogKey = ["team-catalog", "community-1"];
  client.setQueryData(personaCatalogKey, []);
  client.setQueryData(teamCatalogKey, []);

  await act(async () => {
    render(
      createElement(
        QueryClientProvider,
        { client },
        createElement(
          AgentDefinitionDialog,
          dialogProps({
            id: "pm",
            displayName: "PM",
            systemPrompt: "Old instructions.",
          }),
        ),
      ),
    );
  });

  await act(async () => {
    fireEvent.change(screen.getByLabelText(/Instructions file/i), {
      target: { value: "/Users/me/agent-prompts/pm.md" },
    });
  });
  await act(async () => {
    fireEvent.click(screen.getByRole("button", { name: "Reload" }));
  });
  await settle();

  assert.equal(
    client.getQueryState(personaCatalogKey)?.isInvalidated,
    true,
    "the reload publishes a new persona head, so the community catalog is stale",
  );
  assert.equal(
    client.getQueryState(teamCatalogKey)?.isInvalidated,
    true,
    "and the backend refreshed every team catalog head that lists this agent",
  );
});
