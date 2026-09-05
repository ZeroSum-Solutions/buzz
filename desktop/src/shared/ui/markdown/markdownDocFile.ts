/**
 * Pure classification and decoding for viewable markdown document
 * attachments.
 *
 * A markdown file uploaded to the relay has no magic bytes, so Blossom
 * stores it as `application/octet-stream` under a `{sha256}.bin` blob key —
 * the original `.md` name survives only in the message's imeta `filename`
 * field. Classification therefore keys off the imeta filename, never the
 * blob-URL extension or the MIME type.
 *
 * Kept DOM-free (TextDecoder is available in both the webview and Node)
 * so the branch logic is unit-testable without a webview.
 */

/** Filename extensions rendered by the in-app markdown viewer. */
const MARKDOWN_DOC_EXTENSIONS = [".md", ".markdown", ".mdx"] as const;

/**
 * Maximum attachment size the viewer will render. Larger files fall back
 * to the download card path.
 *
 * This constant powers the untrusted-imeta pre-gate (UX only) and the
 * defense-in-depth decode check. The *enforcement* boundary is the native
 * `fetch_markdown_doc_bytes` command's matching `MAX_MARKDOWN_DOC_BYTES`
 * cap in `media_download.rs`, which refuses oversized documents during the
 * streamed fetch — keep the two in sync.
 */
export const MAX_MARKDOWN_DOC_BYTES = 2 * 1024 * 1024;

/**
 * Work model for micromark's *document* tokenizer: the sum, over lines, of
 * the container depth that line sits at.
 *
 * The tokenizer re-enters every open container once per line, so its cost is
 * depth × lines and not either one alone. Bounding the deepest *line*
 * instead — which this gate did until the parsed-tree budget replaced it —
 * measured the wrong shape: a list nested one level per line reaches depth
 * 128 in 128 lines, but the same depth held across 2,999 lines is 767,743
 * bytes and 903 ms with only 130 mdast nodes to show for it, so no
 * node-count bound can see it either.
 *
 * Measured on this machine through `renderMarkdownDocumentHtml`, block quotes
 * held at a constant depth across 2,999 lines:
 *
 * | depth × lines | descent work | bytes | render |
 * |---|---|---|---|
 * | 16 × 2,999 | 47,984 | 101,965 | 121 ms |
 * | 22 × 2,978 | 65,516 | 136,987 | 155 ms |
 * | 32 × 2,999 | 95,968 | 197,933 | 213 ms |
 * | 127 × 2,999 | 380,873 | 767,743 | 903 ms |
 *
 * and lists nested one level per line: depth 362 (65,341 descent work) is
 * 712 ms, depth 800 (319,600) is 7,412 ms. The list shape is the more
 * expensive of the two per unit of descent, so the cap is set from it:
 * 65,536 keeps the block-quote worst case at 155 ms measured and the list
 * worst case at 712 ms, against a maximum of 1,776 across the 2,456 real
 * markdown files in this repository — 37× headroom over anything real.
 */
export const MAX_MARKDOWN_DOC_DESCENT_WORK = 65_536;

/**
 * Work model for micromark's *text* tokenizer: the sum, over
 * blank-line-separated blocks, of the square of the inline-delimiter count in
 * that block.
 *
 * The inline resolvers pair their delimiters against each other within one
 * block — `resolveAllAttention` walks every opener against every closer — so
 * the cost is quadratic *per block*, which is why neither a per-document
 * count of one delimiter (`[`, as this gate had) nor a per-document total of
 * all of them predicts it. The delimiters counted are the CommonMark inline
 * construct starts plus GFM strikethrough, which is a closed set: `!`, `&`,
 * `*`, `<`, `[`, `\`, `]`, `_`, `` ` ``, `~`. (GFM's literal autolinks start
 * on ordinary word characters instead; they are bounded by the node estimate
 * below, since their cost is linear and their node yield is not. Table cells
 * are bounded separately, below, because their cost is *not* per block.)
 *
 * Measured on this machine (M-series, Node 24.15.0) through the pinned
 * parser, `"*a*"` repeated:
 *
 * | shape | delimiter work | parse |
 * |---|---|---|
 * | 16 blocks × 512 | 4,194,304 | 24 ms |
 * | 1 block × 4,096 | 16,777,216 | 33 ms |
 * | 4 blocks × 2,048 | 16,777,216 | 69 ms |
 * | 1 block × 8,192 | 67,108,864 | 142 ms |
 * | 8 blocks × 4,096 | 134,217,728 | 255 ms |
 *
 * and through the production entry `renderMarkdownDocumentHtml`:
 * `"*a*"` × 2,048 (2^24 of work) is 57 ms, × 5,000 (10^8) is 260 ms, and
 * × 20,000 — the round-3 blind critic's F4 shape, 60 KB on one line with no
 * `[` and no `|` — is 1.6 × 10^9 and 3,458 ms.
 *
 * 16,777,216 (2^24) keeps the worst measured document still admitted at
 * 69 ms, inside this app's 200 ms main-thread task budget, against a maximum
 * of 7,333,264 across the 2,456 real markdown files in this repository.
 */
