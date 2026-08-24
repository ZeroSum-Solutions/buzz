/**
 * Focus choreography for the markdown document panel (PR #6731 P2).
 *
 * In the narrow single-panel layout, opening a document unmounts the channel
 * section containing the focused attachment card, and closing unmounts the
 * panel that held focus — in both directions focus falls to `<body>` and
 * keyboard/screen-reader users lose their place. On open, focus moves to the
 * panel's close control; on close, it returns to the attachment card that
 * opened the document (matched by URL, since the original element was
 * unmounted meanwhile).
 */

const PANEL_CLOSE_SELECTOR =
  '[data-testid="markdown-doc-panel"] [data-testid="auxiliary-panel-close"]';

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
 * `url` once the channel section has remounted. Aborts if focus has already
 * landed on a real control (e.g. a different panel claimed it), and gives up
 * quietly when the card no longer exists.
 */
export function restoreFocusToMarkdownDocOpener(url: string): void {
  scheduleFocusSearch(
    () =>
      document.querySelector<HTMLElement>(
        `[data-testid="file-card"][data-doc-url="${CSS.escape(url)}"]`,
      ),
    () => !focusIsFree(),
  );
}
