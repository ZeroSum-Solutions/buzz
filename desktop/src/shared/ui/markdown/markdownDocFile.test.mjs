import assert from "node:assert/strict";
import { test } from "node:test";

import {
  decodeMarkdownDocBytes,
  isMarkdownDocFilename,
  isMarkdownDocTooComplexForPreview,
  MAX_MARKDOWN_DOC_BYTES,
  MAX_MARKDOWN_DOC_PREVIEW_LINES,
  MAX_MARKDOWN_DOC_PREVIEW_LINK_MARKERS,
  MAX_MARKDOWN_DOC_PREVIEW_NESTING_DEPTH,
  MAX_MARKDOWN_DOC_PREVIEW_TABLE_CELL_MARKERS,
} from "./markdownDocFile.ts";

// ── isMarkdownDocFilename ─────────────────────────────────────────────────

test("isMarkdownDocFilename: accepts .md, .markdown, .mdx", () => {
  assert.equal(isMarkdownDocFilename("README.md"), true);
  assert.equal(isMarkdownDocFilename("notes.markdown"), true);
  assert.equal(isMarkdownDocFilename("page.mdx"), true);
});

test("isMarkdownDocFilename: case-insensitive and whitespace-tolerant", () => {
  assert.equal(isMarkdownDocFilename("PLAN.MD"), true);
  assert.equal(isMarkdownDocFilename("  design.Md  "), true);
});

test("isMarkdownDocFilename: rejects other extensions", () => {
  assert.equal(isMarkdownDocFilename("report.pdf"), false);
  assert.equal(isMarkdownDocFilename("archive.zip"), false);
  assert.equal(isMarkdownDocFilename("script.mjs"), false);
  // Extension must be a suffix with a stem, not the whole name.
  assert.equal(isMarkdownDocFilename(".md"), false);
  assert.equal(isMarkdownDocFilename(""), false);
});

test("isMarkdownDocFilename: does not match mid-name extensions", () => {
  assert.equal(isMarkdownDocFilename("notes.md.zip"), false);
  assert.equal(isMarkdownDocFilename("mdfile.txt"), false);
});

// ── decodeMarkdownDocBytes ────────────────────────────────────────────────

test("decodeMarkdownDocBytes: decodes UTF-8 text", () => {
  const bytes = new TextEncoder().encode("# Hello 🐝\n\n- item");
  assert.deepEqual(decodeMarkdownDocBytes(bytes), {
    kind: "ok",
    text: "# Hello 🐝\n\n- item",
  });
});

test("decodeMarkdownDocBytes: rejects oversized payloads", () => {
  const bytes = new Uint8Array(MAX_MARKDOWN_DOC_BYTES + 1);
  assert.deepEqual(decodeMarkdownDocBytes(bytes), { kind: "too-large" });
});

test("decodeMarkdownDocBytes: accepts a payload exactly at the cap", () => {
  const bytes = new Uint8Array(MAX_MARKDOWN_DOC_BYTES).fill(0x61);
  const result = decodeMarkdownDocBytes(bytes);
  assert.equal(result.kind, "ok");
});

test("decodeMarkdownDocBytes: strict decode reports binary content", () => {
  // 0xFF is never valid in UTF-8.
  const bytes = new Uint8Array([0x23, 0x20, 0xff, 0xfe, 0x00]);
  assert.deepEqual(decodeMarkdownDocBytes(bytes), { kind: "binary" });
});

// ── isMarkdownDocTooComplexForPreview ─────────────────────────────────────
//
// Node-count proxy for the mdast/micromark parse cost (Sol audit finding 1,
// port/6731): a document well under the 2 MiB byte cap can still carry
// hundreds of thousands of block nodes if it is mostly one-line list items,
// and that shape parses at superlinear cost on the pinned parser — measured
// 854ms at 16,384 items climbing to 35,923ms at 131,072, before any React
// element exists. These bind the line-count gate that keeps the expensive
// parse off documents shaped like that fixture.

test("isMarkdownDocTooComplexForPreview: false for a short document", () => {
  assert.equal(
    isMarkdownDocTooComplexForPreview("# Title\n\nSome text."),
    false,
  );
});

test("isMarkdownDocTooComplexForPreview: false at exactly the line cap", () => {
  // MAX_MARKDOWN_DOC_PREVIEW_LINES - 1 newline-terminated lines plus one
  // final line with no trailing newline is exactly MAX_MARKDOWN_DOC_PREVIEW_LINES lines.
  const text = `${"- a\n".repeat(MAX_MARKDOWN_DOC_PREVIEW_LINES - 1)}- a`;
  assert.equal(isMarkdownDocTooComplexForPreview(text), false);
});

