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
 * Identity of the card that invoked the current open, captured while it was
 * still mounted. The same attachment can appear in several messages — several
 * cards, one URL — so the opener is remembered as its DOM-order index among
 * the cards sharing that URL. The index survives the narrow-layout swap (the
 * message list remounts in the same order) where an element reference or a
 * URL-only selector cannot single out the invoking card.
 */
type OpenerRecord = { index: number; url: string };

let lastOpenerRecord: OpenerRecord | null = null;

function findOpenerCards(url: string): HTMLElement[] {
  return Array.from(
    document.querySelectorAll<HTMLElement>(
      `[data-testid="file-card"][data-doc-url="${CSS.escape(url)}"]`,
    ),
  );
}

/**
 * Remember which card invoked the open for `url`. Call at open time, while
 * the clicked element is still in the document; a null/detached opener (e.g.
 * a deep-link restore with no invoking card) clears the record and the
 * eventual restore falls back to the first matching card.
 */
export function recordMarkdownDocOpener(
  url: string,
  opener: HTMLElement | null,
): void {
  const index = opener ? findOpenerCards(url).indexOf(opener) : -1;
  lastOpenerRecord = index >= 0 ? { index, url } : null;
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
 * After the panel closes, return focus to the attachment card that opened
 * `url` once the channel section has remounted — the recorded invoking card
 * when one exists, the first URL match otherwise. Aborts if focus has already
 * landed on a real control (e.g. a different panel claimed it), and gives up
 * quietly when no card exists anymore.
 */
export function restoreFocusToMarkdownDocOpener(url: string): void {
  scheduleFocusSearch(
    () => {
      const cards = findOpenerCards(url);
      if (cards.length === 0) return null;
      const record = lastOpenerRecord?.url === url ? lastOpenerRecord : null;
      // One record per invocation: consume it so a later open without a
      // recorded card (deep link, reload) does not inherit a stale index.
      if (record) lastOpenerRecord = null;
      return cards[Math.min(record?.index ?? 0, cards.length - 1)] ?? null;
    },
    () => !focusIsFree(),
  );
}