export const MAX_MARKDOWN_DOC_DELIMITER_WORK = 16_777_216;

/**
 * Cap on the `|` characters a document may carry, document-wide.
 *
 * GFM tables are the one construct whose cost is *not* per block: measured,
 * it tracks the document's total cell-delimiter count and nothing else —
 * quadratically, and identically whether those delimiters sit in one table or
 * thirty-two, adjacent or separated by prose.
 *
 * | shape | `|` markers | render |
 * |---|---|---|
 * | 40 columns × 74 rows | 3,116 | 108 ms |
 * | 12 columns × 236 rows | 3,094 | 115 ms |
 * | 12 columns × 235 rows | 3,081 | 133 ms |
 * | 4 columns × 767 rows | 3,845 | 145 ms |
 * | 12 columns × 314 rows | 4,108 | 202 ms |
 * | 8 tables × 39 rows, prose between | 4,264 | 235 ms |
 * | 32 tables × 50 rows | 21,632 | 5,720 ms |
 * | 300 columns × 100 rows | 30,702 | 6,902 ms |
 *
 * 3,072 keeps the worst measured document still admitted at 133 ms. Across
 * the 2,456 real markdown files in this repository exactly one carries more
 * (a generated 217 KB API listing at 8,876), and it falls back to the
 * download card — the same trade the line gate this replaced already made,
 * at a cap 2% higher than the one it shipped with.
 */
export const MAX_MARKDOWN_DOC_TABLE_CELL_MARKERS = 3_072;

/**
 * Source-side approximation of the parsed-tree node count, used only to
 * bound the parse the real budget (`MAX_MARKDOWN_DOC_NODES`, enforced on the
 * tree in `markdownParseBudget.ts`) has to run before it can refuse.
 *
 * Every mdast node is either a block (which needs at least one line), an
 * inline construct (which needs a delimiter, counted above, or a GFM literal
 * autolink), or a text node between them — so
 * `3 × lines + delimiters + 2 × table cell markers + 3 × autolink candidates`
 * tracks the node count within a small constant factor. Measured against the
 * real tree: 0.67–1.0 on the adversarial shapes (flat lists, literal
 * autolinks, setext headings, emphasis runs, tables) and 0.4–0.99 across the
 * 2,456 real markdown files in this repository, whose maximum estimate is
 * 38,241 (a generated 217 KB API listing with 18,013 real nodes).
 *
 * Without this cap the node budget's refusal is unbounded: `"- a\n"` repeated
 * to the 2 MiB byte cap has no delimiters, no descent and one line per item,
 * and parsing it to the tree the budget would reject exhausts a 4 GB heap
 * (measured: `FATAL ERROR: Ineffective mark-compacts near heap limit`).
 * 48,000 is twice the node budget, so nothing the exact count would admit is
 * refused here first, and 1.26× the largest real estimate; the worst parse it
 * admits — 16,000 one-line list items, 48,002 nodes — refuses in 913–1,016 ms
 * measured. That is the only path on which a refusal is not immediate; every
 * other shape above is refused by a scan, in under 13 ms end to end.
 */
export const MAX_MARKDOWN_DOC_ESTIMATED_NODES = 48_000;

/**
 * The four parse-work quantities a single linear scan of the source can
 * measure. Exported for the unit tests, which assert the caps against the
 * numbers rather than only against the predicate.
 */
export type MarkdownDocParseWork = {
  /** Σ over lines of that line's container depth. */
  descentWork: number;
  /** Σ over blank-line-separated blocks of (inline delimiters in block)². */
  delimiterWork: number;
  /** `|` characters, document-wide. */
  tableCellMarkers: number;
  /** 3 × lines + delimiters + 3 × literal-autolink candidates. */
  estimatedNodes: number;
};

/** Character codes that begin a CommonMark or GFM inline construct. */
function isDelimiterCode(code: number): boolean {
  return (
    code === 33 /* ! */ ||
    code === 38 /* & */ ||
    code === 42 /* * */ ||
    code === 60 /* < */ ||
    code === 91 /* [ */ ||
    code === 92 /* \ */ ||
    code === 93 /* ] */ ||
    code === 95 /* _ */ ||
    code === 96 /* ` */ ||
    code === 126 /* ~ */
  );
}

/**
 * Measure the parse work `text` implies, in one linear pass.
 *
 * Linear and allocation-free — nothing like the parse it precedes — so it is
 * safe to run unconditionally before the expensive path.
 */
