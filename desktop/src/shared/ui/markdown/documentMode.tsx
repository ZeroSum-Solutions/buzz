import type * as React from "react";
import type { Components } from "react-markdown";

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
    variant: DOCUMENT_MODE_VARIANT,
  });
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
