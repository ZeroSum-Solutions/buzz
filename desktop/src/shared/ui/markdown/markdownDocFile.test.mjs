import assert from "node:assert/strict";
import { test } from "node:test";

import {
  decodeMarkdownDocBytes,
  isMarkdownDocFilename,
  isMarkdownDocTooComplexForPreview,
  MAX_MARKDOWN_DOC_BYTES,
  MAX_MARKDOWN_DOC_PREVIEW_LINES,
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
