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
 * Above this many lines, a full Preview render (react-markdown → remark →
 * mdast → micromark) is refused in favor of a bounded fallback.
 *
 * The byte cap above bounds memory, not parsed-node count: a flat list of
 * one-line items ("- a\n" repeated) parses at superlinear cost on the
 * project's pinned parser — measured 854ms at 16,384 items and climbing to
 * tens of seconds well under the byte cap, before any React element or DOM
 * node exists. 3,000 lines keeps the worst case (every line a separate
 * block) comfortably under the panel-ready budget even by the most
 * pessimistic (quadratic) extrapolation from those measurements, while
 * admitting any realistic long document — the branch's own long-doc
 * fixture is 506,681 bytes across only 122 lines. Code view is bounded
 * separately (`CodeBlock.tsx`'s highlighting caps) since it never runs the
 * mdast parse.
 */
export const MAX_MARKDOWN_DOC_PREVIEW_LINES = 3000;

/**
 * Above this many `[` characters (the opener shared by links, images,
 * footnotes, and reference-style links), a full Preview render is refused
 * even when the line-count gate above passes.
 *
 * The line-count gate only bounds *block*-level node count — it does
 * nothing for a single line densely packed with inline constructs.
 * Reproduced on this project's pinned parser (mdast-util-from-markdown +
 * micromark-extension-gfm@3.0.0 + mdast-util-gfm@3.1.0) with
 * `"[a](http://e.co) "` repeated on ONE line: 12,336 links (209,712 bytes,
 * 1 line — passes the line-count gate outright) parses in 351ms, already
 * over this app's 200ms main-thread budget, climbing to 9,379ms/1,105MB at
 * 111,025 links. 2,000 `[` markers extrapolates (by the same superlinear
 * curve measured across those points) to well under 100ms — the same
 * safety margin the line-count gate above uses — while a prose document
 * with occasional literal brackets stays nowhere near this count.
 */
export const MAX_MARKDOWN_DOC_PREVIEW_LINK_MARKERS = 2000;

/**
 * Above this many `|` characters — the cell delimiter GFM tables are built
 * from — a full Preview render is refused even when both gates above pass.
 *
 * A wide table is the shape neither gate above sees: it is few lines and has
 * no `[` at all, yet every cell is its own mdast node with its own inline
 * tokenizer run. Measured on this project's pinned parser through the export's
 * own render path (`renderMarkdownDocumentHtml`), the cost tracks the delimiter
 * count and not the shape that produced it — a 300-column table and a
 * 40-column one cost the same at the same delimiter count:
 *
 * | table | `|` markers | source bytes | lines | render |
 * |---|---|---|---|---|
 * | 12 × 228 | 2,990 | 5,994 | 231 | 163 ms |
 * | 300 × 8 | 3,010 | 6,810 | 10 | 106 ms |
 * | 80 × 50 | 4,212 | 8,574 | 52 | 227 ms |
 * | 40 × 200 | 8,282 | 16,634 | 202 | 681 ms |
 * | 300 × 100 | 30,702 | 62,194 | 102 | 7,622 ms |
 * | 300 × 600 | 181,502 | 363,194 | 602 | 1,138,668 ms |
 *
 * Every one of those is under the 2 MiB byte cap and under both gates above.
 * 3,000 markers keeps the widest document still admitted at 163 ms measured —
 * inside this app's 200 ms main-thread budget — while admitting any table a
 * document realistically carries (a 12-column table needs 230 rows to reach
 * it).
 */
export const MAX_MARKDOWN_DOC_PREVIEW_TABLE_CELL_MARKERS = 3000;

/**
 * Above this container nesting depth on any one line, a full Preview render is
 * refused even when every gate above passes.
 *
 * Nesting is the second shape none of the counts above bound: cost grows with
 * the depth of the container stack the parser has to re-enter on every line,
 * not with the number of lines or inline markers. Measured the same way, a
 * list nested one level per line: depth 100 renders in 36 ms, depth 200 in
 * 147 ms, depth 400 in 953 ms, and depth 800 in 11,066 ms — 801 lines,
 * 647,890 bytes, not one `[` or `|`, so it passes every other gate outright.
 *
 * Depth is counted from each line's leading run of indentation and `>`
 * markers (two indent columns per level, one per `>`, whose single following
 * space belongs to the marker), which is a cheap
 * over-estimate: it counts indentation inside a fenced code block too, where
 * nothing is nested. 128 leaves the deepest document still admitted at 63 ms
 * measured, and stays clear of real documents — the deepest leading
 * indentation in the 2,474 markdown files in this repository is 51 levels.
 */
export const MAX_MARKDOWN_DOC_PREVIEW_NESTING_DEPTH = 128;

/**
 * Whether decoded markdown text is safe to run through the full Preview
 * parse. A single scan is linear and cheap — nothing like the parse it is
 * gating — so it is safe to run unconditionally before the expensive path.
 *
 * Every counter below bounds a quantity the parse actually spends time on:
 * block nodes (lines), inline constructs (`[`), table cells (`|`), and
 * container depth. Bytes bound none of them, which is why the byte cap above
 * is not this check.
 */
export function isMarkdownDocTooComplexForPreview(text: string): boolean {
  let lines = 1;
  let linkMarkers = 0;
  let tableCellMarkers = 0;
  // Leading indentation columns and `>` markers of the line being scanned;
  // together they give the container depth the parser has to descend.
  let inLinePrefix = true;
  let prefixColumns = 0;
  let prefixQuotes = 0;
  // A block-quote marker consumes one following space (CommonMark), so that
  // space is part of the marker and not indentation of its own.
  let quoteSpacePending = false;
  const tooDeep = () =>
    (prefixColumns >> 1) + prefixQuotes >
    MAX_MARKDOWN_DOC_PREVIEW_NESTING_DEPTH;
  for (let i = 0; i < text.length; i++) {
    const code = text.charCodeAt(i);
    if (code === 10 /* "\n" */) {
      lines++;
      if (lines > MAX_MARKDOWN_DOC_PREVIEW_LINES) return true;
      inLinePrefix = true;
      prefixColumns = 0;
      prefixQuotes = 0;
      quoteSpacePending = false;
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
        if (tooDeep()) return true;
        continue;
      }
      // First character of the line's content: the prefix is complete.
      inLinePrefix = false;
      quoteSpacePending = false;
      if (tooDeep()) return true;
    }
    if (code === 91 /* "[" */) {
      linkMarkers++;
      if (linkMarkers > MAX_MARKDOWN_DOC_PREVIEW_LINK_MARKERS) return true;
    } else if (code === 124 /* "|" */) {
      tableCellMarkers++;
      if (tableCellMarkers > MAX_MARKDOWN_DOC_PREVIEW_TABLE_CELL_MARKERS) {
        return true;
      }
    }
  }
  return false;
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
