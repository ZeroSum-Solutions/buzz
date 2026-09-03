/**
 * Pending-restore guard regression: row buttons must be disabled while a
 * restore is pending so clicking a different row cannot call
 * `restoreMutation.reset()` — which would unobserve the running mutation,
 * hide a subsequent rejection, and allow a second concurrent restore.
 *
 * This test verifies the structural invariant: once a restore is dispatched
 * (confirm clicked), the row-toggle buttons are rendered with `disabled`,
 * matching the Restore button's existing disabled guard. The deferred IPC is
 * resolved (not rejected) at teardown so the promise chain drains cleanly.
 *
 * Revert-causality: removing `disabled={restoreMutation.isPending}` from the
 * row button makes `secondRowIsDisabled` false — the assertion fails.
 */

import assert from "node:assert/strict";
import { registerHooks } from "node:module";
import { after, before, test } from "node:test";

import { JSDOM } from "jsdom";

import { installRadixDialogGlobals } from "./canvasDialogTestEnv.mjs";

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

const HEAD = "a".repeat(64);
const OLDER_A = "b".repeat(64);
const OLDER_B = "c".repeat(64);

let React;
let act;
let createRoot;
let QueryClient;
let QueryClientProvider;
let CommunitiesProvider;
let CanvasHistoryPanel;

// Controls the IPC: null means the test has not triggered set_canvas yet.
let deferredResolve = null;
let setCanvasCalls = 0;

before(async () => {
  Object.assign(globalThis, {
    document: dom.window.document,
    HTMLElement: dom.window.HTMLElement,
    MutationObserver: dom.window.MutationObserver,
    ResizeObserver: class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
    self: dom.window,
    window: dom.window,
    localStorage: dom.window.localStorage,
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
  installRadixDialogGlobals(dom);

  dom.window.__TAURI_INTERNALS__ = {
    invoke: async (cmd) => {
      if (cmd === "set_canvas") {
        setCanvasCalls += 1;
        // Return a promise the test drains at teardown via deferredResolve.
        return new Promise((resolve) => {
          deferredResolve = resolve;
        });
      }
      if (cmd === "get_canvas_history") {
        return {
          revisions: [
            { event_id: HEAD, content: "hi", created_at: 3, author: HEAD },
            {
              event_id: OLDER_A,
              content: "older-a",
              created_at: 2,
              author: HEAD,
            },
            {
              event_id: OLDER_B,
              content: "older-b",
              created_at: 1,
              author: HEAD,
            },
          ],
          next_cursor: null,
        };
      }
      if (cmd === "get_users_batch") {
        return { profiles: {} };
      }
      throw new Error(`unexpected command: ${cmd}`);
    },
  };

  ({ default: React, act } = await import("react"));
  ({ createRoot } = await import("react-dom/client"));
  ({ QueryClient, QueryClientProvider } = await import(
    "@tanstack/react-query"
  ));
  ({ CommunitiesProvider } = await import(
    "@/features/communities/useCommunities"
  ));
  ({ CanvasHistoryPanel } = await import("./CanvasHistoryPanel.tsx"));
});

after(() => dom.window.close());

function click(element) {
  element.dispatchEvent(
    new dom.window.MouseEvent("click", { bubbles: true, cancelable: true }),
  );
}

async function settle(iterations = 8) {
  for (let i = 0; i < iterations; i++) {
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 1));
    });
  }
}

test("row buttons are disabled while a restore is pending", async () => {
  setCanvasCalls = 0;
  deferredResolve = null;

  const client = new QueryClient({
    defaultOptions: {
      queries: { gcTime: Number.POSITIVE_INFINITY, retry: false },
      mutations: { gcTime: 0, retry: false },
    },
  });
  const container = dom.window.document.createElement("div");
  dom.window.document.body.appendChild(container);
  const root = createRoot(container);
  await act(async () => {
    root.render(
      React.createElement(
        QueryClientProvider,
        { client },
        React.createElement(
          CommunitiesProvider,
          null,
          React.createElement(CanvasHistoryPanel, {
            channelId: "channel-1",
            currentContent: "hi",
            currentRevision: HEAD,
            canRestore: true,
          }),
        ),
      ),
    );
  });
  await settle();

  // Expand OLDER_A (second item) to reveal its Restore button.
  const items = container.querySelectorAll(
    "[data-testid='channel-canvas-history-item'] button",
  );
  await act(async () => click(items[1]));
  await settle();

  // Click Restore → opens confirmation dialog.
  await act(async () => {
    click(container.querySelector("[data-testid='channel-canvas-restore']"));
  });
  await settle();

  // Confirm → dispatches the deferred set_canvas call (isPending becomes true).
  await act(async () => {
    click(
      dom.window.document.querySelector(
        "[data-testid='channel-canvas-restore-confirm-action']",
      ),
    );
  });
  // Allow one tick for the mutation to enter isPending state without resolving.
  await act(async () => {
    await new Promise((resolve) => setTimeout(resolve, 2));
  });

  // Snapshot observed state while the restore is pending.
  const rowButtons = container.querySelectorAll(
    "[data-testid='channel-canvas-history-item'] button",
  );
  const secondRowIsDisabled =
    rowButtons[rowButtons.length - 1]?.disabled === true;
  const callsWhilePending = setCanvasCalls;

  try {
    // Resolve the deferred IPC so the mutation settles cleanly before teardown.
    // This must run even if assertions fail or the process cannot exit.
    await act(async () => {
      deferredResolve?.({ ok: true, event_id: "e".repeat(64), verified: true });
      deferredResolve = null;
    });
    await settle();
  } finally {
    await act(async () => root.unmount());
    client.clear();
    container.remove();
  }

  assert.equal(
    callsWhilePending,
    1,
    "exactly one set_canvas call fires for the restore",
  );
  assert.ok(
    secondRowIsDisabled,
    "row buttons must be disabled while restoreMutation.isPending is true — " +
      "a disabled button prevents reset() from being called mid-flight, " +
      "which would unobserve the pending mutation and hide subsequent errors",
  );
});
