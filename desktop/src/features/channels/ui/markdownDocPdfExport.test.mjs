/**
 * The viewer's Export PDF path: what it sends to the Rust exporter, what it
 * refuses before rendering, and how a cancelled dialog differs from a failure.
 *
 * `invokeTauri` is exercised through its real seam (`window.__TAURI_INTERNALS__`)
 * rather than a module stub, so these tests bind the production call path.
 */
import assert from "node:assert/strict";
import test from "node:test";

import {
  MAX_PDF_DOCUMENT_SOURCE_BYTES,
  exportMarkdownDocumentToPdf,
  pdfDocumentTitle,
} from "./markdownDocPdfExport.ts";

function installBridge(handler) {
  if (!globalThis.window) globalThis.window = globalThis;
  const previous = globalThis.window.__TAURI_INTERNALS__;
  const calls = [];
  globalThis.window.__TAURI_INTERNALS__ = {
    invoke: async (command, payload) => {
      calls.push({ command, payload });
      return handler(command, payload);
    },
  };
  return {
    calls,
    restore() {
      if (previous === undefined) delete globalThis.window.__TAURI_INTERNALS__;
      else globalThis.window.__TAURI_INTERNALS__ = previous;
    },
  };
}

test("derives a bounded document title from the attachment name", () => {
  assert.equal(pdfDocumentTitle("release-notes.md"), "release-notes");
  assert.equal(
    pdfDocumentTitle("docs/release-notes.MARKDOWN"),
    "release-notes",
  );
  assert.equal(pdfDocumentTitle("notes.mdx"), "notes");
  assert.equal(pdfDocumentTitle("archive.tar.gz"), "archive.tar.gz");
  assert.equal(pdfDocumentTitle(""), "Document");
  assert.equal(pdfDocumentTitle("a\nb.md"), "ab");
  assert.equal(pdfDocumentTitle(`${"t".repeat(500)}.md`).length, 200);
});

test("sends the document-mode HTML, title and filename to the exporter", async () => {
  const bridge = installBridge(() => true);
  try {
    const saved = await exportMarkdownDocumentToPdf({
      content: "# Heading\n\nSee the [handbook](https://example.invalid/h).\n",
      filename: "release-notes.md",
    });
    assert.equal(saved, true);
    assert.equal(bridge.calls.length, 1);
    const { command, payload } = bridge.calls[0];
    assert.equal(command, "export_document_pdf");
    assert.equal(payload.title, "release-notes");
    assert.equal(payload.filename, "release-notes.md");
    assert.match(payload.bodyHtml, /<h1>Heading<\/h1>/);
    assert.match(payload.bodyHtml, /<a href="https:\/\/example\.invalid\/h">/);
  } finally {
    bridge.restore();
  }
});

test("a cancelled save dialog resolves false and is not an error", async () => {
  const bridge = installBridge(() => false);
  try {
    assert.equal(
      await exportMarkdownDocumentToPdf({
        content: "# Heading\n",
        filename: "notes.md",
      }),
      false,
    );
    assert.equal(bridge.calls.length, 1);
  } finally {
    bridge.restore();
  }
});

test("an exporter failure propagates instead of reporting a save", async () => {
  const bridge = installBridge(() => {
    throw new Error("PDF export could not start Chrome: no such file");
  });
  try {
    await assert.rejects(
      exportMarkdownDocumentToPdf({
        content: "# Heading\n",
        filename: "notes.md",
      }),
      /could not start Chrome/,
    );
  } finally {
    bridge.restore();
  }
});

test("refuses an oversized document before it is rendered", async () => {
  const bridge = installBridge(() => true);
  try {
    const oversized = "x".repeat(MAX_PDF_DOCUMENT_SOURCE_BYTES + 1);
    await assert.rejects(
      exportMarkdownDocumentToPdf({
        content: oversized,
        filename: "huge.md",
      }),
      /too large/,
    );
    // The guard is what stops the render: with it removed the exporter would
    // have been invoked with the rendered document.
    assert.equal(bridge.calls.length, 0);
  } finally {
    bridge.restore();
  }
});
