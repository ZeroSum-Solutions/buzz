import * as React from "react";

/**
 * Focus the composer editor on mount and whenever the active draft key
 * changes (channel switch, thread open).
 *
 * Matches the behaviour of Slack/Discord/Signal: the composer is ready to
 * accept typing without an explicit click. The `focus` callback is expected
 * to no-op until the underlying editor is mounted, and to change identity
 * once that happens — so listing it as a dep recovers from the
 * editor-not-ready-yet case on first render. It must focus synchronously;
 * this hook owns the frame scheduling and rechecks ownership at execution.
 * Return true only when focus succeeds, so initial navigation intent survives
 * callbacks made before the editor is ready.
 *
 * The effect trigger deliberately excludes `disabled`: callers pass a
 * disabled flag that includes transient state like `isSending`, which would
 * otherwise re-fire autofocus after every send. When the main channel and
 * an open thread panel both have composers mounted, that race let the main
 * composer steal focus from the thread composer post-send. We only autofocus
 * on mount and on real navigation events (draft-key change).
 *
 * Guards:
 *  - Skip if the composer is currently disabled (archived channel, no
 *    channel, or in-flight send at the moment of mount).
 *  - Preserve text-entry and open-overlay focus. Editor readiness must also
 *    preserve a selected interactive control; a real draft-key navigation
 *    may transfer focus from its navigation button to the composer.
 */
export function useComposerAutofocus(
  focus: () => boolean,
  draftKey: string | null | undefined,
  disabled: boolean,
) {
  // We read `disabled` at execution time but intentionally don't depend on
  // it — see the comment above.
  const disabledRef = React.useRef(disabled);
  disabledRef.current = disabled;

  const previousDraftKey = React.useRef(draftKey);
  const mounted = React.useRef(false);
  const navigationIntent = React.useRef<Element | null>(null);

  React.useEffect(() => {
    if (typeof document === "undefined") return;
    if (!mounted.current || previousDraftKey.current !== draftKey) {
      // Preserve the original navigation control while the editor mounts.
      // Readiness callbacks must not reinterpret a later selection as intent.
      navigationIntent.current = document.activeElement;
    }
    mounted.current = true;
    previousDraftKey.current = draftKey;
    if (disabledRef.current) return;
    const frame = requestAnimationFrame(() => {
      if (disabledRef.current) return;
      const active = document.activeElement as HTMLElement | null;
      if (active !== navigationIntent.current) navigationIntent.current = null;
      if (active && active !== document.body) {
        const tag = active.tagName;
        const navigationHandoff = active === navigationIntent.current;
        if (
          tag === "INPUT" ||
          tag === "TEXTAREA" ||
          tag === "SELECT" ||
          active.isContentEditable ||
          active.closest(
            '[data-slot="popover-content"], [role="dialog"], [role="alertdialog"], [role="menu"], [role="listbox"]',
          ) ||
          (!navigationHandoff &&
            active.matches(
              'button, a[href], [tabindex], [role="button"], [role="switch"]',
            ))
        )
          return;
      }
      // The callback must focus synchronously: a second deferred focus would
      // escape this ownership check and could dismiss a newly opened overlay.
      if (focus()) navigationIntent.current = null;
    });
    return () => cancelAnimationFrame(frame);
  }, [draftKey, focus]);
}
