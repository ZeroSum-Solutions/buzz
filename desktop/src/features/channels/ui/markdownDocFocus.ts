/**
 * Focus choreography for the markdown document panel (PR #6731 P2).
 *
 * In the narrow single-panel layout, opening a document unmounts the channel
 * section containing the focused attachment card, and closing unmounts the
 * panel that held focus — in both directions focus falls to `<body>` and
 * keyboard/screen-reader users lose their place. On open, focus moves to the
 * panel's close control; on close, it returns to the attachment card that
 * opened the document (by recorded identity, since the original element was
 * unmounted meanwhile and the URL alone can match several cards).
 */

const PANEL_CLOSE_SELECTOR =
  '[data-testid="markdown-doc-panel"] [data-testid="auxiliary-panel-close"]';

/**
 * Identity of the surface that invoked the current open. Message ids survive
 * timeline insertions/deletions and narrow-layout remounts; URL ordinals do
 * not. For a thread-only card, the thread head identifies the surviving
 * summary control used as the deliberate return target after the pane closes.
 */
type OpenerRecord = {
  messageId: string;
  threadHeadId: string | null;
  url: string;
};

let lastOpenerRecord: OpenerRecord | null = null;

function findCard(messageId: string, url: string): HTMLElement | null {
  const row = document.querySelector<HTMLElement>(
    `[data-testid="message-row"][data-message-id="${CSS.escape(messageId)}"]`,
  );
  return (
    row?.querySelector<HTMLElement>(
      `[data-testid="file-card"][data-doc-url="${CSS.escape(url)}"]`,
    ) ?? null
  );
}

function findFallback(url: string): HTMLElement | null {
  return document.querySelector<HTMLElement>(
    `[data-testid="file-card"][data-doc-url="${CSS.escape(url)}"]`,
  );
}

function findThreadSummary(threadHeadId: string): HTMLElement | null {
  return document.querySelector<HTMLElement>(
    `[data-testid="message-thread-summary"][data-thread-head-id="${CSS.escape(threadHeadId)}"]`,
  );
}

/**
 * Whether the URL's `doc` search param is still this exact document. Used to
 * tell an authoritative close (the document is actually going away) apart
 * from a transient unmount: ChannelPane's pane-priority chain unmounts this
 * panel whenever a higher-priority pane (profile, thread, agent session)
 * opens, without touching `doc`/`docName` — the panel remounts with the same
 * URL once that pane closes, no new open event. `doc` and this module's
 * `url` argument are the same relay URL by construction
 * (`openMarkdownDoc` in useChannelPanelHistoryState.ts sets both together).
 */
function docSearchStateStillHoldsUrl(url: string): boolean {
  return new URLSearchParams(window.location.search).get("doc") === url;
}

/**
 * Remember the logical surface that invoked the open while its card is still
 * mounted. A detached/null opener (for example URL history restoration) clears
 * the record, leaving close to use the first matching card when available.
 */
export function recordMarkdownDocOpener(
  url: string,
  opener: HTMLElement | null,
): void {
  if (!opener?.isConnected) {
    lastOpenerRecord = null;
    return;
  }
  const row = opener.closest<HTMLElement>(
    '[data-testid="message-row"][data-message-id]',
  );
  const messageId = row?.dataset.messageId;
  if (!messageId) {
    lastOpenerRecord = null;
    return;
  }
  const threadPanel = opener.closest<HTMLElement>(
    '[data-testid="message-thread-panel"]',
  );
  lastOpenerRecord = {
    messageId,
    threadHeadId:
      threadPanel?.querySelector<HTMLElement>(
        '[data-testid="message-row"][data-message-id]',
      )?.dataset.messageId ?? null,
    url,
  };
}

/** Frames to wait for the target to (re)mount before giving up. */
const FOCUS_SEARCH_FRAMES = 12;

function scheduleFocusSearch(
  find: () => HTMLElement | null,
  shouldAbort: () => boolean,
): () => void {
  let frame = 0;
  let attempts = 0;
  const tick = () => {
    if (shouldAbort()) return;
    const target = find();
    if (target) {
      target.focus();
      return;
    }
    attempts += 1;
    if (attempts < FOCUS_SEARCH_FRAMES) frame = requestAnimationFrame(tick);
  };
  frame = requestAnimationFrame(tick);
  return () => cancelAnimationFrame(frame);
}

/**
 * True when moving focus is a restoration, not a steal. `<body>`/null means
 * focus fell off an unmounted subtree. The composer counts as free too: the
 * remounting channel autofocuses it, which is exactly the "lands on the
 * composer rather than the invoking attachment" behavior being fixed —
 * anything else (another panel's control, a clicked button) keeps focus.
 */
function focusIsFree(): boolean {
  const active = document.activeElement;
  if (active === null || active === document.body) return true;
  return active.closest('[data-testid="message-composer"]') !== null;
}

/**
 * Move focus onto the open panel's close control. Returns a canceler for
 * effect cleanup so an unmounting panel stops hunting for its own button.
 */
export function focusMarkdownDocPanelClose(): () => void {
  return scheduleFocusSearch(
    () => document.querySelector<HTMLElement>(PANEL_CLOSE_SELECTOR),
    // Never abort: the open was user-initiated, the panel is the destination.
    () => false,
  );
}

/**
 * After the panel closes, return focus to the exact invoking message/card.
 * If its thread surface was closed to make room for the document, focus the
 * surviving thread-summary control. Only if the logical opener disappeared do
 * we fall back to another card for the same immutable attachment URL.
 */
export function restoreFocusToMarkdownDocOpener(url: string): void {
  // A transient unmount (see `docSearchStateStillHoldsUrl`) must not consume
  // the opener record or search for a restore target: the panel is about to
  // remount with no new open event, and the eventual real close needs the
  // record to still identify the true opener rather than falling back to
  // the first same-URL card.
  if (docSearchStateStillHoldsUrl(url)) return;

  const record = lastOpenerRecord?.url === url ? lastOpenerRecord : null;
  // Only clear the record when this call actually consumes it. A mismatched
  // url means some *other* document's unmount is racing this one (opening a
  // second document before the first panel's cleanup runs) — that call must
  // not destroy a newer, not-yet-consumed record it doesn't own.
  if (record) lastOpenerRecord = null;
  scheduleFocusSearch(
    () =>
      (record ? findCard(record.messageId, url) : null) ??
      (record?.threadHeadId ? findThreadSummary(record.threadHeadId) : null) ??
      findFallback(url),
    () => !focusIsFree(),
  );
}
