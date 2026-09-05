import { invokeTauri } from "@/shared/api/tauri";
import { renderMarkdownDocumentHtml } from "@/shared/ui/markdown/documentMode";
import { exceedsMarkdownDocParseBudget } from "@/shared/ui/markdown/markdownDocFile";
import { isMarkdownTooComplexError } from "@/shared/ui/markdown/markdownParseBudget";

/**
 * Mirror of `MAX_DOCUMENT_HTML_BYTES` in
 * `src-tauri/src/commands/pdf_export.rs`. The Rust cap is the authoritative
 * one — it is what protects the browser process — but refusing here too turns
 * an oversized document into a message the reader understands instead of a
 * backend error string.
 */
export const MAX_PDF_DOCUMENT_HTML_BYTES = 8 * 1024 * 1024;

/**
 * Cap on the markdown source, checked before it is rendered. It matches the
 * viewer's own native 2 MiB fetch cap, so it refuses nothing the panel could
 * have loaded.
 *
 * Bytes are *not* the quantity the render costs, and this cap alone does not
 * bound it: the export runs the same micromark/mdast parse the Preview does,
 * whose cost tracks the parser's own work — delimiter density per block,
 * container descent per line, and the parsed node count — none of which bytes
 * predict. Measured on this branch's pipeline, node density across the 2,456
 * real markdown files in this repository and the adversarial shapes ranges
 * from 1.3 to 4,300 bytes per node, so no byte cap separates them. The bounds
 * that matter are `exceedsMarkdownDocParseBudget` (the parse) and
 * `MAX_MARKDOWN_DOC_NODES` (the tree), both applied below.
 */
export const MAX_PDF_DOCUMENT_SOURCE_BYTES = 2 * 1024 * 1024;

/** Mirror of `MAX_TITLE_CHARS` in `pdf_export.rs`. */
export const MAX_PDF_DOCUMENT_TITLE_CHARS = 200;

export const PDF_DOCUMENT_TOO_LARGE_MESSAGE =
  "This document is too large to export as a PDF.";

export const PDF_DOCUMENT_TOO_COMPLEX_MESSAGE =
  "This document has too many elements to render as a PDF. Download it instead.";

/**
 * The document title placed in the exported PDF's head: the attachment's
 * basename without its markdown extension, bounded to the same length the
 * Rust command accepts.
 */
export function pdfDocumentTitle(filename: string): string {
  const base = filename.split(/[\\/]/).pop() ?? filename;
  const withoutExtension = base.replace(/\.(?:md|markdown|mdx)$/i, "");
  const cleaned = withoutExtension
    .split("")
    .filter((char) => char >= " " || char === "\t")
    .join("")
    .trim();
  const bounded = cleaned.slice(0, MAX_PDF_DOCUMENT_TITLE_CHARS).trim();
  return bounded.length > 0 ? bounded : "Document";
}

/** UTF-8 byte length, which is what the Rust cap counts. */
function utf8ByteLength(value: string): number {
  return new TextEncoder().encode(value).length;
}

export type ExportMarkdownDocumentToPdfArgs = {
  /** The markdown source shown in the viewer. */
  content: string;
  /** The attachment's filename, used for the title and the suggested name. */
  filename: string;
};

/**
 * Render the document in document mode and hand it to the Rust exporter.
 *
 * Resolves `true` when a PDF was written and `false` when the user cancelled
 * the save dialog. Every other failure — an oversized document, one too
 * complex to parse, a missing browser, a failed write — rejects with a message
 * the caller surfaces.
 */
export async function exportMarkdownDocumentToPdf({
  content,
  filename,
}: ExportMarkdownDocumentToPdfArgs): Promise<boolean> {
  if (utf8ByteLength(content) > MAX_PDF_DOCUMENT_SOURCE_BYTES) {
    throw new Error(PDF_DOCUMENT_TOO_LARGE_MESSAGE);
  }
  // The cheap half of the gate the panel applies to Preview, on the identical
  // parse: `renderMarkdownDocumentHtml` is `renderCachedMarkdown` with a
  // different component map, so a document the panel refuses to preview would
  // cost exactly as much here, on the same main thread, with a save dialog yet
  // to appear. The panel hides the Export action for these documents; this is
  // the enforcing check.
  if (exceedsMarkdownDocParseBudget(content)) {
    throw new Error(PDF_DOCUMENT_TOO_COMPLEX_MESSAGE);
  }
  // The other half is enforced inside the parse itself: document mode carries
  // the node budget, so an over-budget document aborts in
  // `processor.parse()` — before the hast conversion, before any React
  // element, and before the save dialog — and is reported as the same bounded
  // refusal rather than as a render failure.
  let bodyHtml: string;
  try {
    bodyHtml = await renderMarkdownDocumentHtml(content);
  } catch (error) {
    if (isMarkdownTooComplexError(error)) {
      throw new Error(PDF_DOCUMENT_TOO_COMPLEX_MESSAGE);
    }
    throw error;
  }
  if (utf8ByteLength(bodyHtml) > MAX_PDF_DOCUMENT_HTML_BYTES) {
    throw new Error(PDF_DOCUMENT_TOO_LARGE_MESSAGE);
  }
  return invokeTauri<boolean>("export_document_pdf", {
    bodyHtml,
    title: pdfDocumentTitle(filename),
    filename,
  });
}
