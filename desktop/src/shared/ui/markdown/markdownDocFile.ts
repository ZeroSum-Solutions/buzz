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
 * Whether decoded markdown text is safe to run through the full Preview
 * parse. A single scan is linear and cheap — nothing like the parse it is
 * gating — so it is safe to run unconditionally before the expensive path.
 */
export function isMarkdownDocTooComplexForPreview(text: string): boolean {
  let lines = 1;
  let linkMarkers = 0;
  for (let i = 0; i < text.length; i++) {
    const code = text.charCodeAt(i);
    if (code === 10 /* "\n" */) {
      lines++;
      if (lines > MAX_MARKDOWN_DOC_PREVIEW_LINES) return true;
    } else if (code === 91 /* "[" */) {
      linkMarkers++;
      if (linkMarkers > MAX_MARKDOWN_DOC_PREVIEW_LINK_MARKERS) return true;
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
