import { invokeTauri } from "@/shared/api/tauri";
import { renderMarkdownDocumentHtml } from "@/shared/ui/markdown/documentMode";

/**
 * Mirror of `MAX_DOCUMENT_HTML_BYTES` in
 * `src-tauri/src/commands/pdf_export.rs`. The Rust cap is the authoritative
 * one — it is what protects the browser process — but refusing here too turns
 * an oversized document into a message the reader understands instead of a
 * backend error string.
 */
export const MAX_PDF_DOCUMENT_HTML_BYTES = 8 * 1024 * 1024;

/**
 * Cap on the markdown source, checked before it is rendered.
 *
 * This is the bound that matters on this side: the render is the expensive
 * step, so refusing an oversized document before it runs is what keeps the
 * cost bounded rather than measuring the cost after paying it. It matches the
 * viewer's own native 2 MiB fetch cap, so a document that can be read in the
 * panel can always be exported.
 */
export const MAX_PDF_DOCUMENT_SOURCE_BYTES = 2 * 1024 * 1024;

/** Mirror of `MAX_TITLE_CHARS` in `pdf_export.rs`. */
export const MAX_PDF_DOCUMENT_TITLE_CHARS = 200;

export const PDF_DOCUMENT_TOO_LARGE_MESSAGE =
  "This document is too large to export as a PDF.";

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
 * the save dialog. Every other failure — an oversized document, a missing
 * browser, a failed write — rejects with a message the caller surfaces.
 */
export async function exportMarkdownDocumentToPdf({
  content,
  filename,
}: ExportMarkdownDocumentToPdfArgs): Promise<boolean> {
  if (utf8ByteLength(content) > MAX_PDF_DOCUMENT_SOURCE_BYTES) {
    throw new Error(PDF_DOCUMENT_TOO_LARGE_MESSAGE);
  }
  const bodyHtml = await renderMarkdownDocumentHtml(content);
  if (utf8ByteLength(bodyHtml) > MAX_PDF_DOCUMENT_HTML_BYTES) {
    throw new Error(PDF_DOCUMENT_TOO_LARGE_MESSAGE);
  }
  return invokeTauri<boolean>("export_document_pdf", {
    bodyHtml,
    title: pdfDocumentTitle(filename),
    filename,
  });
}
