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
 *
 * Live and history failures are tracked apart, because they recover apart. A
 * subscription that never opened leaves the tab loading a history it will then
 * stop following, so `retryLive` re-opens it and `retry` — what the banner's
 * Retry runs — drives both halves; the live banner is cleared only by a
 * subscription that actually opened.
 *
 * "Actually opened" is the whole difficulty. The shared relay client reports a
 * refused REQ through `onReady("closed")` and returns its unsubscribe closure
 * anyway, and it reports a later terminal CLOSED — the point at which it
 * deletes the subscription and stops delivering events — only through
 * `onTerminalClose`. A caller that just awaits the closure records both as an
 * open subscription. Both are handled here, and both leave the tab with a
 * banner and a working Retry.
 */

import type { LiveSubscriptionReadiness } from "@/shared/api/relayClientShared";
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
/**
 * Attempts one controller makes to open its live subscription.
 *
 * Every attempt after the first is a user gesture — {@link
 * ChannelFilesIndexController.retryLive} is only ever reached from the
 * banner's Retry button and schedules nothing itself — so there is no
 * self-amplifying refresh loop here to put a delay in front of. The cap is the
 * terminal state `AGENTS.md`'s review-proven rule 4 asks for: a relay that
 * refuses for ever stops being asked, and the message says the tab has to be
 * reopened, which builds a fresh controller.
 */
export const MAX_LIVE_SUBSCRIBE_ATTEMPTS = 5;

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
  /** True while a live subscription is open for this channel. */
  liveConnected: boolean;
  /** True once the live subscription has spent its last attempt. */
  liveTerminal: boolean;
  /** Non-null when the user must be told the list is not the whole story. */
  error: string | null;
};

export type ChannelFilesIndexController = {
  /** Subscribe, then backfill. Safe to call once; later calls are no-ops. */
  start: () => Promise<void>;
  /** Continue (or retry) the history walk after a stop. */
  loadMore: () => Promise<void>;
  /**
   * Open the live subscription again after it failed. Idempotent: a call made
   * while a subscription is open, while one is being opened, or after the
   * attempt budget is spent does not reach the relay.
   */
  retryLive: () => Promise<void>;
  /**
   * What the error banner's Retry does: re-open the live subscription if it is
   * down, then continue the history walk. Either half alone leaves the other
   * failure standing, which is what made the banner's Retry inert.
   */
  retry: () => Promise<void>;
  snapshot: () => FilesIndexSnapshot;
  /** Drop the subscription and ignore any page still in flight. */
  dispose: () => Promise<void>;
};

/**
 * What the relay tells the controller about one live subscription, beyond the
 * events themselves.
 *
 * Both halves are needed. `onReady` covers the REQ the relay refuses outright —
 * it answers CLOSED, the shared client resolves readiness with `"closed"` and
 * returns an unsubscribe closure exactly as it does for a healthy
 * subscription, so a caller that only awaits the closure records a refusal as
 * a live connection. `onTerminalClose` covers the CLOSED that arrives after
 * readiness, which deletes the subscription inside the shared client and would
 * otherwise be invisible to its owner for the rest of the session.
 */
export type LiveSubscriptionHandlers = {
  /** How the REQ settled: `"closed"` means the relay refused it. */
  onReady: (readiness: LiveSubscriptionReadiness) => void;
  /** The relay ended an open subscription for good, with its message. */
  onTerminalClose: (message: string) => void;
};

