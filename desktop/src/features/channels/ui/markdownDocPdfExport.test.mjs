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

// The export runs the same micromark/mdast parse the Preview does, so the same
// gate has to bind it. Every shape below is far under the 2 MiB source cap,
// which is exactly why bytes are the wrong bound: rendered, each costs seconds
// of synchronous main-thread work with the save dialog yet to appear. The
// elapsed-time assertion is what makes these falsifiable — with the gate
// removed the render runs and the call takes seconds, not milliseconds.
//
// 200 ms is this app's main-thread task budget. Refusing any of these is one
// linear scan of the source; the slowest measured on this machine is 13 ms for
// a 2 MiB document.
const GATE_BUDGET_MS = 200;

/** Assert the export refuses `content` without rendering, inside the budget. */
async function assertRefusedBeforeRender(content, filename) {
  const bridge = installBridge(() => true);
  try {
    assert.ok(content.length < MAX_PDF_DOCUMENT_SOURCE_BYTES);
    const started = performance.now();
    await assert.rejects(
      exportMarkdownDocumentToPdf({ content, filename }),
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
}

test("refuses an emphasis-dense one-liner (round-3 critic F4)", async () => {
  // `"*a*"` × 20,000: 60,000 bytes on ONE line, no `[`, no `|`, no nesting —
  // admitted by every counter the round-3 gate had, and measured at 3,458 ms
  // through `renderMarkdownDocumentHtml`, with the export succeeding and the
  // PDF written. 40,000 inline delimiters in one block is the quantity that
  // costs.
  const content = "*a*".repeat(20000);
  assert.equal(content.split("\n").length, 1);
  assert.ok(!content.includes("["));
  assert.ok(!content.includes("|"));
  await assertRefusedBeforeRender(content, "emphasis.md");
});

test("refuses a block quote held at depth across many lines (round-3 critic F5)", async () => {
  // Depth 127 across 3,000 lines: 767,999 bytes, no `[`, no `|`, and only 130
  // mdast nodes — so the node budget cannot see it either. The round-3 gate
  // bounded the deepest *line* at 128 and admitted this outright, at 903 ms.
  const content = Array.from(
    { length: 3000 },
    () => `${"> ".repeat(127)}x`,
  ).join("\n");
  assert.ok(!content.includes("["));
  assert.ok(!content.includes("|"));
  await assertRefusedBeforeRender(content, "quoted.md");
});

test("refuses a link-dense one-liner before it is rendered", async () => {
  // 60,000 links on ONE line: 1,020,000 bytes, under the byte cap.
  const content = "[a](http://e.co) ".repeat(60000);
  assert.equal(content.split("\n").length, 1);
  await assertRefusedBeforeRender(content, "links.md");
});

test("refuses a flat list of thousands of lines before it is rendered", async () => {
  await assertRefusedBeforeRender("- a\n".repeat(40000), "list.md");
});

test("refuses GFM literal autolinks, which carry no delimiter at all", async () => {
  // `www.`, `://` and `@` start inline constructs on ordinary word
  // characters, so a delimiter counter cannot see them: 60,000 of them is
  // 960,000 bytes on one line, 180,001 mdast nodes and 6,752 ms rendered.
  await assertRefusedBeforeRender("www.example.com ".repeat(60000), "www.md");
});

test("refuses a wide table before it is rendered", async () => {
  // 300 columns × 100 rows: 62,194 bytes, 102 lines, not one `[` — and
  // 6,902 ms of synchronous parse on the main thread if it is let through.
  const columns = 300;
  const header = `|${Array.from({ length: columns }, (_, i) => `c${i}`).join("|")}|\n`;
  const separator = `|${Array.from({ length: columns }, () => "-").join("|")}|\n`;
  const row = `|${Array.from({ length: columns }, () => "x").join("|")}|\n`;
  const content = header + separator + row.repeat(100);
  assert.ok(!content.includes("["));
  await assertRefusedBeforeRender(content, "table.md");
});

test("refuses a deeply nested list before it is rendered", async () => {
  // One level deeper per line, 800 levels: 801 lines, 642,400 bytes, no `[`
  // and no `|` — and 7,412 ms rendered.
  let content = "";
  for (let depth = 0; depth < 800; depth++) {
    content += `${"  ".repeat(depth)}- item ${depth}\n`;
  }
  await assertRefusedBeforeRender(content, "nested.md");
});

test("refuses a document the node budget catches, and never invokes the exporter", async () => {
  // 10,000 one-line list items pass every source-side counter; the parsed
  // tree is 30,002 nodes, past `MAX_MARKDOWN_DOC_NODES`. This one costs its
  // parse — the source counters are what keep that finite — so it is the one
  // refusal without a 200 ms assertion. What it does assert is that the
  // refusal is the bounded "too complex" message and not a render failure,
  // and that nothing reached the exporter.
  const bridge = installBridge(() => true);
  try {
    const content = "- a\n".repeat(10000);
    await assert.rejects(
      exportMarkdownDocumentToPdf({ content, filename: "budget.md" }),
      /too many elements/,
    );
    assert.equal(bridge.calls.length, 0);
  } finally {
    bridge.restore();
  }
});

test("a document just inside the gate still exports", async () => {
  const bridge = installBridge(() => true);
  try {
    // 4,000 inline delimiters in one block (2^24 of delimiter work, the most
    // admitted) across 3,000 lines — the line count the round-3 gate refused
    // outright.
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
    // 12 columns × 228 rows is 3,003 cell markers, and 129 levels of nesting
    // is 8,256 units of container descent: the widest and deepest documents
    // the panel still previews, and so the largest it offers to export.
    // Rendering both is part of the assertion — the export path runs the real
    // parse here.
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
