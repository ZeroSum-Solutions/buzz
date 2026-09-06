import assert from "node:assert/strict";
import { after, afterEach, before, test } from "node:test";
import { JSDOM } from "jsdom";

import { MAX_CANVAS_PREVIEW_SOURCE_LENGTH } from "./canvasPreview.ts";

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});

before(() => {
  Object.assign(globalThis, {
    document: dom.window.document,
    Element: dom.window.Element,
    HTMLElement: dom.window.HTMLElement,
    IS_REACT_ACT_ENVIRONMENT: true,
    Node: dom.window.Node,
    getComputedStyle: dom.window.getComputedStyle.bind(dom.window),
    localStorage: dom.window.localStorage,
    window: dom.window,
  });
  globalThis.__TAURI_INTERNALS__ = {
    invoke: async () => null,
  };
  globalThis.window.__TAURI_INTERNALS__ = globalThis.__TAURI_INTERNALS__;
});

const mountedHooks = new Set();
const activeQueryClients = new Set();

afterEach(async () => {
  globalThis.__TAURI_INTERNALS__.invoke = async () => null;
  for (const hook of mountedHooks) {
    hook.unmount();
  }
  mountedHooks.clear();
  for (const client of activeQueryClients) {
    client.clear();
  }
  activeQueryClients.clear();
  const { cleanup } = await import("@testing-library/react");
  cleanup();
});

after(() => {
  dom.window.close();
});

async function renderCanvasHook({
  channel = { id: "channel-1", channelType: "public", isMember: true },
  currentPubkey = "pubkey-1",
  enabled = true,
  canvasData,
  canvasError = null,
  membersData = [],
} = {}) {
  const React = await import("react");
  const { renderHook } = await import("@testing-library/react");
  const { QueryClient, QueryClientProvider } = await import(
    "@tanstack/react-query"
  );
  const { CommunitiesProvider } = await import(
    "@/features/communities/useCommunities.tsx"
  );
  const { useChannelFilesCanvas } = await import("./useChannelFilesCanvas.tsx");

  const queryClient = new QueryClient({
    defaultOptions: {
      queries: {
        retry: false,
        gcTime: 0,
        staleTime: Infinity,
      },
    },
  });
  activeQueryClients.add(queryClient);

  if (channel?.id) {
    if (canvasError) {
      globalThis.__TAURI_INTERNALS__.invoke = async (command) => {
        if (command === "get_canvas") throw canvasError;
        if (command === "get_channel_members") return { members: [] };
        return null;
      };
    } else if (canvasData !== undefined) {
      queryClient.setQueryData(["channel-canvas", channel.id], canvasData);
    }
    queryClient.setQueryData(["channels", channel.id, "members"], membersData);
  }

  const wrapper = ({ children }) =>
    React.createElement(
      QueryClientProvider,
      { client: queryClient },
      React.createElement(CommunitiesProvider, null, children),
    );

  const rendered = renderHook((props) => useChannelFilesCanvas(props), {
    initialProps: { channel, currentPubkey, enabled },
    wrapper,
  });
  mountedHooks.add(rendered);
  return rendered;
}

test("canvasQuery with 1MB whitespace content still returns a pinned canvas row when author/updatedAt are non-null", async () => {
  const oneMegabyteWhitespace = " ".repeat(1_000_000);
  const { result } = await renderCanvasHook({
    canvasData: {
      content: oneMegabyteWhitespace,
      updatedAt: 1_700_000_000,
      author: "0".repeat(64),
    },
  });

  assert.notEqual(
    result.current,
    null,
    "canvas row must appear when metadata indicates an existing canvas",
  );
  assert.equal(result.current.preview, "");
});

test("presence decision does not require reading past MAX_CANVAS_PREVIEW_SOURCE_LENGTH characters of content", async () => {
  const contentWithNonWhitespacePastCap = `${" ".repeat(
    MAX_CANVAS_PREVIEW_SOURCE_LENGTH,
  )}PAST_THE_CAP`;

  // When bounded metadata is present, the row appears because of the metadata,
  // not because any character walk reached past the preview cap.
  const { result: withMetadata } = await renderCanvasHook({
    canvasData: {
      content: contentWithNonWhitespacePastCap,
      updatedAt: 1_700_000_000,
      author: "0".repeat(64),
    },
  });
  assert.notEqual(
    withMetadata.current,
    null,
    "the row appears when bounded metadata is present",
  );
  assert.equal(
    withMetadata.current.preview,
    "",
    "preview does not read past the cap",
  );

  // When bounded metadata is absent (author and updatedAt are null),
  // presence must NOT be granted by scanning past the cap for content.
  const { result: withoutMetadata } = await renderCanvasHook({
    canvasData: {
      content: contentWithNonWhitespacePastCap,
      updatedAt: null,
      author: null,
    },
  });
  assert.equal(
    withoutMetadata.current,
    null,
    "no canvas row when author/updatedAt are null even if content has non-whitespace past the cap",
  );
});

test("metadata absent but non-whitespace content within the cap still shows the pinned row", async () => {
  // Mirrors sources (e.g. the e2e mock bridge) that never populate
  // author/updatedAt even though a canvas body exists: presence must fall
  // back to a bounded scan of the content rather than staying null.
  const { result } = await renderCanvasHook({
    canvasData: {
      content: "# Kickoff\n\nThe plan lives here.",
      updatedAt: null,
      author: null,
    },
  });

  assert.notEqual(
    result.current,
    null,
    "the row must appear when non-whitespace content is found within the bounded cap, even without metadata",
  );
});

test("canvas query error returns unavailable preview row rather than null", async () => {
  const { CANVAS_UNAVAILABLE_PREVIEW } = await import("./canvasPreview.ts");
  const { waitFor } = await import("@testing-library/react");
  const { result } = await renderCanvasHook({
    canvasError: new Error("failed to fetch canvas"),
  });

  await waitFor(() => {
    assert.notEqual(result.current, null);
    assert.equal(result.current.preview, CANVAS_UNAVAILABLE_PREVIEW);
  });
});

test("disabled or null channel returns null", async () => {
  const { result: disabledResult } = await renderCanvasHook({
    enabled: false,
    canvasData: {
      content: "Hello world",
      updatedAt: 1_700_000_000,
      author: "0".repeat(64),
    },
  });
  assert.equal(disabledResult.current, null);

  const { result: nullChannelResult } = await renderCanvasHook({
    channel: null,
  });
  assert.equal(nullChannelResult.current, null);
});
