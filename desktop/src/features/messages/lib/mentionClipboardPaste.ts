import type { EditorView } from "@tiptap/pm/view";

import {
  getBuzzCopyKind,
  registerMentionClipboardIdentities,
} from "./mentionClipboard";
import { normalizeMentionClipboardContent } from "./normalizeMentionClipboard";

export type RegisterMentionPubkey = (
  displayName: string,
  pubkey: string,
  options?: { isAgent?: boolean },
) => void;

/**
 * Insert `text` through ProseMirror's plain-text paste pipeline.
 *
 * `view.pasteText` re-enters `handlePaste` with the original event, so the
 * clipboard data is rebuilt with only the plain flavor — otherwise the HTML
 * branch would claim the paste again, forever.
 */
function pastePlainText(view: EditorView, text: string): void {
  const clipboardData = new DataTransfer();
  clipboardData.setData("text/plain", text);
  view.pasteText(text, new ClipboardEvent("paste", { clipboardData }));
}

/**
 * Paste clipboard HTML that carries Buzz mention markers.
 *
 * Content follows the flavor the copy declared:
 *
 * - `markdown` — the copy's plain flavor *is* the Markdown source, so insert
 *   it through the text pipeline and TipTap parses `**bold**` exactly as it
 *   does for any other plain paste.
 * - `rich` (or legacy Buzz HTML with no marker) — keep the HTML path, with
 *   chip wrappers flattened to sigil-bearing text.
 *
 * Identity rides along: registering the records is what makes a pasted
 * multi-word name known to the composer, so its chip re-lights and the send
 * path recovers the original pubkey. Each branch registers against the content
 * *it* inserts — the plain flavor is not evidence for what the HTML branch
 * shows, and vice versa — so a record the user never sees binds nothing.
 */
export function handleMentionClipboardPaste({
  clipboardData,
  preventDefault,
  registerMentionPubkey,
  view,
}: {
  clipboardData: DataTransfer;
  preventDefault: () => void;
  registerMentionPubkey?: RegisterMentionPubkey;
  view: EditorView;
}): boolean {
  const html = clipboardData.getData("text/html");
  if (!html) return false;

  const registerVisibleIdentities = (insertedText: string) => {
    if (!registerMentionPubkey) return;
    registerMentionClipboardIdentities({
      html,
      registerMentionPubkey,
      text: insertedText,
    });
  };

  const text = clipboardData.getData("text/plain");
  if (getBuzzCopyKind(html) === "markdown" && text) {
    registerVisibleIdentities(text);
    preventDefault();
    pastePlainText(view, text);
    return true;
  }

  const content = normalizeMentionClipboardContent(html);
  registerVisibleIdentities(content.text);
  preventDefault();
  view.pasteHTML(content.html);
  return true;
}
