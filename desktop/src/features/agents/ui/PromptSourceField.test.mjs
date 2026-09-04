/**
 * Behavioural tests for PromptSourceField, mounted for real in JSDOM and
 * driven through the production `setPromptSourceAndReload` → `invokeTauri`
 * path with only the Tauri IPC boundary stubbed.
 *
 * What they hold:
 *   - Reload is disabled until a path is typed, and while a reload is in
 *     flight.
 *   - Clicking Reload sends `set_prompt_source_and_reload` with the typed path
 *     and the definition id, once.
 *   - The reloaded text propagates to the dialog's instructions field
 *     (`onPromptReloaded`), and the resolved path replaces what was typed.
 *   - Clear is offered on a fresh mount even when nothing is bound, sends
 *     `path: null`, and empties the input.
 *   - A refused path is surfaced in the status line, not swallowed, and
 *     leaves the instructions field alone.
 *   - A reload whose sidecar write failed still reports the mapping error.
 *   - The stored binding seeds the field on open, so a re-opened dialog shows
 *     which file feeds the agent instead of an empty box.
 *   - The seed is fenced: an answer that lost the race to typing, or one for a
 *     definition the dialog has moved off, is discarded.
 *   - An unreadable sidecar is reported, never read as "nothing is bound",
 *     and arms the reset — the only control that can recover it, since Clear
 *     has to read the same file.
 *   - A binding whose prompt no longer matches renders as out of sync instead
 *     of repeating the claim.
 *
 * Mutation proofs: gating Clear on a seen binding again → the fresh-mount
 * Clear test fails; dropping the `onPromptReloaded` call → the propagation
 * test fails; swallowing the rejection in `run` → the error test fails;
 * dropping the seed effect → the seeding test fails; dropping either half of
 * the seed fence → the corresponding fence test fails.
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
  HTMLElement: dom.window.HTMLElement,
  Element: dom.window.Element,
  Node: dom.window.Node,
  getComputedStyle: dom.window.getComputedStyle,
  IS_REACT_ACT_ENVIRONMENT: true,
});

/** Every `set_prompt_source_and_reload` invocation, in order. */
let calls = [];
/** The next raw result (or Error to reject with) the stubbed command returns. */
let nextResult = { localUpdated: true };
/**
 * What `get_prompt_source` answers: a stored path, `null`, an `Error` to
 * reject with, or a function returning a promise (so a test can hold the
 * answer open and act while the seed is still in flight).
 */
let storedPromptSource = null;
/** Every `get_prompt_source` invocation, in order. */
let seedCalls = [];
/** Every `reset_prompt_sources` invocation, in order. */
let resetCalls = [];
/** What `reset_prompt_sources` answers. */
let nextResetResult = "/tmp/prompt-sources.corrupt.json";

/** A stored binding, in the shape the backend returns. */
const bound = (path, inSync = true) => ({ path, inSync });

globalThis.__TAURI_INTERNALS__ = {
  invoke: (cmd, payload) => {
    if (cmd === "get_prompt_source") {
      seedCalls.push(payload);
      if (typeof storedPromptSource === "function") {
        return storedPromptSource(payload);
      }
      return storedPromptSource instanceof Error
        ? Promise.reject(storedPromptSource)
        : Promise.resolve(storedPromptSource);
    }
    if (cmd === "reset_prompt_sources") {
      resetCalls.push(payload);
      return nextResetResult instanceof Error
        ? Promise.reject(nextResetResult)
        : Promise.resolve(nextResetResult);
    }
    if (cmd !== "set_prompt_source_and_reload") {
      return Promise.reject(new Error(`unmocked: ${cmd}`));
    }
    calls.push(payload);
    return nextResult instanceof Error
      ? Promise.reject(nextResult)
      : Promise.resolve(nextResult);
  },
  transformCallback: () => 1,
};
dom.window.__TAURI_INTERNALS__ = globalThis.__TAURI_INTERNALS__;

let act, render, screen, cleanup, fireEvent, createElement;
let PromptSourceField, QueryClient, QueryClientProvider;

before(async () => {
  ({ act, render, screen, cleanup, fireEvent } = await import(
    "@testing-library/react"
  ));
  ({ createElement } = await import("react"));
  ({ PromptSourceField } = await import("./PromptSourceField.tsx"));
  ({ QueryClient, QueryClientProvider } = await import(
    "@tanstack/react-query"
  ));
});

afterEach(() => {
  cleanup?.();
  calls = [];
  seedCalls = [];
  resetCalls = [];
  nextResult = { localUpdated: true };
  nextResetResult = "/tmp/prompt-sources.corrupt.json";
  storedPromptSource = null;
});

