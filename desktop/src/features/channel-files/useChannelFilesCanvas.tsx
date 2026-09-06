import * as React from "react";

import {
  useCanvasQuery,
  useChannelMembersQuery,
} from "@/features/channels/hooks";
import { ChannelCanvas } from "@/features/channels/ui/ChannelCanvas";
import { useChannelModerationCapabilities } from "@/features/channels/ui/ChannelManagementModerationActions";
import { getMarkdownPreviewText } from "@/features/channels/ui/ChannelManagementSheetRows";
import type { Channel } from "@/shared/api/types";

import type { ChannelFilesCanvas } from "./ChannelFilesTab";

/**
 * Characters of the canvas body read to build the pinned row's preview.
 *
 * The body is relay-sourced and `getMarkdownPreviewText` walks every line of
 * whatever it is handed, so the bound sits on the input to that walk — the
 * work, not the rendered result, which the tab caps separately.
 */
export const MAX_CANVAS_PREVIEW_SOURCE_LENGTH = 2_000;

/** Shown on the row when the canvas exists but could not be read. */
export const CANVAS_UNAVAILABLE_PREVIEW = "This canvas could not be loaded.";

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
  const content = canvasQuery.data?.content?.trim() ?? "";
  const failed = canvasQuery.error != null;

  return React.useMemo(() => {
    if (!enabled || channelId === null) return null;
    if (!failed && content === "") return null;
    return {
      preview: failed
        ? CANVAS_UNAVAILABLE_PREVIEW
        : getMarkdownPreviewText(
            content.slice(0, MAX_CANVAS_PREVIEW_SOURCE_LENGTH),
          ),
      surface: (
        <ChannelCanvas
          canEdit={canEdit}
          channelId={channelId}
          isArchived={isArchived}
        />
      ),
    };
  }, [canEdit, channelId, content, enabled, failed, isArchived]);
}
