import assert from "node:assert/strict";
import { after, afterEach, before, test } from "node:test";
import { JSDOM } from "jsdom";

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});
const frames = new Map();
let nextFrame = 0;
function flushFrames() {
  const pending = [...frames.values()];
  frames.clear();
  for (const callback of pending) callback();
}
before(() => {
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    value: dom.window.navigator,
  });
  Object.assign(globalThis, {
    CustomEvent: dom.window.CustomEvent,
    DOMParser: dom.window.DOMParser,
    Element: dom.window.Element,
    Event: dom.window.Event,
    Node: dom.window.Node,
    MutationObserver: dom.window.MutationObserver,
    getComputedStyle: dom.window.getComputedStyle.bind(dom.window),
  });
  Object.assign(globalThis, {
    requestAnimationFrame: (callback) => {
      frames.set(++nextFrame, callback);
      return nextFrame;
    },
    cancelAnimationFrame: (id) => frames.delete(id),
    document: dom.window.document,
    HTMLElement: dom.window.HTMLElement,
    IS_REACT_ACT_ENVIRONMENT: true,
    window: dom.window,
  });
});
afterEach(async () => {
  const { cleanup } = await import("@testing-library/react");
  cleanup();
  frames.clear();
});
after(() => dom.window.close());

async function harness() {
  const React = await import("react");
  const { render } = await import("@testing-library/react");
  const { useComposerAutofocus } = await import("./useComposerAutofocus.ts");
  function Harness({ ready, draftKey = "channel-a", disabled = false }) {
    const ref = React.useRef(null);
    const focus = React.useCallback(() => {
      if (ready) ref.current?.focus();
    }, [ready]);
    useComposerAutofocus(focus, draftKey, disabled);
    return React.createElement(
      React.Fragment,
      null,
      React.createElement("textarea", { ref, "aria-label": "Composer" }),
      React.createElement("button", { type: "button" }, "Channel navigation"),
      React.createElement(
        "div",
        { "data-slot": "popover-content", "data-state": "open" },
        React.createElement("button", {
          role: "switch",
          "aria-label": "Agent text-to-speech",
        }),
      ),
    );
  }
  const view = render(React.createElement(Harness, { ready: false }));
  React.act(flushFrames);
  return {
    ...view,
    update: (props, flush = true) => {
      view.rerender(React.createElement(Harness, props));
      if (flush) React.act(flushFrames);
    },
    act: React.act,
  };
}

test("late editor readiness preserves the selected voice control", async () => {
  const h = await harness();
  const voice = h.getByRole("switch");
  await h.act(async () => voice.focus());
  h.update({ ready: true });
  assert.ok(
    document.activeElement === voice,
    "late readiness must not steal voice control focus",
  );
});

test("late readiness preserves a selected navigation control", async () => {
  const h = await harness();
  const navigation = h.getByRole("button", { name: "Channel navigation" });
  await h.act(async () => navigation.focus());
  h.update({ ready: true });
  assert.ok(document.activeElement === navigation);
});

test("real channel navigation focuses the new composer", async () => {
  const h = await harness();
  const navigation = h.getByRole("button", { name: "Channel navigation" });
  await h.act(async () => navigation.focus());
  h.update({ ready: true, draftKey: "channel-b" });
  assert.ok(document.activeElement === h.getByRole("textbox"));
});

test("navigation cannot steal focus from an open voice popover", async () => {
  const h = await harness();
  const voice = h.getByRole("switch");
  await h.act(async () => voice.focus());
  h.update({ ready: true, draftKey: "channel-b" });
  assert.ok(document.activeElement === voice);
});

test("initial editor readiness focuses when the document owns focus", async () => {
  const h = await harness();
  h.update({ ready: true });
  assert.ok(document.activeElement === h.getByRole("textbox"));
});

test("disabled composer does not take focus", async () => {
  const h = await harness();
  h.update({ ready: true, disabled: true });
  assert.ok(document.activeElement === document.body);
});

test("opening a voice control after scheduling cancels the focus handoff", async () => {
  const h = await harness();
  h.update({ ready: true }, false);
  const voice = h.getByRole("switch");
  await h.act(async () => voice.focus());
  await h.act(async () => flushFrames());
  assert.ok(document.activeElement === voice);
});

test("navigation never overrides a newer button selection", async () => {
  const h = await harness();
  h.update({ ready: true, draftKey: "channel-b" }, false);
  const navigation = h.getByRole("button", { name: "Channel navigation" });
  await h.act(async () => navigation.focus());
  await h.act(async () => flushFrames());
  assert.ok(document.activeElement === navigation);
});

test("autofocus callback focuses the production editor synchronously without a later focus theft", async () => {
  const React = await import("react");
  const { render, waitFor } = await import("@testing-library/react");
  const { EditorContent } = await import("@tiptap/react");
  const { useRichTextEditor } = await import("./useRichTextEditor.ts");
  let richText;
  function EditorHarness() {
    richText = useRichTextEditor({});
    return richText.editor
      ? React.createElement(EditorContent, { editor: richText.editor })
      : null;
  }
  render(React.createElement(EditorHarness));
  await waitFor(() => assert.ok(richText?.editor?.isInitialized));
  // JSDOM has no layout; only scroll geometry is synthetic, focus and the
  // production Tiptap/ProseMirror editor are real.
  richText.editor.view.dom.getBoundingClientRect = () => ({
    top: 0,
    left: 0,
    right: 100,
    bottom: 20,
    width: 100,
    height: 20,
  });
  dom.window.Range.prototype.getClientRects = () => [];
  dom.window.Range.prototype.getBoundingClientRect = () => ({
    top: 0,
    left: 0,
    right: 0,
    bottom: 0,
    width: 0,
    height: 0,
  });
  await React.act(async () => {
    richText.editor.commands.setContent("<p>Draft text</p>");
    flushFrames();
  });
  await React.act(async () => richText.focusForAutofocus());
  assert.ok(
    document.activeElement === richText.editor.view.dom,
    "autofocus must finish in the guarded frame",
  );
  assert.equal(
    richText.editor.state.selection.from,
    richText.editor.state.doc.content.size - 1,
  );
  const control = document.createElement("button");
  document.body.append(control);
  control.focus();
  await React.act(async () => flushFrames());
  assert.ok(
    document.activeElement === control,
    "no deferred TipTap focus may steal subsequent user selection",
  );
  control.remove();
});

test("navigation preserves the focused action in an alert dialog", async () => {
  const h = await harness();
  const action = h.getByRole("switch");
  action.parentElement.removeAttribute("data-slot");
  action.parentElement.setAttribute("role", "alertdialog");
  await h.act(async () => action.focus());
  h.update({ ready: true, draftKey: "channel-b" });
  assert.ok(document.activeElement === action);
});
