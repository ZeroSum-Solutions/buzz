/**
 * The one-line preview the Files tab's pinned canvas row shows.
 *
 * Split out of the hook so the bound below is bound by a test: the hook that
 * used to hold it needs a React tree and three queries to reach, and a guard no
 * test can remove is a guard that protects nothing.
 */

import { getMarkdownPreviewText } from "@/features/channels/ui/ChannelManagementSheetRows";

/**
 * Characters of the canvas body read to build the preview.
 *
 * The body is relay-sourced and uncapped where it is read
 * (`desktop/src-tauri/src/commands/canvas.rs`), and `getMarkdownPreviewText`
 * runs eleven regex replaces and two trims over *every line* of whatever it is
 * handed. The bound therefore sits on the input to that walk — the work — not
 * on the rendered result, which the tab caps separately for the DOM.
 */
export const MAX_CANVAS_PREVIEW_SOURCE_LENGTH = 2_000;

/**
 * The preview for `rawContent`: at most
 * {@link MAX_CANVAS_PREVIEW_SOURCE_LENGTH} characters of it, with the markdown
 * syntax stripped.
 *
 * The slice comes before the trim, so a body padded with whitespace costs the
 * same as any other body of the same cap.
 */
export function canvasPreviewText(rawContent: string): string {
  return getMarkdownPreviewText(
    rawContent.slice(0, MAX_CANVAS_PREVIEW_SOURCE_LENGTH).trim(),
  );
}

/** Shown on the pinned row when the canvas exists but could not be read. */
export const CANVAS_UNAVAILABLE_PREVIEW = "This canvas could not be loaded.";