after(() => dom.window.close());

/**
 * Mount the field and collect every prompt handed back to the dialog.
 *
 * Awaited so the seed request the mount effect fires has settled before a test
 * asserts — except where a test deliberately holds it open.
 */
async function mount(overrides = {}) {
  const reloaded = [];
  const pendingReports = [];
  const props = () => ({
    definitionId: "pm",
    disabled: false,
    onPendingChange: (pending) => pendingReports.push(pending),
    onPromptReloaded: (prompt) => reloaded.push(prompt),
    ...overrides,
  });
  // The reload runs through a real React Query mutation, so the field needs a
  // real client: that is what invalidates the persona caches the reload just
  // made stale.
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0 } },
  });
  const tree = (extra = {}) =>
    createElement(
      QueryClientProvider,
      { client },
      createElement(PromptSourceField, { ...props(), ...extra }),
    );
  let rerender;
  await act(async () => {
    ({ rerender } = render(tree()));
  });
  return {
    pendingReports,
    reloaded,
    /** Re-render with a different definition id, as switching agents does. */
    async switchTo(definitionId) {
      await act(async () => {
        rerender(tree({ definitionId }));
      });
    },
  };
}

const input = () => screen.getByLabelText(/Instructions file/i);
const reloadButton = () => screen.getByRole("button", { name: "Reload" });
const clearButton = () => screen.getByRole("button", { name: "Clear" });
const status = () =>
  screen.getByText((_, node) => node?.id === "persona-prompt-source-status");

async function type(value) {
  await act(async () => {
    fireEvent.change(input(), { target: { value } });
  });
}

test("Reload is disabled until a path is typed", async () => {
  await mount();
  assert.equal(reloadButton().disabled, true);
  await type("   ");
  assert.equal(reloadButton().disabled, true);
  await type("/Users/me/agent-prompts/pm.md");
  assert.equal(reloadButton().disabled, false);
});

test("Reload sends the typed path once and propagates the reloaded text", async () => {
  nextResult = {
    localUpdated: true,
    publish: "published",
    binding: bound("/Users/me/agent-prompts/pm.md"),
    prompt: "Ship the roadmap.\n",
  };
  const { reloaded } = await mount();
  await type("~/../Users/me/agent-prompts/pm.md");
  await act(async () => {
    fireEvent.click(reloadButton());
  });

  assert.deepEqual(calls, [
    { definitionId: "pm", path: "~/../Users/me/agent-prompts/pm.md" },
  ]);
  assert.deepEqual(
    reloaded,
    ["Ship the roadmap.\n"],
    "the reloaded text must reach the dialog's instructions field",
  );
  assert.equal(
    input().value,
    "/Users/me/agent-prompts/pm.md",
    "the resolved path replaces what was typed",
  );
  assert.match(status().textContent, /reloaded/i);
});

test("Clear is offered on a fresh mount and unbinds without a stored path", async () => {
  nextResult = { localUpdated: false };
  const { reloaded } = await mount();

  // The seed found no binding, and Clear must still be offered: the seed can
  // be stale (another window bound a file since) or have failed outright.
  assert.equal(clearButton().disabled, false);
  await type("/Users/me/agent-prompts/pm.md");
  await act(async () => {
    fireEvent.click(clearButton());
  });

  assert.deepEqual(calls, [{ definitionId: "pm", path: null }]);
  assert.equal(input().value, "", "clearing empties the path field");
  assert.deepEqual(reloaded, [], "a clear leaves the instructions alone");
  assert.match(status().textContent, /unlinked/i);
});

test("a refused path is surfaced and the instructions are left alone", async () => {
  nextResult = new Error("Prompt file must live inside your home directory");
  const { reloaded } = await mount();
  await type("/etc/passwd");
  await act(async () => {
    fireEvent.click(reloadButton());
  });

  assert.match(status().textContent, /home directory/);
  assert.deepEqual(reloaded, []);
});

test("a reload whose mapping write failed still reports the mapping error", async () => {
  nextResult = {
    localUpdated: true,
    publish: "published",
    mappingError: "failed to read prompt-sources.json: Is a directory",
    prompt: "Ship the roadmap.\n",
  };
  const { reloaded } = await mount();
  await type("/Users/me/agent-prompts/pm.md");
  await act(async () => {
    fireEvent.click(reloadButton());
  });

  assert.deepEqual(reloaded, ["Ship the roadmap.\n"]);
  assert.match(status().textContent, /not remembered/i);
});

