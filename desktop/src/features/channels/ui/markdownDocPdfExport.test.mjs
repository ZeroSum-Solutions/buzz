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

// The export runs the same mdast/micromark parse the Preview does, so the same
// complexity predicate has to gate it. Both shapes below are far under the
// 2 MiB source cap, which is exactly why bytes are the wrong bound: rendered,
// each costs seconds of synchronous main-thread work. The elapsed-time
// assertion is what makes these falsifiable — with the gate removed the render
// runs and the call takes seconds, not milliseconds.
const GATE_BUDGET_MS = 2000;

test("refuses a link-dense one-liner before it is rendered", async () => {
  const bridge = installBridge(() => true);
  try {
    // 60,000 links on ONE line: 1,020,000 bytes, under the byte cap, and one
    // line, under the line-count gate.
    const content = "[a](http://e.co) ".repeat(60000);
    assert.ok(content.length < MAX_PDF_DOCUMENT_SOURCE_BYTES);
    assert.equal(content.split("\n").length, 1);

    const started = performance.now();
    await assert.rejects(
      exportMarkdownDocumentToPdf({ content, filename: "links.md" }),
      /too many elements/,
    );
    const elapsed = performance.now() - started;

    assert.equal(bridge.calls.length, 0);
    assert.ok(
      elapsed < GATE_BUDGET_MS,
      `the gate must refuse before the parse, took ${elapsed}ms`,
    );
  } finally {
    bridge.restore();
  }
});

test("refuses a flat list of thousands of lines before it is rendered", async () => {
  const bridge = installBridge(() => true);
  try {
    const content = "- a\n".repeat(40000);
    assert.ok(content.length < MAX_PDF_DOCUMENT_SOURCE_BYTES);

    const started = performance.now();
    await assert.rejects(
      exportMarkdownDocumentToPdf({ content, filename: "list.md" }),
      /too many elements/,
    );
    const elapsed = performance.now() - started;

    assert.equal(bridge.calls.length, 0);
    assert.ok(
      elapsed < GATE_BUDGET_MS,
      `the gate must refuse before the parse, took ${elapsed}ms`,
    );
  } finally {
    bridge.restore();
  }
});

test("refuses a wide table before it is rendered", async () => {
  const bridge = installBridge(() => true);
  try {
    // 300 columns × 100 rows: 62,194 bytes (under the byte cap), 102 lines
    // (under the line gate), not one `[` (under the link gate) — and 8,643 ms
    // of synchronous parse on the main thread if it is let through.
    const columns = 300;
    const header = `|${Array.from({ length: columns }, (_, i) => `c${i}`).join("|")}|\n`;
    const separator = `|${Array.from({ length: columns }, () => "-").join("|")}|\n`;
    const row = `|${Array.from({ length: columns }, () => "x").join("|")}|\n`;
    const content = header + separator + row.repeat(100);
    assert.ok(content.length < MAX_PDF_DOCUMENT_SOURCE_BYTES);
    assert.ok(content.split("\n").length < 3000);
    assert.ok(!content.includes("["));

    const started = performance.now();
    await assert.rejects(
      exportMarkdownDocumentToPdf({ content, filename: "table.md" }),
      /too many elements/,
    );
    const elapsed = performance.now() - started;

    assert.equal(bridge.calls.length, 0);
    assert.ok(
      elapsed < GATE_BUDGET_MS,
      `the gate must refuse before the parse, took ${elapsed}ms`,
    );
  } finally {
    bridge.restore();
  }
});

test("refuses a deeply nested list before it is rendered", async () => {
  const bridge = installBridge(() => true);
  try {
    // One level deeper per line, 800 levels: 801 lines, 647,890 bytes, no `[`
    // and no `|` — every other gate admits it, and it renders in 11,066 ms.
    let content = "";
    for (let depth = 0; depth < 800; depth++) {
      content += `${"  ".repeat(depth)}- item ${depth}\n`;
    }
    assert.ok(content.length < MAX_PDF_DOCUMENT_SOURCE_BYTES);
    assert.ok(content.split("\n").length < 3000);

    const started = performance.now();
    await assert.rejects(
      exportMarkdownDocumentToPdf({ content, filename: "nested.md" }),
      /too many elements/,
    );
    const elapsed = performance.now() - started;

    assert.equal(bridge.calls.length, 0);
    assert.ok(
      elapsed < GATE_BUDGET_MS,
      `the gate must refuse before the parse, took ${elapsed}ms`,
    );
  } finally {
    bridge.restore();
  }
});

test("a document just inside the complexity gate still exports", async () => {
  const bridge = installBridge(() => true);
  try {
    // 2,000 link markers and 2,999 lines: the largest document the panel will
    // preview, and so the largest it offers to export.
    const content = `${"[a](http://e.co)\n".repeat(2000)}${"text\n".repeat(999)}`;
    assert.equal(
      await exportMarkdownDocumentToPdf({ content, filename: "edge.md" }),
      true,
    );
    assert.equal(bridge.calls.length, 1);
  } finally {
    bridge.restore();
  }
});

test("a table and a nesting depth just inside the gate still export", async () => {
  const bridge = installBridge(() => true);
  try {
    // 12 columns × 228 rows is 2,990 cell markers, and 128 levels is the
    // deepest nesting admitted: the widest and deepest documents the panel
    // still previews, and so the largest it offers to export. Rendering both
    // is part of the assertion — the export path runs the real parse here.
    const header = `|${Array.from({ length: 12 }, (_, i) => `c${i}`).join("|")}|\n`;
    const separator = `|${Array.from({ length: 12 }, () => "-").join("|")}|\n`;
    const row = `|${Array.from({ length: 12 }, () => "x").join("|")}|\n`;
    let nested = "";
    for (let depth = 0; depth < 129; depth++) {
      nested += `${"  ".repeat(depth)}- item ${depth}\n`;
    }
    const content = `${header + separator + row.repeat(228)}\n${nested}`;

    assert.equal(
      await exportMarkdownDocumentToPdf({ content, filename: "edge-table.md" }),
      true,
    );
    assert.equal(bridge.calls.length, 1);
    assert.match(bridge.calls[0].payload.bodyHtml, /<table>/);
  } finally {
    bridge.restore();
  }
});
