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
 *   - Clear is offered on a fresh mount — the dialog cannot read the stored
 *     binding back — sends `path: null`, and empties the input.
 *   - A refused path is surfaced in the status line, not swallowed, and
 *     leaves the instructions field alone.
 *   - A reload whose sidecar write failed still reports the mapping error.
 *
 * Mutation proofs: gating Clear on a seen binding again → the fresh-mount
 * Clear test fails; dropping the `onPromptReloaded` call → the propagation
 * test fails; swallowing the rejection in `run` → the error test fails.
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

globalThis.__TAURI_INTERNALS__ = {
  invoke: (cmd, payload) => {
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
let PromptSourceField;

before(async () => {
  ({ act, render, screen, cleanup, fireEvent } = await import(
    "@testing-library/react"
  ));
  ({ createElement } = await import("react"));
  ({ PromptSourceField } = await import("./PromptSourceField.tsx"));
});

afterEach(() => {
  cleanup?.();
  calls = [];
  nextResult = { localUpdated: true };
});

after(() => dom.window.close());

/** Mount the field and collect every prompt handed back to the dialog. */
function mount(overrides = {}) {
  const reloaded = [];
  render(
    createElement(PromptSourceField, {
      definitionId: "pm",
      disabled: false,
      onPromptReloaded: (prompt) => reloaded.push(prompt),
      ...overrides,
    }),
  );
  return { reloaded };
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
  mount();
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
    path: "/Users/me/agent-prompts/pm.md",
    prompt: "Ship the roadmap.\n",
  };
  const { reloaded } = mount();
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
  const { reloaded } = mount();

  // Nothing was reloaded in this session, so the field knows of no binding —
  // and must still offer to remove one that exists on disk.
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
  const { reloaded } = mount();
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
  const { reloaded } = mount();
  await type("/Users/me/agent-prompts/pm.md");
  await act(async () => {
    fireEvent.click(reloadButton());
  });

  assert.deepEqual(reloaded, ["Ship the roadmap.\n"]);
  assert.match(status().textContent, /not remembered/i);
});

test("both buttons are disabled while the dialog is saving", () => {
  mount({ disabled: true });
  assert.equal(reloadButton().disabled, true);
  assert.equal(clearButton().disabled, true);
});