test("both buttons are disabled while the dialog is saving", async () => {
  await mount({ disabled: true });
  assert.equal(reloadButton().disabled, true);
  assert.equal(clearButton().disabled, true);
});

test("the stored binding seeds the field on open", async () => {
  storedPromptSource = bound("/Users/me/agent-prompts/pm.md");
  await mount();

  assert.deepEqual(
    seedCalls,
    [{ definitionId: "pm" }],
    "the field asks the backend which file is bound, once",
  );
  assert.equal(
    input().value,
    "/Users/me/agent-prompts/pm.md",
    "a re-opened dialog shows the bound path instead of an empty box",
  );
  assert.equal(
    reloadButton().disabled,
    false,
    "the seeded path is reloadable without retyping it",
  );
  assert.match(status().textContent, /loaded from \/Users\/me\/agent-prompts/);
});

test("no stored binding leaves the field empty and says so", async () => {
  storedPromptSource = null;
  await mount();

  assert.equal(input().value, "");
  assert.equal(reloadButton().disabled, true);
  assert.match(status().textContent, /file in your home folder/i);
});

test("a seed that lands after the operator types does not overwrite them", async () => {
  let release;
  storedPromptSource = () =>
    new Promise((resolve) => {
      release = () => resolve(bound("/Users/me/agent-prompts/stale.md"));
    });
  await mount();

  await type("/Users/me/agent-prompts/typed.md");
  await act(async () => {
    release();
  });

  assert.equal(
    input().value,
    "/Users/me/agent-prompts/typed.md",
    "a late seed must not clobber what the operator is typing",
  );
});

test("a seed for the previous definition is discarded after the dialog switches", async () => {
  const releases = [];
  storedPromptSource = (payload) =>
    new Promise((resolve) => {
      releases.push(() =>
        resolve(bound(`/Users/me/agent-prompts/${payload.definitionId}.md`)),
      );
    });
  const { switchTo } = await mount();
  await switchTo("designer");

  // The first definition's answer arrives last, and is now stale.
  await act(async () => {
    releases[1]();
    releases[0]();
  });

  assert.equal(
    input().value,
    "/Users/me/agent-prompts/designer.md",
    "the newest definition's binding wins regardless of arrival order",
  );
});

test("an unreadable sidecar is reported, not read as nothing bound", async () => {
  storedPromptSource = new Error(
    "failed to parse prompt-sources.json: expected value",
  );
  await mount();

  assert.match(status().textContent, /failed to parse prompt-sources\.json/);
  assert.equal(
    clearButton().disabled,
    false,
    "a failed seed must still leave a way to unbind",
  );

  // Clear cannot actually recover this state — it reads the same file — so the
  // field offers the reset, warns that it is not scoped to this agent, and the
  // reset is what makes the field usable again.
  const reset = screen.getByRole("button", {
    name: /Reset instructions-file settings/i,
  });
  assert.match(
    screen.getByText(/every agent on this machine/i).textContent,
    /renamed/i,
    "the warning must say the file is kept and every binding is affected",
  );
  await act(async () => {
    fireEvent.click(reset);
  });
  assert.deepEqual(resetCalls, [{}], "the reset runs its own command");
  assert.match(status().textContent, /reset/i);
  assert.equal(
    screen.queryByRole("button", {
      name: /Reset instructions-file settings/i,
    }),
    null,
    "once recovered, the machine-wide action is put away again",
  );
});

test("a binding whose prompt no longer matches renders as out of sync", async () => {
  storedPromptSource = bound("/Users/me/agent-prompts/pm.md", false);
  await mount();

  assert.match(
    status().textContent,
    /no longer match \/Users\/me\/agent-prompts\/pm\.md/,
    "the field must not claim a file feeds an agent it no longer feeds",
  );
  assert.match(
    status().textContent,
    /Reload .*or Clear/,
    "and must name both ways back to a true state",
  );
});

test("a reload reporting a surviving binding names it in the status line", async () => {
  nextResult = {
    localUpdated: true,
    publish: "published",
    binding: bound("/Users/me/agent-prompts/a.md", false),
    mappingError: "failed to write prompt-sources.json.tmp: Is a directory",
    prompt: "Instructions from B.\n",
  };
  await mount();
  await type("/Users/me/agent-prompts/b.md");
  await act(async () => {
    fireEvent.click(reloadButton());
  });

  assert.match(
    status().textContent,
    /still set to \/Users\/me\/agent-prompts\/a\.md/,
    "the binding that survived the failed write must be named, not hidden",
  );
});
