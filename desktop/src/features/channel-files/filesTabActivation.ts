/**
 * When the channel attachment index is allowed to run.
 *
 * The index is not a projection of already-loaded data: it opens a live relay
 * subscription and walks up to `MAX_BACKFILL_PAGES` of channel history. So it
 * stays off until the user opens the Files tab for the channel that is on
 * screen, and a Chat-only session pays for none of it.
 *
 * The activation state is a channel id rather than a boolean on purpose. A
 * boolean latched by the previous channel is still true during the render that
 * first shows the next channel, and the index effect runs before the effect
 * that resets the tab — long enough to open a subscription for a channel whose
 * Files tab was never opened.
 */

/**
 * True when the Files tab has been opened for the channel currently on screen.
 *
 * @param filesTabOpenedChannelId Channel whose Files tab the user opened, or
 *   `null` when no tab has been opened since the last channel change.
 * @param activeChannelId Channel currently on screen.
 */
export function isFilesIndexEnabled(
  filesTabOpenedChannelId: string | null,
  activeChannelId: string | null,
): boolean {
  return (
    filesTabOpenedChannelId !== null &&
    filesTabOpenedChannelId === activeChannelId
  );
}
