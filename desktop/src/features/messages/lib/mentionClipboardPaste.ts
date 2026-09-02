import type { EditorView } from "@tiptap/pm/view";

import {
  getBuzzCopyKind,
  selectBindableMentionIdentities,
  selectVisibleMentionIdentities,
  type VerifyMentionIdentities,
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

/** Everything the composer currently holds, as the mention matchers read it. */
function readComposerText(view: EditorView): string {
  const { doc } = view.state;
  return doc.textBetween(0, doc.content.size, "\n", "\n");
}

/**
 * Bind the identities this paste is entitled to, once they check out.
 *
 * Verification can need a relay round trip, so it lands after the insertion.
 * That is safe in both directions: a binding only matters at send time, and
 * nothing is bound until it has been vouched for.
 *
 * The visibility gate runs a second time against what the composer holds when
 * the answer arrives — an in-flight verification whose paste the user has
 * since deleted or replaced must not bind a name nothing on screen shows.
 */
function bindVerifiedIdentities({
  html,
  insertedText,
  registerMentionPubkey,
  verifyMentionIdentities,
  view,
}: {
  html: string;
  insertedText: string;
  registerMentionPubkey?: RegisterMentionPubkey;
  verifyMentionIdentities?: VerifyMentionIdentities;
  view: EditorView;
}): void {
  // No verifier, no bindings: an unchecked pair is exactly what must not
  // become a `p` tag, so a composer that cannot check pastes readable text.
  if (!registerMentionPubkey || !verifyMentionIdentities) return;
  void selectBindableMentionIdentities({
    html,
    text: insertedText,
    verifyMentionIdentities,
  })
    .then((identities) => {
      for (const record of selectVisibleMentionIdentities(
        identities,
        readComposerText(view),
      )) {
        registerMentionPubkey(record.label, record.pubkey, {
          isAgent: record.isAgent,
        });
      }
    })
    .catch((error) => {
      // Nothing is orphaned by giving up here — the pasted words are already
      // in the composer and simply stay plain. Retrying an identity lookup the
      // user never asked for would be the surprising behaviour.
      console.warn("Could not verify pasted mention identities", error);
    });
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
 * Identity rides along: binding the records is what makes a pasted multi-word
 * name known to the composer, so its chip re-lights and the send path recovers
 * the original pubkey. Each branch is judged against the content *it* inserts
 * — the plain flavor is not evidence for what the HTML branch shows, and vice
 * versa — and then against trusted Buzz state, so neither a record the user
 * never sees nor a pair this community cannot confirm binds anything.
 */
export function handleMentionClipboardPaste({
  clipboardData,
  preventDefault,
  registerMentionPubkey,
  verifyMentionIdentities,
  view,
}: {
  clipboardData: DataTransfer;
  preventDefault: () => void;
  registerMentionPubkey?: RegisterMentionPubkey;
  verifyMentionIdentities?: VerifyMentionIdentities;
  view: EditorView;
}): boolean {
  const html = clipboardData.getData("text/html");
  if (!html) return false;

  const bind = (insertedText: string) =>
    bindVerifiedIdentities({
      html,
      insertedText,
      registerMentionPubkey,
      verifyMentionIdentities,
      view,
    });

  const text = clipboardData.getData("text/plain");
  if (getBuzzCopyKind(html) === "markdown" && text) {
    preventDefault();
    pastePlainText(view, text);
    bind(text);
    return true;
  }

  const content = normalizeMentionClipboardContent(html);
  preventDefault();
  view.pasteHTML(content.html);
  bind(content.text);
  return true;
}
