/**
 * The node budget that bounds a markdown document render.
 *
 * The source-text counters in `markdownDocFile.ts` bound the *parse*; this
 * bounds everything the parse feeds. Once micromark has produced a tree,
 * every later phase — the fork's own remark plugins, mdast→hast, React
 * element construction, and (on the export path) `renderToStaticMarkup` —
 * costs in proportion to the number of mdast nodes, and nothing before this
 * point counts them.
 *
 * With one measured exception, which this budget does *not* bound: a GFM
 * table. `mdast-util-to-hast` pads every body row out to the header's column
 * count, so a table can emit up to 668× the cells mdast carries while staying
 * far inside this cap. That shape is bounded before the parse instead, by
 * `MAX_MARKDOWN_DOC_TABLE_CELL_WORK` in `markdownDocFile.ts`, which counts
 * exactly the cells the conversion emits.
 *
 * Measured on this machine (M-series, Node 24.15.0, this branch's pinned
 * pipeline) through the production entry `renderMarkdownDocumentHtml`, over
 * the 2,456 real markdown files reachable from this repository:
 *
 * | document | bytes | nodes | render |
 * |---|---|---|---|
 * | `desktop/tests/fixtures/long-doc.md` | 506,681 | 117 | 116 ms |
 * | `docs/remote-agents.md` | 113,756 | 3,172 | 75 ms |
 * | `wry-0.55.1/CHANGELOG.md` | 145,455 | 7,093 | 111 ms |
 * | `sqlx-0.9.0/CHANGELOG.md` | 173,425 | 11,946 | 213 ms |
 * | `tokio-1.52.3/CHANGELOG.md` | 174,950 | 13,811 | 261 ms |
 * | `CHANGELOG.md` (this repo) | 355,823 | 15,059 | 328 ms |
 * | `qwen-results.md` (corpus maximum) | 216,815 | 18,013 | 382 ms |
 *
 * The cost is linear in node count across that range (≈21 µs/node), so the
 * cap is a real bound and not a proxy for one. 24,000 is 1.33× the largest
 * real document measured, which keeps every one of them renderable; the
 * corresponding worst case is ≈500 ms. A cap that held the *largest admitted*
 * document inside this app's 200 ms main-thread task budget would sit near
 * 11,000 nodes and would refuse `sqlx-0.9.0/CHANGELOG.md` (173 KB, a document
 * a user can plainly expect to read), so the budget trades that ceiling for
 * the feature working on real documents — see the deviation recorded in the
 * PR body. Every document in the corpus below 150 KB renders inside 200 ms.
 */
export const MAX_MARKDOWN_DOC_NODES = 24_000;

/**
 * Raised when a document's parsed tree exceeds `MAX_MARKDOWN_DOC_NODES`.
 *
 * Typed rather than a bare `Error` so the export path can tell "too complex"
 * (a bounded, expected refusal the reader is told about) apart from a genuine
 * render failure, without matching on message text.
 */
export class MarkdownTooComplexError extends Error {
  /** The budget that was exceeded. */
  readonly nodeBudget: number;

  constructor(nodeBudget: number) {
    super(`Markdown document exceeds the ${nodeBudget}-node render budget`);
    this.name = "MarkdownTooComplexError";
    this.nodeBudget = nodeBudget;
  }
}

/** Whether a thrown value is this module's budget refusal. */
export function isMarkdownTooComplexError(
  error: unknown,
): error is MarkdownTooComplexError {
  return error instanceof MarkdownTooComplexError;
}

/** Anything with mdast's shape: a node, with optional child nodes. */
type NodeLike = { children?: readonly NodeLike[] };

/**
 * Count the nodes of `tree`, stopping as soon as the budget is passed.
 *
 * Returns the count when the tree fits and `null` when it does not, so the
 * walk itself is bounded by the budget rather than by the tree — a tree with
 * a million nodes costs the same to reject as one with `budget + 1`.
 */
export function countMarkdownNodesWithinBudget(
  tree: NodeLike,
  budget: number,
): number | null {
  const stack: NodeLike[] = [tree];
  let count = 0;
  while (stack.length > 0) {
    const node = stack.pop();
    if (node === undefined) break;
    count += 1;
    if (count > budget) return null;
    const children = node.children;
    if (children !== undefined) {
      for (const child of children) stack.push(child);
    }
  }
  return count;
}

/**
 * A remark plugin that aborts the parse once the mdast tree passes the node
 * budget.
 *
 * It is applied as a `mdast-util-from-markdown` *transform* rather than as a
 * remark transformer, which puts it inside `processor.parse()`: it runs at
 * the end of `fromMarkdown`, before the tree is returned to unified and so
 * before every other remark plugin, before mdast→hast, and before any React
 * element exists. That is the earliest composable hook this parser offers —
 * `mdast-util-from-markdown` has no per-node creation hook (its `enter`/`exit`
 * maps are keyed by micromark token type and *replace* rather than wrap, so an
 * extension cannot observe node creation without shadowing the handlers that
 * do the creating), and micromark's tokenizer, which runs before any mdast
 * node exists, has no extension point at all. Bounding the tokenizer is
 * therefore the source-side work model's job (`markdownDocFile.ts`); this
 * bounds the tree.
 */
export function remarkMarkdownDocNodeBudget(
  this: {
    data: () => { fromMarkdownExtensions?: unknown[] };
  },
  options?: { budget?: number },
) {
  const budget = options?.budget ?? MAX_MARKDOWN_DOC_NODES;
  const data = this.data();
  const extensions = data.fromMarkdownExtensions ?? [];
  data.fromMarkdownExtensions = extensions;
  extensions.push({
    transforms: [
      (tree: NodeLike) => {
        if (countMarkdownNodesWithinBudget(tree, budget) === null) {
          throw new MarkdownTooComplexError(budget);
        }
        return tree;
      },
    ],
  });
}
