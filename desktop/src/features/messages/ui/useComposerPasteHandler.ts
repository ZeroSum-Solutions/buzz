import * as React from "react";
import type { Editor } from "@tiptap/react";
import { handleAgentSnapshotPaste } from "@/features/messages/lib/agentSnapshotClipboard";
import type { BlobDescriptor } from "@/shared/api/tauri";
import { hasMentionClipboardHtml } from "@/features/messages/lib/normalizeMentionClipboard";
import {
  handleMentionClipboardPaste,
  type RegisterMentionPubkey,
} from "@/features/messages/lib/mentionClipboardPaste";
import type { VerifyMentionIdentities } from "@/features/messages/lib/mentionClipboard";
import { getBuzzCodeBlockClipboardText } from "@/shared/lib/codeBlockClipboard";

export function useComposerPasteHandler(options: {
  editor: Editor | null;
  /** Teaches the composer each `name → pubkey` pair a Buzz copy carried. */
  registerMentionPubkey?: RegisterMentionPubkey;
  /** Confirms a carried pair against trusted state; without it none bind. */
  verifyMentionIdentities?: VerifyMentionIdentities;
  scrollToBottom: () => void;
  setPendingImeta: (
    update: (current: BlobDescriptor[]) => BlobDescriptor[],
  ) => void;
  uploadFile: (file: File) => Promise<unknown>;
}) {
  const uploadFileRef = React.useRef(options.uploadFile);
  uploadFileRef.current = options.uploadFile;
  const registerMentionPubkeyRef = React.useRef(options.registerMentionPubkey);
  registerMentionPubkeyRef.current = options.registerMentionPubkey;
  const verifyMentionIdentitiesRef = React.useRef(
    options.verifyMentionIdentities,
  );
  verifyMentionIdentitiesRef.current = options.verifyMentionIdentities;
  React.useEffect(() => {
    const editor = options.editor;
    if (!editor) return;
    editor.setOptions({
      editorProps: {
        ...editor.options.editorProps,
        handlePaste: (view, event) => {
          const mediaItem = Array.from(event.clipboardData?.items ?? []).find(
            (item) => item.kind === "file",
          );
          if (mediaItem) {
            const file = mediaItem.getAsFile();
            if (file) void uploadFileRef.current(file);
            return true;
          }
          const codeBlockText = getBuzzCodeBlockClipboardText(
            event.clipboardData,
          );
          if (codeBlockText !== null) {
            event.preventDefault();
            editor
              .chain()
              .focus()
              .insertContent([
                {
                  type: "codeBlock",
                  content:
                    codeBlockText.length > 0
                      ? [{ type: "text", text: codeBlockText }]
                      : [],
                },
                { type: "paragraph" },
              ])
              .run();
            options.scrollToBottom();
            return true;
          }
          if (handleAgentSnapshotPaste(event, options.setPendingImeta))
            return true;
          const clipboardData = event.clipboardData;
          const html = clipboardData?.getData("text/html");
          if (clipboardData && html && hasMentionClipboardHtml(html)) {
            if (clipboardData.getData("text/plain").includes("\n")) {
              options.scrollToBottom();
            }
            return handleMentionClipboardPaste({
              clipboardData,
              preventDefault: () => event.preventDefault(),
              registerMentionPubkey: registerMentionPubkeyRef.current,
              verifyMentionIdentities: verifyMentionIdentitiesRef.current,
              view,
            });
          }
          if ((clipboardData?.getData("text/plain") ?? "").includes("\n"))
            options.scrollToBottom();
          return false;
        },
      },
    });
  }, [options.editor, options.scrollToBottom, options.setPendingImeta]);
}
