import * as React from "react";

import {
  useCanvasQuery,
  useChannelMembersQuery,
} from "@/features/channels/hooks";
import { ChannelCanvas } from "@/features/channels/ui/ChannelCanvas";
import { useChannelModerationCapabilities } from "@/features/channels/ui/ChannelManagementModerationActions";
import type { Channel } from "@/shared/api/types";

import {
  CANVAS_UNAVAILABLE_PREVIEW,
  canvasPreviewText,
  MAX_CANVAS_PREVIEW_SOURCE_LENGTH,
} from "./canvasPreview";
import type { ChannelFilesCanvas } from "./ChannelFilesTab";

export type UseChannelFilesCanvasInput = {
  /** The channel whose canvas to pin, or null when none is open. */
  channel: Channel | null;
  /** The signed-in user, for the edit capability check. */
  currentPubkey: string | undefined;
  /** False keeps the Files tab's queries off a Chat-only session. */
  enabled: boolean;
};

/**
 * The channel's canvas as the Files tab pins it, or `null` when the channel
 * has none.
 *
 * A read failure returns a row rather than `null`: a canvas that exists but
 * could not be fetched must not read as a channel without one, and the row is
 * the only way back to the surface that reports the failure and retries it.
 */
export function useChannelFilesCanvas({
  channel,
  currentPubkey,
  enabled,
}: UseChannelFilesCanvasInput): ChannelFilesCanvas | null {
  const channelId = channel?.id ?? null;
  const canvasQuery = useCanvasQuery(channelId, enabled);
  const membersQuery = useChannelMembersQuery(channelId, enabled);
  const { canManageChannel } = useChannelModerationCapabilities(
    membersQuery.data,
    currentPubkey,
    enabled,
  );

  const isArchived = channel?.archivedAt != null;
  const canEdit =
    canManageChannel &&
    channel?.channelType !== "dm" &&
    (channel?.isMember ?? false);
  // The raw body, not a trimmed copy of it: trimming here would rescan the
  // whole relay-sourced body on every render of the screen that owns this hook,
  // not only when the body changes.
  const rawContent = canvasQuery.data?.content ?? "";
  const failed = canvasQuery.error != null;
  const hasMetadata =
    canvasQuery.data?.updatedAt != null || canvasQuery.data?.author != null;
  // Bounded fallback for presence: some sources (the e2e mock bridge; a relay
  // that omits author/updatedAt) never populate the metadata fields even when
  // a canvas exists. Scan only the same bounded prefix the preview itself
  // reads — never the full, unbounded rawContent — so this stays O(1) instead
  // of rescanning a relay-sourced body on every render.
  const hasContentWithinCap =
    rawContent.slice(0, MAX_CANVAS_PREVIEW_SOURCE_LENGTH).trim().length > 0;

  return React.useMemo(() => {
    if (!enabled || channelId === null) return null;
    if (!failed && !hasMetadata && !hasContentWithinCap) return null;
    return {
      preview: failed
        ? CANVAS_UNAVAILABLE_PREVIEW
        : canvasPreviewText(rawContent),
      surface: (
        <ChannelCanvas
          canEdit={canEdit}
          channelId={channelId}
          isArchived={isArchived}
        />
      ),
    };
  }, [
    canEdit,
    channelId,
    enabled,
    failed,
    hasContentWithinCap,
    hasMetadata,
    isArchived,
    rawContent,
  ]);
}
