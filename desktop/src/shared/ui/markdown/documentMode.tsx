import type * as React from "react";
import type { Components } from "react-markdown";

import {
  exceedsMarkdownDocParseWorkBudget,
  measureMarkdownDocParseWork,
} from "./markdownDocFile";
import {
  MAX_MARKDOWN_DOC_NODES,
  isMarkdownTooComplexError,
} from "./markdownParseBudget";
import { renderCachedMarkdown } from "./nodeCache";

/**
 * Document mode: the markdown render used for a printed document rather than
 * for the chat timeline.
 *
 * It shares the parse pipeline every other Buzz markdown surface uses
 * (`renderCachedMarkdown`, same remark plugins, same URL transform) but
 * swaps the component map for one that emits plain semantic HTML:
 *
 * - **Links are kept.** A chat render turns anchors into cards, pills and
 *   previews, and turns them into inert `<span>`s when non-interactive. A
 *   document keeps `<a href>` so the printed page still carries its
 *   references.
 * - **Attachments render as links.** Snapshot cards, file cards, audio
 *   players and image mosaics have no meaning on paper; the underlying URL,
 *   labelled, does.
 * - **Code is never collapsed.** No syntax-highlight cap, no scroll
 *   container, no max height — the print stylesheet wraps long lines and lets
 *   a block flow across pages.
 *
 * The output carries no application classes: it is styled entirely by the
 * print stylesheet the Rust exporter wraps it in
 * (`src-tauri/src/commands/pdf_export_print.css`), so it stays readable as a
 * standalone document.
 */
export const DOCUMENT_MODE_VARIANT = "doc-v1";

/** Renders a custom inline node (mention, spoiler, deep link) as its text. */
function PlainInline({ children }: { children?: React.ReactNode }) {
  return <span>{children}</span>;
}

/**
 * Component map for document mode. Standard markdown elements fall through to
 * react-markdown's own HTML output; only the elements a chat render would
 * turn into interactive widgets are overridden here.
 */
export const documentModeComponents = {
  // An image cannot be fetched during an export (the print document's
  // content-security policy denies every remote subresource), so it is
  // rendered as the labelled link it stands for instead of a blank box.
  img: ({ alt, src }: React.ComponentPropsWithoutRef<"img">) => {
    const href = typeof src === "string" ? src : "";
    const label = alt && alt.length > 0 ? alt : href;
    if (href.length === 0) return <span>{label}</span>;
    return <a href={href}>{label}</a>;
  },
  mention: PlainInline,
  "channel-link": PlainInline,
  "channel-deep-link": PlainInline,
  "message-link": PlainInline,
  "entity-link": PlainInline,
  emoji: PlainInline,
  spoiler: PlainInline,
} as Components;

/**
 * The parsed element tree for `content` in document mode. Rendering it to a
 * string is the caller's job (see `renderMarkdownDocumentHtml`), so this stays
 * usable from a React tree too.
 */
export function markdownDocumentElement(content: string): React.ReactElement {
  return renderCachedMarkdown({
    components: documentModeComponents,
    content,
    // Documents follow markdown's own paragraph rules; the chat surface's
    // newline-is-a-break behaviour would double every wrapped line.
    hardLineBreaks: false,
    nodeBudget: MAX_MARKDOWN_DOC_NODES,
    variant: DOCUMENT_MODE_VARIANT,
  });
}

/**
 * Whether `content` is too complex to render, by the same two bounds the
 * Preview and the Export share.
 *
 * The cheap source-side work model runs first and bounds micromark's own
 * tokenizers, whose cost is spent before any mdast node exists. What it
 * admits is then parsed with the node budget enforced inside
 * `processor.parse()`, so the answer comes from the parsed syntax tree and
 * not from a guess about the source text. A refusal from the first bound is
 * sub-millisecond; a refusal from the second costs the parse, which the first
 * bound is what keeps finite.
 *
 * That parse is skipped when the source-side node estimate is under half the
 * budget. Measured over every document the scan admits — the 2,462 real
 * markdown files reachable from this repository and every adversarial shape
 * in `markdownDocFile.ts` — the estimate never came in below the real node
 * count (highest ratio 1.00), so under half the budget it already proves the
 * tree fits with a factor of two to spare. Paying a whole extra parse to
 * learn the same thing would put the panel's open over its 200 ms
 * main-thread budget on documents nowhere near the cap: the 507 KB
 * long-document fixture is 117 nodes against an estimate of 411, and the
 * extra parse measured 296 ms against a 200 ms budget in
 * `markdown-doc-viewer.spec.ts`.
 */
export function isMarkdownDocumentTooComplex(content: string): boolean {
  const work = measureMarkdownDocParseWork(content);
  if (exceedsMarkdownDocParseWorkBudget(work)) return true;
  if (2 * work.estimatedNodes <= MAX_MARKDOWN_DOC_NODES) return false;
  try {
    markdownDocumentElement(content);
    return false;
  } catch (error) {
    if (isMarkdownTooComplexError(error)) return true;
    throw error;
  }
}

/**
 * Render `content` to the document-mode HTML body the PDF exporter prints.
 *
 * `react-dom/server` is imported lazily so the renderer only enters the
 * bundle on the export path.
 */
export async function renderMarkdownDocumentHtml(
  content: string,
): Promise<string> {
  const { renderToStaticMarkup } = await import("react-dom/server");
  return renderToStaticMarkup(markdownDocumentElement(content));
}
