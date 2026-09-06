/**
 * The resolve-once state machine behind a path-link token.
 *
 * Split out of `PathLinkChip` so the whole lifecycle — resolve on demand,
 * ignore a superseded answer, open only what the click actually named — is
 * unit-testable without a webview, and so the chip is only markup.
 *
 * A message can be edited. `MarkdownInner` re-renders on the new body and the
 * token keeps its position in the tree, so React reconciles the *same*
 * component instance with a new `text` prop and never remounts it. Without a
 * reset a settled `{ status: "link" }` from the old text would survive: the
 * chip would show `docs/readme.md` while its label, title and click target
 * still carried the path the sender wrote first. So the resolution is fenced
 * by a generation counter — the candidate is reset during render, and an
 * in-flight answer from an older generation is dropped instead of committed.
 */

import * as React from "react";

import {
  type PathLinkInvoke,
  type PathLinkTarget,
  resolvePathLink,
} from "./pathLinks";

/** Where one token's single resolution attempt has got to. */
export type PathLinkResolution =
  /** Nothing asked yet. No IPC has happened for this token. */
  | { status: "idle" }
  /** A resolution is in flight; further triggers wait for its answer. */
  | { status: "pending" }
  /** The resolver answered "not a link"; the token stays plain text. */
  | { status: "text" }
  /** The resolver found a file. */
  | { status: "link"; target: PathLinkTarget };

export type UsePathLinkResolutionOptions = {
  /** The token's literal text — the path candidate to resolve. */
  text: string;
  /** The message sender, which selects the roots the candidate may live in. */
  senderPubkey: string | null;
  /** The IPC seam; `PathLinkChip` passes Tauri's `invoke`. */
  invoke: PathLinkInvoke;
  /** Called with a target the user actually clicked, once, after it settles. */
  onOpen: (target: PathLinkTarget) => void;
  /** Called with a user-facing message when the resolver refuses. */
  onError: (message: string) => void;
};

export type PathLinkResolutionControls = {
  /** The current state, for rendering. */
  state: PathLinkResolution;
  /** Resolve once, on hover or focus. Safe to call repeatedly. */
  resolve: () => void;
  /** Resolve if needed, then open the result if it is still the current one. */
  activate: () => void;
};

function errorMessage(error: unknown, fallback: string): string {
  return error instanceof Error && error.message ? error.message : fallback;
}

/**
 * Resolve one inline-code token to a local file, at most once per candidate.
 *
 * Nothing is resolved while a channel renders: the caller triggers `resolve`
 * on hover or focus and `activate` on click. `activate` opens only when the
 * settled answer belongs to the candidate that was clicked — a message edit,
 * or a change of sender, retires the answer instead of acting on it.
 */
export function usePathLinkResolution({
  text,
  senderPubkey,
  invoke,
  onOpen,
  onError,
}: UsePathLinkResolutionOptions): PathLinkResolutionControls {
  const [state, setState] = React.useState<PathLinkResolution>({
    status: "idle",
  });
  // `resolve` is the only writer of both; the ref lets a click act on the
  // resolution it just triggered without waiting for a render.
  const stateRef = React.useRef<PathLinkResolution>({ status: "idle" });
  const generationRef = React.useRef(0);
  // The resolution in flight, so a click that lands before the hover's answer
  // waits for that same answer instead of seeing "pending" and opening
  // nothing.
  const inFlightRef = React.useRef<Promise<PathLinkResolution> | null>(null);

  // Resolution outlives a hover, so the setter must not write into an
  // unmounted token.
  const mountedRef = React.useRef(true);
  React.useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  // Handlers are read through refs so a caller's inline closures do not
  // restart a resolution that is already in flight.
  const invokeRef = React.useRef(invoke);
  const onOpenRef = React.useRef(onOpen);
  const onErrorRef = React.useRef(onError);
  invokeRef.current = invoke;
  onOpenRef.current = onOpen;
  onErrorRef.current = onError;

  // Adjusted during render rather than in an effect, so no committed frame
  // ever shows the previous candidate's target under the new text.
  const [candidate, setCandidate] = React.useState({ text, senderPubkey });
  if (candidate.text !== text || candidate.senderPubkey !== senderPubkey) {
    generationRef.current += 1;
    stateRef.current = { status: "idle" };
    inFlightRef.current = null;
    setCandidate({ text, senderPubkey });
    setState({ status: "idle" });
  }

  const run = React.useCallback((): Promise<PathLinkResolution> => {
    const current = stateRef.current;
    if (current.status === "pending" && inFlightRef.current) {
      return inFlightRef.current;
    }
    if (current.status !== "idle") return Promise.resolve(current);
    const generation = generationRef.current;
    setState({ status: "pending" });
    stateRef.current = { status: "pending" };
    const attempt = (async (): Promise<PathLinkResolution> => {
      try {
        const target = await resolvePathLink(
          text,
          senderPubkey,
          (command, args) => invokeRef.current(command, args),
        );
        // Superseded: the reset during render already returned the token to
        // `idle` for its new candidate, so this answer is simply dropped.
        if (generationRef.current !== generation) return { status: "idle" };
        const settled: PathLinkResolution = target
          ? { status: "link", target }
          : { status: "text" };
        stateRef.current = settled;
        if (mountedRef.current) setState(settled);
        return settled;
      } catch (error) {
        if (generationRef.current !== generation) return { status: "idle" };
        // A failure from the resolver — a refusal, a permission error, a
        // mount that is away — is not "not a link". Say so, and return the
        // token to `idle` so the control stays and a later hover or click
        // asks again, rather than freezing it as text for the life of the
        // render.
        const settled: PathLinkResolution = { status: "idle" };
        stateRef.current = settled;
        if (mountedRef.current) setState(settled);
        onErrorRef.current(errorMessage(error, "Couldn't check that path."));
        return settled;
      }
    })();
    inFlightRef.current = attempt;
    void attempt.finally(() => {
      if (inFlightRef.current === attempt) inFlightRef.current = null;
    });
    return attempt;
  }, [senderPubkey, text]);

  const resolve = React.useCallback(() => {
    void run();
  }, [run]);

  const activate = React.useCallback(() => {
    const generation = generationRef.current;
    void run().then((settled) => {
      // The consent a click carries belongs to the token that was clicked. If
      // the message was edited while the resolution was in flight, the user
      // would be opening a file the visible text no longer names.
      if (generationRef.current !== generation) return;
      if (settled.status === "link") onOpenRef.current(settled.target);
    });
  }, [run]);

  return { state, resolve, activate };
}
