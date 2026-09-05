import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { getChannelReconnectRepairEvents } from "@/shared/api/channelReconnectRepair";
import { relayClient } from "@/shared/api/relayClient";
import type { Channel, RelayEvent } from "@/shared/api/types";
import {
  type ChannelFilesIndexController,
  type FilesIndexPageRequest,
  type FilesIndexSnapshot,
  createChannelFilesIndexController,
} from "./channelFilesBackfill";
import { emptyFilesIndex, selectIndexedFiles } from "./channelFilesIndex";
import type { ChannelFile } from "./useChannelFiles";

/**
 * Fetch one history page for the index.
 *
 * The reconnect-repair command is the one channel query that is keyset-paged
 * and fixed to the full channel kind set — replies included, deletions and
 * edits included, no `top_level` filter — which is exactly what the index
 * needs and what the timeline window (`get_channel_window`) is not.
 */
function fetchHistoryPage({
  channelId,
  limit,
  until,
  beforeId,
}: FilesIndexPageRequest): Promise<RelayEvent[]> {
  return getChannelReconnectRepairEvents({
    channelId,
    since: 0,
    limit,
    until,
    beforeId,
  });
}

const EMPTY_SNAPSHOT: FilesIndexSnapshot = {
  index: emptyFilesIndex(),
  isBackfilling: false,
  complete: false,
  hasMore: false,
  pagesFetched: 0,
  error: null,
};

export type ChannelFilesIndexResult = {
  files: ChannelFile[];
  /** True when a cap stopped the index short of the whole channel. */
  truncated: boolean;
  isLoading: boolean;
  isError: boolean;
  error: string | null;
  /** Continue or retry the history walk. */
  refetch: () => void;
  /** More history can still be loaded and no page is in flight. */
  canLoadOlder: boolean;
};

/**
 * Every attachment in a channel, newest first, from the channel's own index
 * rather than the loaded message window.
 *
 * `enabled` gates the whole machine: a channel whose Files tab has never been
 * opened opens no subscription and fetches no history.
 */
export function useChannelFilesIndex(
  activeChannel: Channel | null,
  enabled = true,
): ChannelFilesIndexResult {
  const channelId = activeChannel?.id ?? null;
  const [snapshot, setSnapshot] = useState<FilesIndexSnapshot>(EMPTY_SNAPSHOT);
  const controllerRef = useRef<ChannelFilesIndexController | null>(null);

  useEffect(() => {
    if (!channelId || !enabled) {
      controllerRef.current = null;
      setSnapshot(EMPTY_SNAPSHOT);
      return;
    }

    let cancelled = false;
    const controller = createChannelFilesIndexController({
      channelId,
      subscribeLive: (id, onEvent) =>
        relayClient.subscribeToChannelLive(id, onEvent),
      fetchPage: fetchHistoryPage,
      onChange: (next) => {
        if (!cancelled) setSnapshot(next);
      },
    });
    controllerRef.current = controller;
    setSnapshot(controller.snapshot());
    void controller.start();

    return () => {
      cancelled = true;
      controllerRef.current = null;
      void controller.dispose().catch((error: unknown) => {
        console.error(
          "Failed to close the channel file index",
          channelId,
          error,
        );
      });
    };
  }, [channelId, enabled]);

  const projection = useMemo(
    () => selectIndexedFiles(snapshot.index),
    [snapshot.index],
  );

  const refetch = useCallback(() => {
    void controllerRef.current?.loadMore();
  }, []);

  return {
    files: projection.files,
    truncated: projection.truncated,
    isLoading:
      snapshot.isBackfilling &&
      projection.files.length === 0 &&
      !snapshot.error,
    // A failure with nothing to show is the empty error state; a failure with
    // rows is a banner over the rows, never a blanked list.
    isError: snapshot.error !== null && projection.files.length === 0,
    error: snapshot.error,
    refetch,
    canLoadOlder: snapshot.hasMore && !snapshot.isBackfilling,
  };
}
