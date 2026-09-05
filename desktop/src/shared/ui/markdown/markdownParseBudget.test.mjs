/**
 * The node budget: the guard that gates on the *parsed tree* rather than on
 * the source text.
 *
 * The source-side work model (`markdownDocFile.ts`) bounds micromark's own
 * tokenizers, whose cost is spent before any mdast node exists. Everything
 * after that — the fork's remark plugins, mdast→hast, React element
 * construction, `renderToStaticMarkup` — costs in proportion to node count,
 * and only the tree knows what that is.
 *
 * These bind the production seam: `renderCachedMarkdown` is the single entry
 * every markdown surface in the app parses through, and `nodeBudget` is the
 * input document surfaces set on it.
 */
import assert from "node:assert/strict";
import { test } from "node:test";

import {
  documentModeComponents,
  isMarkdownDocumentTooComplex,
} from "./documentMode.tsx";
import { exceedsMarkdownDocParseBudget } from "./markdownDocFile.ts";
import {
  countMarkdownNodesWithinBudget,
  isMarkdownTooComplexError,
  MarkdownTooComplexError,
  MAX_MARKDOWN_DOC_NODES,
} from "./markdownParseBudget.ts";
import { renderCachedMarkdown } from "./nodeCache.ts";

/** A tree of exactly `count` nodes: a root with `count - 1` leaf children. */
function treeOfSize(count) {
  return {
    type: "root",
    children: Array.from({ length: count - 1 }, () => ({ type: "text" })),
  };
}

/**
 * Parse `content` through the production seam with `nodeBudget` applied.
 * A fresh `variant` per call keeps the module-level parse cache out of it.
 */
let variantCounter = 0;
function parseWithBudget(content, nodeBudget) {
  variantCounter += 1;
  return renderCachedMarkdown({
    components: documentModeComponents,
    content,
    hardLineBreaks: false,
    nodeBudget,
    variant: `budget-test-${variantCounter}`,
  });
}

/**
 * `n` one-line paragraphs, which is exactly `2n + 1` mdast nodes (a root,
 * `n` paragraphs, `n` text nodes) — an exact, parser-checked count to put a
 * boundary on.
 */
function paragraphs(n) {
  return "a\n\n".repeat(n);
}

// ── the counter ───────────────────────────────────────────────────────────

test("counts a tree exactly at the budget", () => {
  assert.equal(countMarkdownNodesWithinBudget(treeOfSize(1000), 1000), 1000);
});

test("refuses a tree one node past the budget", () => {
  assert.equal(countMarkdownNodesWithinBudget(treeOfSize(1001), 1000), null);
});

test("the walk is bounded by the budget, not by the tree", () => {
  // A tree three orders of magnitude past the budget must cost the same to
  // reject as one node past it, or the guard is itself the resource leak.
  assert.equal(countMarkdownNodesWithinBudget(treeOfSize(1_000_000), 8), null);
});

// ── the plugin, through the production parse seam ─────────────────────────

test("a document exactly at the budget parses", () => {
  const content = paragraphs(500);
  assert.doesNotThrow(() => parseWithBudget(content, 1001));
});

test("a document one node past the budget aborts the parse", () => {
  const content = paragraphs(500);
  assert.throws(
    () => parseWithBudget(content, 1000),
    (error) => {
      assert.ok(error instanceof MarkdownTooComplexError);
      assert.ok(isMarkdownTooComplexError(error));
      assert.equal(error.name, "MarkdownTooComplexError");
      assert.equal(error.nodeBudget, 1000);
      return true;
    },
  );
});

test("a surface that sets no budget is not gated", () => {
  // Chat messages parse through the same seam and must not start throwing:
  // a throw there takes out the timeline, not one attachment.
  assert.doesNotThrow(() => parseWithBudget(paragraphs(5000), undefined));
});

// ── the document gate ─────────────────────────────────────────────────────

test("the node budget refuses a document the source work model admits", () => {
  // 10,000 one-line list items: no delimiters, no table markers, no
  // container descent, and a source-side node estimate of 30,003 — inside
  // every cap the scan applies. The parsed tree is 30,002 nodes, past
  // `MAX_MARKDOWN_DOC_NODES`. Nothing but the count on the real tree refuses
  // this, so with the budget removed it is admitted and renders.
  const content = "- a\n".repeat(10000);
  assert.equal(exceedsMarkdownDocParseBudget(content), false);
  assert.equal(isMarkdownDocumentTooComplex(content), true);
});

test("the same shape inside the node budget is admitted", () => {
  // 7,000 items is 21,002 nodes. Bracketing the cap on the production path
  // keeps the constant honest: a budget raised past the tree it is meant to
  // refuse fails the test above, one lowered fails this one.
  const content = "- a\n".repeat(7000);
  assert.equal(exceedsMarkdownDocParseBudget(content), false);
  assert.equal(isMarkdownDocumentTooComplex(content), false);
});

test("a realistic 200 KB README is admitted", () => {
  const sections = [];
  for (let i = 0; i < 700; i++) {
    sections.push(
      `## Section ${i}\n\nSome prose about the ${i}th topic with a [link](https://example.invalid/${i}) and \`inline code\` and a fair amount of ordinary filler text so the paragraph is a realistic length for a project README that a person would read end to end.\n\n- first bullet for section ${i}\n- second bullet for section ${i}\n\n\`\`\`sh\nbuzz run --section ${i}\n\`\`\`\n\n`,
    );
  }
  const content = `# Project\n\n${sections.join("")}`;
  assert.ok(content.length > 200_000);
  assert.equal(exceedsMarkdownDocParseBudget(content), false);
  assert.equal(isMarkdownDocumentTooComplex(content), false);
});

test("the document gate refuses the round-3 critic's F4 and F5 shapes", () => {
  assert.equal(isMarkdownDocumentTooComplex("*a*".repeat(20000)), true);
  assert.equal(
    isMarkdownDocumentTooComplex(
      Array.from({ length: 3000 }, () => `${"> ".repeat(127)}x`).join("\n"),
    ),
    true,
  );
});

test("the exported budget is the one document mode applies", () => {
  // The bracket above is stated in list items; this states it in the
  // constant, so a change to `MAX_MARKDOWN_DOC_NODES` cannot silently leave
  // the two disagreeing.
  assert.equal(MAX_MARKDOWN_DOC_NODES, 24_000);
  assert.ok(3 * 7000 + 2 < MAX_MARKDOWN_DOC_NODES);
  assert.ok(3 * 10000 + 2 > MAX_MARKDOWN_DOC_NODES);
});
