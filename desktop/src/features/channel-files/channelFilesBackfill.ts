/**
 * Drives one channel's attachment index: live subscription first, then a
 * keyset backfill over the channel's history.
 *
 * Live before history is the ordering that makes the index lossless. A finite
 * history query answered before the subscription is open leaves a gap in which
 * an arriving attachment belongs to neither, which is the defect
 * `AGENTS.md`'s review-proven rule 2 records from PR #3995. Subscribing first
 * makes the two overlap instead; the overlap is safe because
 * {@link ingestIndexEvents} keys everything by event id.
 *
 * Every page is ingested before its cursor is advanced, so an interruption
 * leaves a consistent prefix: the resumed run refetches at worst the page that
 * failed, and re-ingesting it changes nothing.
 */

import type { RelayEvent } from "@/shared/api/types";
import {
  type FilesIndex,
  emptyFilesIndex,
  ingestIndexEvents,
} from "./channelFilesIndex";

/** Events asked for per history page. */
export const BACKFILL_PAGE_LIMIT = 100;
/**
 * Pages one backfill run may fetch. The bound is on pages — the thing that
 * costs a relay round trip and an index insert — so 50 pages is at most 5 000
 * events, which is the index's own retention cap. A run that stops here leaves
 * `hasMore` true and the user can continue it.
 */
export const MAX_BACKFILL_PAGES = 50;
/** Relay-supplied failure text kept in the message shown to the user. */
export const MAX_ERROR_DETAIL_LENGTH = 200;

/** One keyset page request. `until`/`beforeId` are the previous page's tail. */
export type FilesIndexPageRequest = {
  channelId: string;
  limit: number;
  until?: number;
  beforeId?: string;
};

/** What the hook renders. */
export type FilesIndexSnapshot = {
  index: FilesIndex;
  /** A history page is in flight. */
  isBackfilling: boolean;
  /** The channel's history has been walked to its end. */
  complete: boolean;
  /** More history can still be asked for (`loadMore`). */
  hasMore: boolean;
  /** History pages fetched since this controller was created. */
  pagesFetched: number;
  /** Non-null when the user must be told the list is not the whole story. */
  error: string | null;
};

export type ChannelFilesIndexController = {
  /** Subscribe, then backfill. Safe to call once; later calls are no-ops. */
  start: () => Promise<void>;
  /** Continue (or retry) the history walk after a stop. */
  loadMore: () => Promise<void>;
  snapshot: () => FilesIndexSnapshot;
  /** Drop the subscription and ignore any page still in flight. */
  dispose: () => Promise<void>;
};

export type ChannelFilesIndexOptions = {
  channelId: string;
  subscribeLive: (
    channelId: string,
    onEvent: (event: RelayEvent) => void,
  ) => Promise<() => void | Promise<void>>;
  fetchPage: (request: FilesIndexPageRequest) => Promise<RelayEvent[]>;
  onChange?: (snapshot: FilesIndexSnapshot) => void;
};

type Cursor = { until: number; beforeId: string };

function detail(cause: unknown): string {
  const message = cause instanceof Error ? cause.message : String(cause);
  return message.slice(0, MAX_ERROR_DETAIL_LENGTH);
}

/**
 * The oldest entry of a page, as a keyset cursor: the smallest `created_at`
 * and, among ties, the largest event id — the order the relay pages in
 * (`commands/channel_reconnect_repair.rs`).
 */
export function oldestPageCursor(events: readonly RelayEvent[]): Cursor | null {
  let cursor: Cursor | null = null;
  for (const event of events) {
    if (typeof event?.id !== "string") continue;
    if (!Number.isSafeInteger(event.created_at)) continue;
    if (
      cursor === null ||
      event.created_at < cursor.until ||
      (event.created_at === cursor.until && event.id > cursor.beforeId)
    ) {
      cursor = { until: event.created_at, beforeId: event.id };
    }
  }
  return cursor;
}

/** Create the controller for one channel's attachment index. */
export function createChannelFilesIndexController({
  channelId,
  subscribeLive,
  fetchPage,
  onChange,
}: ChannelFilesIndexOptions): ChannelFilesIndexController {
  let index = emptyFilesIndex();
  let cursor: Cursor | null = null;
  let pagesFetched = 0;
  let complete = false;
  let isBackfilling = false;
  let liveError: string | null = null;
  let historyError: string | null = null;
  let disposed = false;
  let started = false;
  let unsubscribe: (() => void | Promise<void>) | null = null;
  let subscribing: Promise<void> | null = null;

  function snapshot(): FilesIndexSnapshot {
    return {
      index,
      isBackfilling,
      complete,
      hasMore: !complete && !disposed,
      pagesFetched,
      error: historyError ?? liveError,
    };
  }

  function emit(): void {
    onChange?.(snapshot());
  }

  function ingest(events: readonly RelayEvent[]): boolean {
    const next = ingestIndexEvents(index, events);
    if (next === index) return false;
    index = next;
    return true;
  }

  async function subscribe(): Promise<void> {
    subscribing = (async () => {
      try {
        const dispose = await subscribeLive(channelId, (event) => {
          if (disposed) return;
          if (ingest([event])) emit();
        });
        if (disposed) {
          await dispose();
          return;
        }
        unsubscribe = dispose;
      } catch (cause) {
        // Not fatal — history still loads — but the tab must say so, because
        // without it the list silently stops following the channel.
        liveError = `Files are not receiving live updates: ${detail(cause)}`;
        emit();
      }
    })();
    await subscribing;
  }

  async function backfill(): Promise<void> {
    if (isBackfilling || complete || disposed) return;
    isBackfilling = true;
    historyError = null;
    emit();
    try {
      for (let page = 0; page < MAX_BACKFILL_PAGES; page += 1) {
        if (disposed || complete) return;
        const request: FilesIndexPageRequest = {
          channelId,
          limit: BACKFILL_PAGE_LIMIT,
          ...(cursor ? { until: cursor.until, beforeId: cursor.beforeId } : {}),
        };

        let events: RelayEvent[];
        try {
          events = await fetchPage(request);
        } catch (cause) {
          historyError = `Could not load older files (page ${
            pagesFetched + 1
          }): ${detail(cause)}`;
          return;
        }
        // A page that lands after dispose belongs to a closed channel view.
        if (disposed) return;

        // Ingest first, advance the cursor second: a failure between the two
        // costs a refetch of one page, never a skipped one.
        const changed = ingest(events);
        pagesFetched += 1;
        const next = oldestPageCursor(events);
        if (events.length < BACKFILL_PAGE_LIMIT || next === null) {
          complete = true;
          return;
        }
        if (
          cursor !== null &&
          next.until === cursor.until &&
          next.beforeId === cursor.beforeId
        ) {
          historyError =
            "Could not load older files: the relay did not advance the history cursor.";
          return;
        }
        cursor = next;
        if (changed) emit();
      }
    } finally {
      isBackfilling = false;
      if (!disposed) emit();
    }
  }

  return {
    async start() {
      if (started || disposed) return;
      started = true;
      await subscribe();
      if (disposed) return;
      await backfill();
    },
    async loadMore() {
      if (disposed || complete) return;
      await backfill();
    },
    snapshot,
    async dispose() {
      if (disposed) return;
      disposed = true;
      if (subscribing) await subscribing.catch(() => undefined);
      const handle = unsubscribe;
      unsubscribe = null;
      if (handle) await handle();
    },
  };
}
