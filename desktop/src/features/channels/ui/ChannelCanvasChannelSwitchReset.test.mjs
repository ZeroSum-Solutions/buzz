/**
 * Channel-switch reset regression: ChannelManagementSheet renders ChannelCanvas
 * keyed by channelId, so a channel change remounts the subtree and drops all
 * edit state (isEditing, draft, editBaseRevision, history selection, mutation
 * instance). Without the key the sheet stays mounted and a draft typed against
 * canvas-less channel A would publish as channel B's canvas under A's retained
 * `none` precondition.
 *
 * Mirrors the parent's `<ChannelCanvas key={channelId ?? "none"} …>` wiring:
 * starts creating on canvas-less A, types a draft, switches to canvas-less B,
 * and asserts the editor is gone (state reset) and nothing was submitted.
 */

import assert from "node:assert/strict";
import { registerHooks } from "node:module";
import { after, before, test } from "node:test";

import { JSDOM } from "jsdom";

registerHooks({
  resolve(specifier, context, nextResolve) {
    if (specifier === "@/shared/ui/markdown") {
      return { shortCircuit: true, url: "buzz-canvas-stub:markdown" };
    }
    return nextResolve(specifier, context);
  },
  load(url, context, nextLoad) {
    if (url === "buzz-canvas-stub:markdown") {
      return {
        format: "module",
        shortCircuit: true,
        source: "export function Markdown() { return null; }\n",
      };
    }
    return nextLoad(url, context);
  },
});

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});

let React;
let act;
let createRoot;
let QueryClient;
let QueryClientProvider;
let ChannelNavigationProvider;
let ChannelCanvas;

const setCanvasCalls = [];

before(async () => {
  Object.assign(globalThis, {
    document: dom.window.document,
    HTMLElement: dom.window.HTMLElement,
    window: dom.window,
    IS_REACT_ACT_ENVIRONMENT: true,
  });
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    value: dom.window.navigator,
    writable: true,
  });
  dom.window.matchMedia = () => ({
    matches: false,
    addEventListener() {},
    removeEventListener() {},
  });

  // Both channels are canvas-less: get_canvas returns a null head everywhere.
  dom.window.__TAURI_INTERNALS__ = {
    invoke: async (cmd, args) => {
      if (cmd === "get_canvas") {
        return { content: "", event_id: null, updated_at: null, author: null };
      }
      if (cmd === "set_canvas") {
        setCanvasCalls.push(args);
        return { ok: true, event_id: "e".repeat(64), verified: true };
      }
      throw new Error(`unexpected command: ${cmd}`);
    },
  };

  ({ default: React, act } = await import("react"));
  ({ createRoot } = await import("react-dom/client"));
  ({ QueryClient, QueryClientProvider } = await import(
    "@tanstack/react-query"
  ));
  ({ ChannelNavigationProvider } = await import(
    "@/shared/context/ChannelNavigationContext"
  ));
  ({ ChannelCanvas } = await import("./ChannelCanvas.tsx"));
});

after(() => dom.window.close());

function click(element) {
  element.dispatchEvent(
    new dom.window.MouseEvent("click", { bubbles: true, cancelable: true }),
  );
}

async function settle(iterations = 6) {
  for (let i = 0; i < iterations; i++) {
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 1));
    });
  }
}

// Renders ChannelCanvas keyed by channelId exactly as ChannelManagementSheet
// does, so re-rendering with a new channelId remounts the subtree.
function Harness({ channelId }) {
  return React.createElement(ChannelCanvas, {
    key: channelId ?? "none",
    channelId,
    canEdit: true,
    isArchived: false,
  });
}

test("switching channels mid-create resets edit state and submits nothing", async () => {
  const queryClient = new QueryClient({
    defaultOptions: {
      queries: { gcTime: Number.POSITIVE_INFINITY, retry: false },
      mutations: { gcTime: 0 },
    },
  });
  const container = dom.window.document.createElement("div");
  dom.window.document.body.appendChild(container);
  const root = createRoot(container);

  function render(channelId) {
    return act(async () => {
      root.render(
        React.createElement(
          QueryClientProvider,
          { client: queryClient },
          React.createElement(
            ChannelNavigationProvider,
            { channels: [] },
            React.createElement(Harness, { channelId }),
          ),
        ),
      );
    });
  }

  // Load canvas-less channel A and start creating.
  await render("channel-a");
  await act(async () => {
    await queryClient.refetchQueries({ queryKey: ["channel-canvas"] });
  });
  await settle();
  const createButton = container.querySelector(
    "[data-testid='channel-canvas-edit']",
  );
  assert.ok(createButton, "create button renders for canvas-less channel A");
  await act(async () => click(createButton));
  const editor = container.querySelector(
    "[data-testid='channel-canvas-editor']",
  );
  assert.ok(editor, "editor opens on channel A");

  // Type a draft against A.
  await act(async () => {
    const setter = Object.getOwnPropertyDescriptor(
      dom.window.HTMLTextAreaElement.prototype,
      "value",
    ).set;
    setter.call(editor, "draft written for channel A");
    editor.dispatchEvent(new dom.window.Event("input", { bubbles: true }));
  });
  await settle();

  // Switch to canvas-less channel B — the key change remounts ChannelCanvas.
  await render("channel-b");
  await act(async () => {
    await queryClient.refetchQueries({ queryKey: ["channel-canvas"] });
  });
  await settle();

  // Edit state must not survive the switch: the editor is gone and B shows its
  // own fresh Create action.
  assert.equal(
    container.querySelector("[data-testid='channel-canvas-editor']"),
    null,
    "editor does not carry across the channel switch",
  );
  const bCreate = container.querySelector(
    "[data-testid='channel-canvas-edit']",
  );
  assert.ok(bCreate, "channel B shows its own fresh Create action");
  assert.equal(
    bCreate.textContent.trim(),
    "Create canvas",
    "channel B is treated as canvas-less, not carrying A's edit session",
  );

  // Nothing was ever submitted — A's draft never published against B.
  assert.equal(
    setCanvasCalls.length,
    0,
    "no canvas save fired across the channel switch",
  );

  await settle(12);
  await act(async () => root.unmount());
  queryClient.clear();
  container.remove();
});