test("isMarkdownDocTooComplexForPreview: true one line past the cap", () => {
  const text = "- a\n".repeat(MAX_MARKDOWN_DOC_PREVIEW_LINES + 1);
  assert.equal(isMarkdownDocTooComplexForPreview(text), true);
});

test("isMarkdownDocTooComplexForPreview: true for a near-limit adversarial list (hundreds of thousands of blocks, well under the 2 MiB byte cap)", () => {
  // Mirrors the Sol audit's own reproduction shape ("- a\n" repeated);
  // 500,000 items is 2,000,000 bytes — under MAX_MARKDOWN_DOC_BYTES, so the
  // byte gate alone would admit it to the parser without this line-count
  // gate catching it first.
  const text = "- a\n".repeat(500_000);
  assert.ok(text.length < MAX_MARKDOWN_DOC_BYTES);
  assert.equal(isMarkdownDocTooComplexForPreview(text), true);
});

test("isMarkdownDocTooComplexForPreview: a legitimate long-but-simple document (few lines, large paragraphs) stays under the cap", () => {
  // Shaped like the branch's own long-doc.md fixture: large byte count,
  // very few lines. The line-count proxy must not penalize prose shape.
  const text = `# Heading\n\n${"word ".repeat(120_000)}\n`;
  assert.ok(text.length > 500_000);
  assert.equal(isMarkdownDocTooComplexForPreview(text), false);
});

// ── inline-construct (link marker) density (Sol audit finding 1, round 2,
// port/6731) ─────────────────────────────────────────────────────────────
//
// The line-count gate above only bounds block count. A single line densely
// packed with inline link syntax passes it outright (1 line) while still
// driving the mdast/micromark *inline* tokenizer into superlinear cost — the
// audit's own reproduction: "[a](http://e.co) " repeated on one line.

test("isMarkdownDocTooComplexForPreview: false at exactly the link-marker cap", () => {
  const text = `[a](http://e.co) `.repeat(
    MAX_MARKDOWN_DOC_PREVIEW_LINK_MARKERS,
  );
  assert.equal(isMarkdownDocTooComplexForPreview(text), false);
});

test("isMarkdownDocTooComplexForPreview: true one link marker past the cap", () => {
  const text = `[a](http://e.co) `.repeat(
    MAX_MARKDOWN_DOC_PREVIEW_LINK_MARKERS + 1,
  );
  assert.equal(isMarkdownDocTooComplexForPreview(text), true);
});

test("isMarkdownDocTooComplexForPreview: true for a one-line link-dense document (audit reproduction shape)", () => {
  // Mirrors the audit's own reproduction exactly: a single line, no
  // newlines at all, so the line-count gate alone would admit it.
  const text = "[a](http://e.co) ".repeat(12_336);
  assert.ok(!text.includes("\n"));
  assert.ok(text.length < MAX_MARKDOWN_DOC_BYTES);
  assert.equal(isMarkdownDocTooComplexForPreview(text), true);
});

// ── table-cell density (T9 round-3 critic finding F1) ─────────────────────
//
// The two gates above bound block count and inline-link density. A wide GFM
// table passes both — few lines, not one `[` — while every cell is its own
// mdast node: a 300-column, 100-row table is 62,194 bytes over 102 lines and
// renders in 8,643 ms through the export's own path. The `|` count is the
// quantity that tracks that cost.

/** A GFM table with `cols` columns and `rows` body rows. */
function gfmTable(cols, rows) {
  const header = `|${Array.from({ length: cols }, (_, i) => `c${i}`).join("|")}|\n`;
  const separator = `|${Array.from({ length: cols }, () => "-").join("|")}|\n`;
  const row = `|${Array.from({ length: cols }, () => "x").join("|")}|\n`;
  return header + separator + row.repeat(rows);
}

/** The `|` characters the gate counts. */
function cellMarkers(text) {
  return text.split("|").length - 1;
}

test("isMarkdownDocTooComplexForPreview: false at exactly the table-cell cap", () => {
  // 12 columns × 228 rows: 2,990 markers, the widest table still admitted,
  // measured at 163ms through renderMarkdownDocumentHtml.
  const text = gfmTable(12, 228);
  assert.ok(cellMarkers(text) <= MAX_MARKDOWN_DOC_PREVIEW_TABLE_CELL_MARKERS);
  assert.equal(isMarkdownDocTooComplexForPreview(text), false);
});