export type ChannelFilesIndexOptions = {
  channelId: string;
  subscribeLive: (
    channelId: string,
    onEvent: (event: RelayEvent) => void,
    handlers: LiveSubscriptionHandlers,
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
  let liveConnected = false;
  let liveAttempts = 0;
  let unsubscribe: (() => void | Promise<void>) | null = null;
  let subscribing: Promise<void> | null = null;

  /** No subscription, and no attempts left to open one. */
  function liveTerminal(): boolean {
    return !liveConnected && liveAttempts >= MAX_LIVE_SUBSCRIBE_ATTEMPTS;
  }

  function snapshot(): FilesIndexSnapshot {
    return {
      index,
      isBackfilling,
      complete,
      hasMore: !complete && !disposed,
      pagesFetched,
      liveConnected,
      liveTerminal: liveTerminal(),
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

  /**
   * Run `work` on behalf of a relay callback, and let nothing escape.
   *
   * These callbacks are invoked from the relay client's shared event
   * dispatcher, which walks one buffer for every subscription on the socket in
   * a bare loop (`relayClosedRecovery.ts`, `flushEvents`) from a `setTimeout`.
   * A throw there is an uncaught exception that silently discards the rest of
   * that batch — the timeline, unread counts, typing indicators and huddles,
   * not only Files. Two things in `work` can throw: an event shape the index
   * refuses, and `onChange`, which is React's state setter and so carries a
   * render. The failure becomes the tab's live-error banner; if publishing even
   * that fails, it goes to the console, because there is no sink left.
   */
  function runInRelayCallback(work: () => void): void {
    try {
      work();
    } catch (cause) {
      liveError = `Files stopped following live updates: ${detail(cause)}`;
      try {
        emit();
      } catch (emitCause) {
        console.error(
          "Failed to publish the channel file index",
          channelId,
          emitCause,
        );
      }
    }
  }

  /** The banner text for a live subscription that is down, with its reason. */
  function liveDownMessage(reason: unknown): string {
    const given = reason === null || reason === undefined ? "" : detail(reason);
    const text = given || "the relay closed the subscription";
    return liveTerminal()
      ? `Files are not receiving live updates: ${text}. Reopen the Files tab to try again.`
      : `Files are not receiving live updates: ${text}`;
  }

  /**
   * Record that no live subscription is open, with the reason.
   *
   * Reached from both halves of a relay refusal: a CLOSED answered inside the
   * readiness window, and a terminal CLOSED that ends an already-open
   * subscription. In the second case the shared client has already deleted the
   * subscription, so the handle we hold is dead — dropping it is what keeps
   * `dispose` from closing a subscription id the relay reassigned.
   */
  function noteLiveDown(reason: unknown): void {
    runInRelayCallback(() => {
      unsubscribe = null;
      liveConnected = false;
      liveError = liveDownMessage(reason);
      emit();
    });
  }

  /**
   * Fold one live event into the index.
   *
   * The relay's `EVENT` payload is `JSON.parse`d and cast, never validated
   * (`relayClientSession.ts`, `handleWsMessage`), so this is an untrusted DTO:
   * {@link ingestIndexEvents} type-checks and caps every field it reads, and
   * {@link runInRelayCallback} catches whatever still gets through rather than
   * throwing into the dispatcher shared with every other subscription.
   */
  function onLiveEvent(event: RelayEvent): void {
    if (disposed) return;
    runInRelayCallback(() => {
      if (ingest([event])) emit();
    });
  }

  /**
   * Open the live subscription, once. Idempotent on every state that makes a
   * second relay call wrong: one already open, one already being opened, a
   * disposed controller, or a spent attempt budget.
   */
  async function subscribe(): Promise<void> {
    if (subscribing) {
      await subscribing;
      return;
    }
    if (disposed || liveConnected || liveTerminal()) return;
    liveAttempts += 1;
    const attempt = (async () => {
      // A record rather than two `let`s: these are written from callbacks, and
      // property reads re-widen at the `await` below where a narrowed local
      // would not.
      const outcome: {
        readiness: LiveSubscriptionReadiness | null;
        closedReason: string | null;
      } = { readiness: null, closedReason: null };
      try {
        const dispose = await subscribeLive(channelId, onLiveEvent, {
          onReady: (value) => {
            outcome.readiness = value;
          },
          onTerminalClose: (message) => {
            outcome.closedReason = message;
            noteLiveDown(message);
          },
        });
        if (disposed) {
          await dispose();
          return;
        }
        if (outcome.readiness === "closed") {
          // The relay answered the REQ with CLOSED. The shared client resolves
          // readiness for every outcome and returns this closure anyway, so
          // without this branch a refusal — `error: too many subscriptions`,
          // `restricted:` — would be recorded as an open subscription and the
          // tab would follow nothing for the life of the channel view.
          //
          // Closing the handle covers the retryable class too: readiness does
          // not carry the class, so rather than leave a subscription whose
          // recovery this controller cannot observe, the attempt ends here and
          // the banner's Retry opens a fresh one.
          await dispose();
          if (outcome.closedReason === null) noteLiveDown(null);
          return;
        }
        unsubscribe = dispose;
        liveConnected = true;
        // The only place the live banner is cleared: a subscription that
        // actually opened is the only thing that makes it untrue.
        liveError = null;
        emit();
      } catch (cause) {
        // Not fatal — history still loads — but the tab must say so, because
        // without it the list silently stops following the channel.
        liveError = liveDownMessage(cause);
        emit();
      }
    })();
    subscribing = attempt;
    try {
      await attempt;
    } finally {
      if (subscribing === attempt) subscribing = null;
    }
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
        // costs a refetch of one page, never a skipped one. The relay's page
        // is untrusted shape as well as untrusted content, so reading it is
        // allowed to fail — and when it does the walk stops with a message
        // rather than throwing out of `start()` as an unhandled rejection.
        let changed: boolean;
        let next: Cursor | null;
        let pageSize: number;
        try {
          if (!Array.isArray(events)) {
            throw new Error("the relay returned a page that is not a list");
          }
          pageSize = events.length;
          changed = ingest(events);
          next = oldestPageCursor(events);
        } catch (cause) {
          historyError = `Could not load older files (page ${
            pagesFetched + 1
          }): ${detail(cause)}`;
          return;
        }
        pagesFetched += 1;
        if (pageSize < BACKFILL_PAGE_LIMIT || next === null) {
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
      // `backfill` owns the disposed/complete/in-flight gates; a second copy
      // here would be a guard no test could remove independently.
      await backfill();
    },
    async retryLive() {
      await subscribe();
    },
    async retry() {
      await subscribe();
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
