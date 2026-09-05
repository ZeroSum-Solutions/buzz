/**
 * Document-mode renderer: the markdown render the PDF exporter prints.
 *
 * The first test binds this renderer to the fixture the Rust exporter is
 * tested against (`desktop/tests/fixtures/pdf-export/approval-body.html`), so
 * the HTML the app sends and the HTML the exporter is proven on cannot drift.
 * Regenerate the fixture only when the renderer intentionally changes.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import { renderMarkdownDocumentHtml } from "./documentMode.tsx";

const fixturesDir = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../../../tests/fixtures/pdf-export",
);

test("renders the export fixture to the committed document-mode HTML", async () => {
  const markdown = readFileSync(path.join(fixturesDir, "approval.md"), "utf8");
  const expected = readFileSync(
    path.join(fixturesDir, "approval-body.html"),
    "utf8",
  );
  assert.equal(await renderMarkdownDocumentHtml(markdown), expected);
});

test("keeps links as links rather than flattening them to text", async () => {
  const html = await renderMarkdownDocumentHtml(
    "See the [handbook](https://example.invalid/handbook).",
  );
  assert.match(html, /<a href="https:\/\/example\.invalid\/handbook">/);
  assert.match(html, />handbook</);
});

test("renders code blocks uncollapsed, without the viewer's chrome", async () => {
  const html = await renderMarkdownDocumentHtml(
    ["```python", "print('hello')", "```"].join("\n"),
  );
  assert.match(html, /<pre><code class="language-python">/);
  // The chat/viewer code block wraps itself in a scroll container with a
  // height cap and a copy button; a printed document must have none of that.
  assert.doesNotMatch(html, /data-code-block/);
  assert.doesNotMatch(html, /max-h-/);
  assert.doesNotMatch(html, /overflow-/);
});

test("renders images as labelled links so an export fetches nothing", async () => {
  const html = await renderMarkdownDocumentHtml(
    "![vendor logo](https://remote.invalid/logo.png)",
  );
  assert.doesNotMatch(html, /<img/);
  assert.match(html, /<a href="https:\/\/remote\.invalid\/logo\.png">/);
  assert.match(html, />vendor logo</);
});

test("degrades chat-only inline nodes to plain text", async () => {
  const html = await renderMarkdownDocumentHtml(
    "Jump to buzz://channel/2f9d1c5e and read it.",
  );
  assert.doesNotMatch(html, /<channel-deep-link/);
  assert.match(html, /buzz:\/\/channel\/2f9d1c5e/);
});

test("uses markdown paragraph rules, not the chat newline-is-a-break rule", async () => {
  const html = await renderMarkdownDocumentHtml("one\ntwo");
  assert.doesNotMatch(html, /<br/i);
});