test("isMarkdownDocTooComplexForPreview: true one table-cell marker past the cap", () => {
  const text = `|${"x|".repeat(MAX_MARKDOWN_DOC_PREVIEW_TABLE_CELL_MARKERS)}`;
  assert.equal(
    cellMarkers(text),
    MAX_MARKDOWN_DOC_PREVIEW_TABLE_CELL_MARKERS + 1,
  );
  assert.equal(isMarkdownDocTooComplexForPreview(text), true);
});

test("isMarkdownDocTooComplexForPreview: true for a wide table (critic F1 reproduction shape)", () => {
  // The critic's own reproduction: 300 columns, 100 rows. Under the 2 MiB
  // byte cap, 102 lines (under the line gate), no `[` at all (under the link
  // gate) — and 8,643ms of synchronous parse.
  const text = gfmTable(300, 100);
  assert.ok(text.length < MAX_MARKDOWN_DOC_BYTES);
  assert.ok(text.split("\n").length < MAX_MARKDOWN_DOC_PREVIEW_LINES);
  assert.ok(!text.includes("["));
  assert.equal(isMarkdownDocTooComplexForPreview(text), true);
});

test("isMarkdownDocTooComplexForPreview: a document with prose-scale pipes is not penalized", () => {
  // Tables the size a document actually carries, plus pipes in running text,
  // stay far under the cap.
  const text = `# Report\n\n${gfmTable(4, 40)}\n\nUse \`a || b\` when either matches.\n`;
  assert.equal(isMarkdownDocTooComplexForPreview(text), false);
});

// ── container nesting depth (T9 round-3 critic finding F1) ────────────────
//
// The parser re-enters the whole container stack on every line, so cost grows
// with depth, not with line count: a list nested one level per line is
// 801 lines and 647,890 bytes at depth 800 — under every gate above — and
// renders in 11,066 ms.

/** A list nested one level deeper on each line. */
function nestedList(depth) {
  let out = "";
  for (let i = 0; i < depth; i++) out += `${"  ".repeat(i)}- item ${i}\n`;
  return out;
}

test("isMarkdownDocTooComplexForPreview: false at exactly the nesting-depth cap", () => {
  // The deepest line is indented MAX * 2 columns, which is exactly MAX levels.
  const text = nestedList(MAX_MARKDOWN_DOC_PREVIEW_NESTING_DEPTH + 1);
  assert.equal(isMarkdownDocTooComplexForPreview(text), false);
});

test("isMarkdownDocTooComplexForPreview: true one nesting level past the cap", () => {
  const text = nestedList(MAX_MARKDOWN_DOC_PREVIEW_NESTING_DEPTH + 2);
  assert.equal(isMarkdownDocTooComplexForPreview(text), true);
});

test("isMarkdownDocTooComplexForPreview: true for a deeply nested list (critic F1 reproduction shape)", () => {
  const text = nestedList(800);
  assert.ok(text.length < MAX_MARKDOWN_DOC_BYTES);
  assert.ok(text.split("\n").length < MAX_MARKDOWN_DOC_PREVIEW_LINES);
  assert.ok(!text.includes("["));
  assert.equal(isMarkdownDocTooComplexForPreview(text), true);
});

test("isMarkdownDocTooComplexForPreview: counts `>` markers as nesting too", () => {
  const shallow = `${"> ".repeat(MAX_MARKDOWN_DOC_PREVIEW_NESTING_DEPTH)}quoted\n`;
  const deep = `${"> ".repeat(MAX_MARKDOWN_DOC_PREVIEW_NESTING_DEPTH + 1)}quoted\n`;
  assert.equal(isMarkdownDocTooComplexForPreview(shallow), false);
  assert.equal(isMarkdownDocTooComplexForPreview(deep), true);
});

test("isMarkdownDocTooComplexForPreview: a tab counts as four indent columns", () => {
  // A tab-indented document must not slip past the depth gate by using one
  // character per level.
  const tabs = "\t".repeat(MAX_MARKDOWN_DOC_PREVIEW_NESTING_DEPTH);
  assert.equal(isMarkdownDocTooComplexForPreview(`${tabs}- deep\n`), true);
});

test("isMarkdownDocTooComplexForPreview: an ordinary indented document stays under the depth cap", () => {
  const text = `# Title\n\n- one\n  - two\n    - three\n\n\`\`\`js\n${" ".repeat(40)}const x = 1;\n\`\`\`\n`;
  assert.equal(isMarkdownDocTooComplexForPreview(text), false);
});
