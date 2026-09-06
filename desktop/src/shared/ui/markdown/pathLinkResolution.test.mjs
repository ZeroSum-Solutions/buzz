import assert from "node:assert/strict";
import { after, afterEach, before, test } from "node:test";

import { JSDOM } from "jsdom";

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});

before(() => {
  Object.assign(globalThis, {
    CustomEvent: dom.window.CustomEvent,
    document: dom.window.document,
    Element: dom.window.Element,
    Event: dom.window.Event,
    HTMLElement: dom.window.HTMLElement,
    IS_REACT_ACT_ENVIRONMENT: true,
    Node: dom.window.Node,
    window: dom.window,
  });
});

afterEach(async () => {
  const { cleanup } = await import("@testing-library/react");
  cleanup();
});

after(() => dom.window.close());

const FIRST = "docs/open-previews.command";
const SECOND = "docs/readme.md";

function target(path, filename, kind = "file") {
  return { path, filename, kind, sizeBytes: 12 };
}

/** A resolver whose answers are keyed by candidate, recording every call. */
function scriptedInvoke(answers) {
  const calls = [];
  return {
    calls,
    invoke: async (command, payload) => {
      calls.push({ command, payload });
      return answers[payload.candidate] ?? null;
    },
  };
}

/** A resolver that hands back the promise's settle function to the test. */
function deferredInvoke() {
  const calls = [];
  let settle;
  return {
    calls,
    settle: (value) => settle(value),
    invoke: (command, payload) => {
      calls.push({ command, payload });
      return new Promise((resolvePromise) => {
        settle = resolvePromise;
      });
    },
  };
}

async function renderResolution(initialProps) {
  const { renderHook } = await import("@testing-library/react");
  const { usePathLinkResolution } = await import("./pathLinkResolution.ts");
  return renderHook((props) => usePathLinkResolution(props), { initialProps });
}

function baseProps(overrides) {
  return {
    text: FIRST,
    senderPubkey: null,
    invoke: async () => null,
    onOpen: () => {},
    onError: () => {},
    ...overrides,
  };
}

test("a token resolves once and a click opens what it resolved to", async () => {
  const { act } = await import("@testing-library/react");
  const opened = [];
  const { calls, invoke } = scriptedInvoke({
    [FIRST]: target(
      "/root/docs/open-previews.command",
      "open-previews.command",
    ),
  });
  const view = await renderResolution(
    baseProps({ invoke, onOpen: (value) => opened.push(value) }),
  );

  assert.equal(view.result.current.state.status, "idle");
  await act(async () => {
    view.result.current.resolve();
  });
  assert.equal(view.result.current.state.status, "link");
  assert.equal(calls.length, 1);
  assert.equal(calls[0].command, "resolve_path_link");

  // A second trigger asks nothing more.
  await act(async () => {
    view.result.current.resolve();
  });
  assert.equal(calls.length, 1);

  await act(async () => {
    view.result.current.activate();
  });
  assert.equal(opened.length, 1);
  assert.equal(opened[0].filename, "open-previews.command");
});

test("editing the message retires the settled resolution", async () => {
  // The reported shape: the sender posts an executable path, the pointer
  // resolves it, then the sender edits the message to a harmless one. React
  // reconciles the same component, so without a reset the chip would still
  // open the first path under the second path's label.
  const { act } = await import("@testing-library/react");
  const opened = [];
  const { calls, invoke } = scriptedInvoke({
    [FIRST]: target(
      "/root/docs/open-previews.command",
      "open-previews.command",
    ),
    [SECOND]: target("/root/docs/readme.md", "readme.md", "markdown"),
  });
  const view = await renderResolution(
    baseProps({ invoke, onOpen: (value) => opened.push(value) }),
  );

  await act(async () => {
    view.result.current.resolve();
  });
  assert.equal(
    view.result.current.state.target.filename,
    "open-previews.command",
  );

  await act(async () => {
    view.rerender(
      baseProps({
        invoke,
        onOpen: (value) => opened.push(value),
        text: SECOND,
      }),
    );
  });
  assert.equal(
    view.result.current.state.status,
    "idle",
    "the edited token starts over instead of inheriting a target",
  );

  await act(async () => {
    view.result.current.activate();
  });
  assert.equal(calls.length, 2);
  assert.equal(calls[1].payload.candidate, SECOND);
  assert.deepEqual(
    opened.map((value) => value.filename),
    ["readme.md"],
  );
});

test("changing the sender retires the settled resolution", async () => {
  const { act } = await import("@testing-library/react");
  const { calls, invoke } = scriptedInvoke({
    [FIRST]: target(
      "/root/docs/open-previews.command",
      "open-previews.command",
    ),
  });
  const view = await renderResolution(baseProps({ invoke }));

  await act(async () => {
    view.result.current.resolve();
  });
  assert.equal(view.result.current.state.status, "link");

  await act(async () => {
    view.rerender(baseProps({ invoke, senderPubkey: "ab12" }));
  });
  assert.equal(view.result.current.state.status, "idle");

  await act(async () => {
    view.result.current.resolve();
  });
  assert.equal(calls.length, 2);
  assert.equal(calls[1].payload.senderPubkey, "ab12");
});

test("an answer that lands after an edit is dropped, never opened", async () => {
  const { act } = await import("@testing-library/react");
  const opened = [];
  const deferred = deferredInvoke();
  const props = {
    invoke: deferred.invoke,
    onOpen: (value) => opened.push(value),
  };
  const view = await renderResolution(baseProps(props));

  // A click starts a resolution that has not answered yet.
  await act(async () => {
    view.result.current.activate();
  });
  assert.equal(view.result.current.state.status, "pending");

  // The message is edited while that resolution is in flight.
  await act(async () => {
    view.rerender(baseProps({ ...props, text: SECOND }));
  });
  assert.equal(view.result.current.state.status, "idle");

  // The first candidate's answer arrives afterwards.
  await act(async () => {
    deferred.settle(
      target("/root/docs/open-previews.command", "open-previews.command"),
    );
    await Promise.resolve();
  });
  assert.deepEqual(opened, [], "a superseded answer must never be opened");
  assert.equal(
    view.result.current.state.status,
    "idle",
    "the token is still free to resolve its new candidate",
  );
});

test("a resolver refusal is surfaced and leaves the token as text", async () => {
  const { act } = await import("@testing-library/react");
  const errors = [];
  const view = await renderResolution(
    baseProps({
      invoke: async () => {
        throw new Error("path link candidate exceeds 4096 bytes");
      },
      onError: (message) => errors.push(message),
    }),
  );

  await act(async () => {
    view.result.current.resolve();
  });
  assert.equal(view.result.current.state.status, "text");
  assert.deepEqual(errors, ["path link candidate exceeds 4096 bytes"]);
});

test("a candidate the shape gate refuses never reaches the resolver", async () => {
  const { act } = await import("@testing-library/react");
  const { calls, invoke } = scriptedInvoke({});
  const view = await renderResolution(baseProps({ invoke, text: "cargo" }));

  await act(async () => {
    view.result.current.resolve();
  });
  assert.deepEqual(calls, []);
  assert.equal(view.result.current.state.status, "text");
});
