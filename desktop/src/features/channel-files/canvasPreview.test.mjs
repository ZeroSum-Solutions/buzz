import assert from "node:assert/strict";
import { after, before, test } from "node:test";

import { JSDOM } from "jsdom";

// The preview helper pulls in `getMarkdownPreviewText`, which lives beside the
// channel-management rows and therefore drags a React module in with it.
const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
  pretendToBeVisual: true,
});

before(() => {
  Object.assign(globalThis, {
    document: dom.window.document,
    Element: dom.window.Element,
    HTMLElement: dom.window.HTMLElement,
    Node: dom.window.Node,
    getComputedStyle: dom.window.getComputedStyle.bind(dom.window),
    window: dom.window,
  });
});

after(() => dom.window.close());

async function load() {
  return import("./canvasPreview.ts");
}

test("the preview reads at most the capped prefix of the canvas body", async () => {
  const { MAX_CANVAS_PREVIEW_SOURCE_LENGTH, canvasPreviewText } = await load();
  // Everything past the cap is a marker the preview must never have seen. The
  // canvas body is relay-sourced and uncapped where it is read, so a preview
  // built from the whole body walks whatever the sender chose to send.
  const body = `# Kickoff\n\n${"a".repeat(
    MAX_CANVAS_PREVIEW_SOURCE_LENGTH,
  )}\n\nPAST_THE_CAP`;

  const preview = canvasPreviewText(body);

  assert.ok(preview.startsWith("Kickoff"), preview.slice(0, 40));
  assert.ok(
    !preview.includes("PAST_THE_CAP"),
    "the preview must not read past the cap",
  );
  assert.ok(preview.length <= MAX_CANVAS_PREVIEW_SOURCE_LENGTH);
});

test("the cap is applied before the body is trimmed", async () => {
  const { MAX_CANVAS_PREVIEW_SOURCE_LENGTH, canvasPreviewText } = await load();
  // Leading whitespace is the cheapest padding a sender can send. Trimming
  // first and slicing second would let it push real text into the walk.
  const body = `${" ".repeat(MAX_CANVAS_PREVIEW_SOURCE_LENGTH)}PAST_THE_CAP`;

  assert.equal(canvasPreviewText(body), "");
});

test("the preview strips markdown syntax and joins the lines", async () => {
  const { canvasPreviewText } = await load();
  assert.equal(
    canvasPreviewText("# Kickoff\n\n- **The plan** lives [here](http://x)."),
    "Kickoff The plan lives here.",
  );
});