export function measureMarkdownDocParseWork(
  text: string,
): MarkdownDocParseWork {
  let lines = 1;
  let delimiters = 0;
  let tableCellMarkers = 0;
  let autolinkCandidates = 0;
  let descentWork = 0;
  let delimiterWork = 0;
  // Delimiters seen since the last blank line: one micromark text block.
  let blockDelimiters = 0;
  // Leading indentation columns and `>` markers of the line being scanned;
  // together they give the container depth the parser has to descend.
  let inLinePrefix = true;
  let prefixColumns = 0;
  let prefixQuotes = 0;
  // A block-quote marker consumes one following space (CommonMark), so that
  // space is part of the marker and not indentation of its own.
  let quoteSpacePending = false;
  let lineHasContent = false;

  const endLine = () => {
    descentWork += (prefixColumns >> 1) + prefixQuotes;
    if (!lineHasContent) {
      // A blank line closes the block: bank its quadratic cost and reset.
      delimiterWork += blockDelimiters * blockDelimiters;
      blockDelimiters = 0;
    }
  };

  for (let i = 0; i < text.length; i++) {
    const code = text.charCodeAt(i);
    if (code === 10 /* "\n" */) {
      lines++;
      endLine();
      inLinePrefix = true;
      prefixColumns = 0;
      prefixQuotes = 0;
      quoteSpacePending = false;
      lineHasContent = false;
      continue;
    }
    if (inLinePrefix) {
      if (code === 32 /* " " */) {
        if (quoteSpacePending) quoteSpacePending = false;
        else prefixColumns++;
        continue;
      }
      if (code === 9 /* "\t" */) {
        quoteSpacePending = false;
        // A tab advances to the next four-column stop; four is markdown's own
        // tab width, and over-counting here only makes the gate stricter.
        prefixColumns += 4;
        continue;
      }
      if (code === 62 /* ">" */) {
        prefixQuotes++;
        quoteSpacePending = true;
        continue;
      }
      // First character of the line's content: the prefix is complete.
      inLinePrefix = false;
      quoteSpacePending = false;
      lineHasContent = true;
    }
    if (isDelimiterCode(code)) {
      delimiters++;
      blockDelimiters++;
    } else if (code === 124 /* "|" */) {
      tableCellMarkers++;
    } else if (code === 64 /* "@", email autolink */) {
      autolinkCandidates++;
    } else if (
      code === 58 /* ":" */ &&
      text.charCodeAt(i + 1) === 47 /* "/" */ &&
      text.charCodeAt(i + 2) === 47 /* "/" */
    ) {
      autolinkCandidates++;
    } else if (
      code === 119 /* "w" */ &&
      text.charCodeAt(i + 1) === 119 &&
      text.charCodeAt(i + 2) === 119 &&
      text.charCodeAt(i + 3) === 46 /* "." */
    ) {
      autolinkCandidates++;
    }
  }
  endLine();
  delimiterWork += blockDelimiters * blockDelimiters;

  return {
    descentWork,
    delimiterWork,
    tableCellMarkers,
    // A table cell delimiter yields about two nodes, so it counts twice.
    estimatedNodes:
      3 * lines + delimiters + 2 * tableCellMarkers + 3 * autolinkCandidates,
  };
}

/**
 * Whether decoded markdown text costs more to *parse* than the caps above
 * allow.
 *
 * This is the cheap pre-filter, not the guard: it bounds micromark's own
 * tokenizers, which run before any mdast node exists and so before
 * `MAX_MARKDOWN_DOC_NODES` can see anything. The guard is
 * `isMarkdownDocumentTooComplex` in `documentMode.tsx`, which runs this and
 * then the parse, with the node budget enforced inside it.
 */
export function exceedsMarkdownDocParseBudget(text: string): boolean {
  return exceedsMarkdownDocParseWorkBudget(measureMarkdownDocParseWork(text));
}

/** `exceedsMarkdownDocParseBudget` for a measurement already taken. */
export function exceedsMarkdownDocParseWorkBudget(
  work: MarkdownDocParseWork,
): boolean {
  return (
    work.descentWork > MAX_MARKDOWN_DOC_DESCENT_WORK ||
    work.delimiterWork > MAX_MARKDOWN_DOC_DELIMITER_WORK ||
    work.tableCellMarkers > MAX_MARKDOWN_DOC_TABLE_CELL_MARKERS ||
    work.estimatedNodes > MAX_MARKDOWN_DOC_ESTIMATED_NODES
  );
}

/** Whether an imeta filename should open in the in-app markdown viewer. */
export function isMarkdownDocFilename(filename: string): boolean {
  const lower = filename.trim().toLowerCase();
  return MARKDOWN_DOC_EXTENSIONS.some(
    (extension) => lower.endsWith(extension) && lower.length > extension.length,
  );
}

export type MarkdownDocDecodeResult =
  | { kind: "ok"; text: string }
  | { kind: "too-large" }
  | { kind: "binary" };

/**
 * Decode fetched attachment bytes for the viewer.
 *
 * Strict UTF-8: a file that merely *claims* to be markdown by name but is
 * actually binary fails decoding and reports `binary`, so the panel can fall
 * back to the download action instead of rendering mojibake.
 */
export function decodeMarkdownDocBytes(
  bytes: Uint8Array,
): MarkdownDocDecodeResult {
  if (bytes.byteLength > MAX_MARKDOWN_DOC_BYTES) {
    return { kind: "too-large" };
  }
  try {
    const text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
    return { kind: "ok", text };
  } catch {
    return { kind: "binary" };
  }
}
