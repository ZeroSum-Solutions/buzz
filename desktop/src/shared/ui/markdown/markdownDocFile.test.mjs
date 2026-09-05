import assert from "node:assert/strict";
import { test } from "node:test";

import {
  decodeMarkdownDocBytes,
  exceedsMarkdownDocParseBudget,
  isMarkdownDocFilename,
  MAX_MARKDOWN_DOC_BYTES,
  MAX_MARKDOWN_DOC_DELIMITER_WORK,
  MAX_MARKDOWN_DOC_DESCENT_WORK,
  MAX_MARKDOWN_DOC_ESTIMATED_NODES,
  MAX_MARKDOWN_DOC_TABLE_CELL_MARKERS,
  MAX_MARKDOWN_DOC_TABLE_CELL_WORK,
  measureMarkdownDocParseWork,
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

// ── parse-work pre-filter ─────────────────────────────────────────────────
//
// The cheap half of the render gate. It does not try to guess how many nodes
// a document has — `MAX_MARKDOWN_DOC_NODES` counts those on the parsed tree
// (see markdownParseBudget.test.mjs). What it bounds is the work micromark's
// own tokenizers do *before* any mdast node exists, on the two axes where
// that work is superlinear, plus a node estimate whose only job is to keep
// the parse the node budget has to run finite.
//
// Every number below is a measurement through the production entry
// `renderMarkdownDocumentHtml`; the reproduction shapes are the ones the
// round-3 blind critic filed as F4 and F5.

/** A GFM table with `cols` columns and `rows` body rows. */
function gfmTable(cols, rows) {
  const header = `|${Array.from({ length: cols }, (_, i) => `c${i}`).join("|")}|\n`;
  const separator = `|${Array.from({ length: cols }, () => "-").join("|")}|\n`;
  const row = `|${Array.from({ length: cols }, () => "x").join("|")}|\n`;
  return header + separator + row.repeat(rows);
}

/**
 * A GFM table whose header declares `cols` columns and whose `rows` body
 * lines carry no `|` at all — hast pads each of them to the header's width.
 */
function pipelessTable(cols, rows) {
  const header = `|${Array.from({ length: cols }, (_, i) => `c${i}`).join("|")}|\n`;
  const separator = `|${Array.from({ length: cols }, () => "-").join("|")}|\n`;
  return header + separator + "x\n".repeat(rows);
}

/** A list nested one level deeper on each line. */
function nestedList(depth) {
  let out = "";
  for (let i = 0; i < depth; i++) out += `${"  ".repeat(i)}- item ${i}\n`;
  return out;
}

/** `lines` lines of prose, each held at block-quote depth `depth`. */
function quotedAtDepth(depth, lines) {
  return Array.from({ length: lines }, () => `${"> ".repeat(depth)}x`).join(
    "\n",
  );
}

test("parse work: a short document costs nothing on any axis", () => {
  const work = measureMarkdownDocParseWork("# Title\n\nSome text.\n");
  assert.equal(work.descentWork, 0);
  assert.ok(work.delimiterWork < 100);
  assert.ok(work.estimatedNodes < 100);
  assert.equal(exceedsMarkdownDocParseBudget("# Title\n\nSome text."), false);
});

// ── delimiter work (text and table tokenizers) ────────────────────────────

test("delimiter work: false at exactly the cap", () => {
  // 4,096 `*` in one block is exactly 2^24 of delimiter work: 57 ms measured.
  const text = "*a*".repeat(2048);
  assert.equal(
    measureMarkdownDocParseWork(text).delimiterWork,
    MAX_MARKDOWN_DOC_DELIMITER_WORK,
  );
  assert.equal(exceedsMarkdownDocParseBudget(text), false);
});

test("delimiter work: true one delimiter past the cap", () => {
  // 4,097 delimiters in one block: (2^12 + 1)^2, one unit over.
  const text = `${"*a*".repeat(2048)}*`;
  assert.equal(measureMarkdownDocParseWork(text).delimiterWork, 4097 ** 2);
  assert.ok(4097 ** 2 > MAX_MARKDOWN_DOC_DELIMITER_WORK);
  assert.equal(exceedsMarkdownDocParseBudget(text), true);
});

test("delimiter work: refuses the critic's F4 emphasis one-liner", () => {
  // `"*a*"` × 20,000: 60,000 bytes, ONE line, no `[`, no `|`, depth 0 — every
  // counter the round-3 gate had said this was cheap, and it rendered in
  // 3,458 ms through renderMarkdownDocumentHtml with Export offered.
  const text = "*a*".repeat(20000);
  assert.ok(text.length < MAX_MARKDOWN_DOC_BYTES);
  assert.equal(text.split("\n").length, 1);
  assert.ok(!text.includes("["));
  assert.ok(!text.includes("|"));
  assert.equal(measureMarkdownDocParseWork(text).descentWork, 0);
  assert.equal(exceedsMarkdownDocParseBudget(text), true);
});

test("delimiter work: refuses code-span and raw-angle density too", () => {
  // The same axis, reached with different characters — the counter is the
  // whole CommonMark + GFM delimiter alphabet, not one construct.
  assert.equal(exceedsMarkdownDocParseBudget("`a` ".repeat(40000)), true);
  assert.equal(exceedsMarkdownDocParseBudget("<a> ".repeat(40000)), true);
  assert.equal(exceedsMarkdownDocParseBudget("~a~ ".repeat(40000)), true);
});

test("delimiter work: a blank line ends a block, so cost is per block", () => {
  // The same 8,192 delimiters cost 2^26 in one block and 2^24 across four —
  // which is why the counter is a sum of squares and not a total.
  const oneBlock = "*a*".repeat(4096);
  const fourBlocks = Array.from({ length: 4 }, () => "*a*".repeat(1024)).join(
    "\n\n",
  );
  assert.equal(measureMarkdownDocParseWork(oneBlock).delimiterWork, 8192 ** 2);
  assert.equal(
    measureMarkdownDocParseWork(fourBlocks).delimiterWork,
    4 * 2048 ** 2,
  );
  assert.equal(exceedsMarkdownDocParseBudget(oneBlock), true);
  assert.equal(exceedsMarkdownDocParseBudget(fourBlocks), false);
});

test("delimiter work: real documents keep their tables and prose", () => {
  const text = `# Report\n\n${gfmTable(12, 228)}\n\nUse \`a || b\` when either matches.\n`;
  assert.equal(exceedsMarkdownDocParseBudget(text), false);
});

// ── table cell markers (GFM table tokenizer) ──────────────────────────────
//
// Tables are the one construct whose cost is *not* per block: measured, it
// tracks the document-wide `|` count and nothing else, quadratically, whether
// those markers sit in one table or thirty-two.

test("table markers: false at exactly the cap", () => {
  // 12 columns × 235 rows is 3,081 markers, 133 ms measured — the widest
  // table still admitted. (One marker over the cap on a 12-column table is
  // a whole row, so the exact-cap shape is the narrower one below.)
  const text = gfmTable(2, 1022);
  assert.equal(
    measureMarkdownDocParseWork(text).tableCellMarkers,
    MAX_MARKDOWN_DOC_TABLE_CELL_MARKERS,
  );
  assert.equal(exceedsMarkdownDocParseBudget(text), false);
});

test("table markers: true one marker past the cap", () => {
  const text = `${gfmTable(2, 1022)}|`;
  assert.equal(
    measureMarkdownDocParseWork(text).tableCellMarkers,
    MAX_MARKDOWN_DOC_TABLE_CELL_MARKERS + 1,
  );
  assert.equal(exceedsMarkdownDocParseBudget(text), true);
});

test("table markers: refuses a wide table (round-3 F1 reproduction shape)", () => {
  // 300 columns × 100 rows: 62,194 bytes, 102 lines, no `[`, 6,902 ms.
  const text = gfmTable(300, 100);
  assert.ok(text.length < MAX_MARKDOWN_DOC_BYTES);
  assert.ok(!text.includes("["));
  assert.equal(exceedsMarkdownDocParseBudget(text), true);
});

test("table markers: are counted document-wide, not per block", () => {
  // 32 separate tables of 50 rows cost the same 5,720 ms as one table of
  // 1,600 rows, so splitting them must not buy a document past the cap.
  const text = `${gfmTable(12, 50)}\n`.repeat(32);
  assert.ok(
    measureMarkdownDocParseWork(text).tableCellMarkers >
      MAX_MARKDOWN_DOC_TABLE_CELL_MARKERS,
  );
  assert.equal(exceedsMarkdownDocParseBudget(text), true);
});

test("table markers: a document with prose-scale pipes is not penalized", () => {
  const text = `# Report\n\n${gfmTable(4, 40)}\n\nUse \`a || b\` when either matches.\n`;
  assert.equal(exceedsMarkdownDocParseBudget(text), false);
});

// ── table cell work (mdast→hast row padding) ──────────────────────────────
//
// `mdast-util-to-hast` pads every table body row out to the *header's* column
// count. mdast carries only the cells that were written; hast carries
// `rows × columns`. So a table whose header declares many columns and whose
// body rows carry no `|` at all is invisible to every counter above — the
// marker count finds only the header's pipes, and the parsed tree the node
// budget walks holds about three nodes per row. This was the round-4 blind
// critic's F6. `markdownParseBudget.test.mjs` asserts that this counter
// equals the `<td>` count the production render emits; these bound it.

test("table cell work: false at exactly the cap", () => {
  // 16 columns × 1,024 rows is exactly 16,384 padded cells: 131 ms measured
  // through `renderMarkdownDocumentHtml`, the largest table admitted.
  const text = pipelessTable(16, 1024);
  assert.equal(
    measureMarkdownDocParseWork(text).tableCellWork,
    MAX_MARKDOWN_DOC_TABLE_CELL_WORK,
  );
  assert.equal(exceedsMarkdownDocParseBudget(text), false);
});

test("table cell work: true one row past the cap", () => {
  const text = pipelessTable(16, 1025);
  assert.equal(
    measureMarkdownDocParseWork(text).tableCellWork,
    MAX_MARKDOWN_DOC_TABLE_CELL_WORK + 16,
  );
  assert.equal(exceedsMarkdownDocParseBudget(text), true);
});

test("table cell work: refuses the round-4 critic's F6 table", () => {
  // 1,500 columns × 2,000 pipeless rows: 14,894 bytes — 0.7% of the byte cap
  // — and every other counter says it is cheap. Rendered, it is 3,000,000
  // `<td>` elements: 8,813 ms on the panel-open path measured, and a 4 GB
  // heap exhausted at 1,000 × 5,900.
  const text = pipelessTable(1500, 2000);
  const work = measureMarkdownDocParseWork(text);
  assert.ok(text.length < MAX_MARKDOWN_DOC_BYTES);
  assert.ok(work.tableCellMarkers <= MAX_MARKDOWN_DOC_TABLE_CELL_MARKERS);
  assert.ok(work.estimatedNodes <= MAX_MARKDOWN_DOC_ESTIMATED_NODES);
  assert.equal(work.descentWork, 0);
  assert.equal(work.delimiterWork, 0);
  assert.equal(work.tableCellWork, 3_000_000);
  assert.equal(exceedsMarkdownDocParseBudget(text), true);
});

test("table cell work: an ordinary table is admitted, written out or not", () => {
  // 12 × 200 is the same 2,400 cells whether the rows write them or not —
  // which is the point: the count is of the cells the conversion emits.
  assert.equal(
    measureMarkdownDocParseWork(pipelessTable(12, 200)).tableCellWork,
    2400,
  );
  assert.equal(
    measureMarkdownDocParseWork(gfmTable(12, 200)).tableCellWork,
    2400,
  );
  assert.equal(exceedsMarkdownDocParseBudget(pipelessTable(12, 200)), false);
  assert.equal(exceedsMarkdownDocParseBudget(gfmTable(12, 200)), false);
});

test("table cell work: is summed document-wide, not taken per table", () => {
  // Two tables of 100 × 100 cost what one table of 20,000 cells costs, so
  // splitting a table must not buy a document past the cap.
  const text = `${pipelessTable(100, 100)}\n${pipelessTable(100, 100)}`;
  assert.equal(measureMarkdownDocParseWork(text).tableCellWork, 20_000);
  assert.equal(exceedsMarkdownDocParseBudget(text), true);
});

test("table cell work: a blank line ends the body, as it does in the parser", () => {
  // Verified against the render: a 10 × 3 table followed by a blank line and
  // five more lines emits 30 `<td>`, not 80 — the lines after the blank are a
  // paragraph. A model that kept counting them would refuse real documents.
  const text = `${pipelessTable(1000, 0)}\n${"x\n".repeat(4000)}`;
  assert.equal(measureMarkdownDocParseWork(text).tableCellWork, 0);
});

test("table cell work: a delimiter row of another width is not a table", () => {
  // Verified against the render: this emits no `<td>` at all — GFM requires
  // the delimiter row to have the header's cell count, so the whole block
  // stays a paragraph.
  const text = `|a|b|c|\n|-|-|\n${"x\n".repeat(2000)}`;
  assert.equal(measureMarkdownDocParseWork(text).tableCellWork, 0);
  assert.equal(exceedsMarkdownDocParseBudget(text), false);
});

test("table cell work: prose that merely carries a pipe opens no table", () => {
  const text = "# Report\n\nSome | pipe in prose.\n\n---\n\nmore text\n";
  assert.equal(measureMarkdownDocParseWork(text).tableCellWork, 0);
});

// ── descent work (document tokenizer) ─────────────────────────────────────

test("descent work: false at exactly the cap", () => {
  // Depth 22 held across 2,978 lines is 65,516 units: 155 ms measured.
  const text = quotedAtDepth(22, 2978);
  const work = measureMarkdownDocParseWork(text);
  assert.equal(work.descentWork, 22 * 2978);
  assert.ok(work.descentWork <= MAX_MARKDOWN_DOC_DESCENT_WORK);
  assert.equal(exceedsMarkdownDocParseBudget(text), false);
});

test("descent work: true one unit past the cap", () => {
  const lines = MAX_MARKDOWN_DOC_DESCENT_WORK + 1;
  const text = quotedAtDepth(1, lines);
  assert.equal(measureMarkdownDocParseWork(text).descentWork, lines);
  assert.equal(exceedsMarkdownDocParseBudget(text), true);
});

test("descent work: refuses the critic's F5 held-depth block quote", () => {
  // Depth 127 across 3,000 lines: 767,999 bytes, 130 mdast nodes — so no
  // node budget can see it — and 903 ms. The round-3 gate bounded the
  // deepest *line* (128) and admitted this outright.
  const text = quotedAtDepth(127, 3000);
  assert.ok(text.length < MAX_MARKDOWN_DOC_BYTES);
  assert.ok(!text.includes("["));
  assert.ok(!text.includes("|"));
  assert.equal(exceedsMarkdownDocParseBudget(text), true);
});

test("descent work: refuses a deeply nested list (round-3 F1 shape)", () => {
  const text = nestedList(800);
  assert.equal(measureMarkdownDocParseWork(text).descentWork, (799 * 800) / 2);
  assert.equal(exceedsMarkdownDocParseBudget(text), true);
});

test("descent work: a tab counts as four indent columns", () => {
  // A tab-indented document must not buy depth at one character per level.
  const text = `${"\t".repeat(200)}- deep\n`.repeat(200);
  assert.equal(measureMarkdownDocParseWork(text).descentWork, 200 * 400);
  assert.equal(exceedsMarkdownDocParseBudget(text), true);
});

test("descent work: an ordinary indented document stays under the cap", () => {
  const text = `# Title\n\n- one\n  - two\n    - three\n\n\`\`\`js\n${" ".repeat(40)}const x = 1;\n\`\`\`\n`;
  assert.equal(exceedsMarkdownDocParseBudget(text), false);
});

// ── node estimate (keeps the node budget's own parse finite) ──────────────

test("node estimate: false at exactly the cap", () => {
  const lines = MAX_MARKDOWN_DOC_ESTIMATED_NODES / 3;
  const text = Array.from({ length: lines }, () => "a").join("\n");
  assert.equal(
    measureMarkdownDocParseWork(text).estimatedNodes,
    MAX_MARKDOWN_DOC_ESTIMATED_NODES,
  );
  assert.equal(exceedsMarkdownDocParseBudget(text), false);
});

test("node estimate: true one line past the cap", () => {
  const lines = MAX_MARKDOWN_DOC_ESTIMATED_NODES / 3 + 1;
  const text = Array.from({ length: lines }, () => "a").join("\n");
  assert.equal(exceedsMarkdownDocParseBudget(text), true);
});

test("node estimate: refuses a flat list that would exhaust the heap", () => {
  // `"- a\n"` repeated to the 2 MiB byte cap has no delimiters and no
  // descent; parsing it to the tree the node budget would reject exhausts a
  // 4 GB Node heap outright. This is the counter that keeps the node
  // budget's own parse finite.
  const text = "- a\n".repeat(524288);
  assert.equal(text.length, MAX_MARKDOWN_DOC_BYTES);
  assert.equal(measureMarkdownDocParseWork(text).delimiterWork, 0);
  assert.equal(measureMarkdownDocParseWork(text).descentWork, 0);
  assert.equal(exceedsMarkdownDocParseBudget(text), true);
});

test("node estimate: counts GFM literal autolinks, which have no delimiter", () => {
  // `www.`, `://` and `@` are the only inline constructs that start on
  // ordinary word characters, so the delimiter counter cannot see them.
  // 60,000 of them is 960,000 bytes, one line, 180,001 mdast nodes and
  // 6,752 ms — admitted by every round-3 counter.
  for (const unit of ["www.example.com ", "http://e.co ", "a@b.co "]) {
    const text = unit.repeat(60000);
    assert.ok(text.length < MAX_MARKDOWN_DOC_BYTES);
    assert.equal(measureMarkdownDocParseWork(text).delimiterWork, 0);
    assert.equal(exceedsMarkdownDocParseBudget(text), true);
  }
});

test("node estimate: a long line-count document is no longer refused for its lines", () => {
  // The round-3 gate refused any document over 3,000 lines, which refused
  // real 9,600-line READMEs. Lines are only a third of the estimate now.
  const text = "prose line\n".repeat(9600);
  assert.ok(text.split("\n").length > 9000);
  assert.equal(exceedsMarkdownDocParseBudget(text), false);
});
