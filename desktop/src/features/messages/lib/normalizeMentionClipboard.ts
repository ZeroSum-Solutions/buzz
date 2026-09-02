/**
 * Detect whether clipboard HTML contains Buzz mention / channel-link
 * elements (marked with `data-mention` or `data-channel-link` attributes).
 */
export function hasMentionClipboardHtml(html: string): boolean {
  return html.includes("data-mention") || html.includes("data-channel-link");
}

/**
 * Put back the `@` / `#` a rendered chip strips for display.
 *
 * Exported for unit coverage: the surrounding normalization needs a DOM, this
 * decision doesn't.
 */
export function restoreChipSigil(text: string, sigil: "@" | "#"): string {
  if (!text || text.startsWith(sigil)) return text;
  return `${sigil}${text}`;
}

/**
 * Tags whose boundaries a reader sees as a line break.
 *
 * `innerText` derives this from layout, but a `DOMParser` document is never
 * rendered, so it falls back to `textContent` and runs "…the bug" straight into
 * "@John Smith". The visibility check that reads this text requires a boundary
 * before the sigil, so without the breaks a mention opening a paragraph would
 * look invisible and lose the identity it was copied with.
 */
const BLOCK_LEVEL_TAGS = new Set([
  "ADDRESS",
  "ARTICLE",
  "ASIDE",
  "BLOCKQUOTE",
  "BR",
  "DD",
  "DIV",
  "DL",
  "DT",
  "FIGCAPTION",
  "FIGURE",
  "FOOTER",
  "H1",
  "H2",
  "H3",
  "H4",
  "H5",
  "H6",
  "HEADER",
  "HR",
  "LI",
  "MAIN",
  "NAV",
  "OL",
  "P",
  "PRE",
  "SECTION",
  "TABLE",
  "TD",
  "TH",
  "TR",
  "UL",
]);

/** The text `node`'s subtree contributes, with block boundaries as newlines. */
function readRenderedText(node: Node): string {
  let text = "";
  for (const child of Array.from(node.childNodes)) {
    if (child.nodeType === Node.TEXT_NODE) {
      text += child.nodeValue ?? "";
      continue;
    }
    if (!(child instanceof Element)) continue;
    const inner = readRenderedText(child);
    text += BLOCK_LEVEL_TAGS.has(child.tagName) ? `\n${inner}\n` : inner;
  }
  return text;
}

/** Clipboard HTML ready to insert, paired with the text it will contribute. */
export type MentionClipboardContent = {
  html: string;
  /**
   * What the reader will see. Both come from the same parse, so a caller
   * deciding what the paste made visible cannot be reading different markup
   * from the one being inserted.
   */
  text: string;
};

/**
 * Normalize clipboard HTML that contains Buzz mention / channel-link
 * elements.  Replaces the styled `<span data-mention>` and
 * `<button data-channel-link>` wrappers with unstyled text nodes so
 * TipTap's Bold extension doesn't misinterpret their font-weight as bold.
 *
 * Returns cleaned HTML that preserves surrounding formatting (bold, italic,
 * line breaks, etc.) while stripping only the mention/channel-link styling,
 * alongside that HTML's rendered text.
 */
export function normalizeMentionClipboardContent(
  html: string,
): MentionClipboardContent {
  const doc = new DOMParser().parseFromString(html, "text/html");

  for (const el of Array.from(
    doc.querySelectorAll("[data-mention], [data-channel-link]"),
  )) {
    // Replace the styled wrapper with a plain <span> containing the text.
    // This preserves the text content inline while stripping the
    // font-weight/color styles that would confuse Tiptap's mark detection.
    const span = doc.createElement("span");
    // The rendered chip strips its sigil for display, so flattening it
    // verbatim would paste dead text that no composer can re-light. Restore
    // the sigil unless the source already carries it (Buzz's own copy
    // handlers write it back before the HTML reaches the clipboard).
    span.textContent = restoreChipSigil(
      el.textContent ?? "",
      el.hasAttribute("data-mention") ? "@" : "#",
    );
    el.replaceWith(span);
  }

  // Also strip any inline font-weight styles on remaining elements that
  // could be misinterpreted as bold by Tiptap (font-weight >= 500).
  for (const el of Array.from(doc.querySelectorAll("[style]"))) {
    if (el instanceof HTMLElement) {
      const fw = el.style.fontWeight;
      // Remove font-weight if it's the mention-highlight value (600)
      // but not an intentional bold (700/bold).
      if (fw === "600") {
        el.style.removeProperty("font-weight");
        if (!el.getAttribute("style")?.trim()) {
          el.removeAttribute("style");
        }
      }
    }
  }

  return { html: doc.body.innerHTML, text: readRenderedText(doc.body) };
}
