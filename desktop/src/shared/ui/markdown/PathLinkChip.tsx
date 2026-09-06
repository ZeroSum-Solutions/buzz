import * as React from "react";
import { toast } from "sonner";

import { invokeTauri } from "@/shared/api/tauri";
import { cn } from "@/shared/lib/cn";

import {
  localMarkdownDocUrl,
  type PathLinkTarget,
  resolvePathLink,
} from "./pathLinks";
import { useMarkdownDocViewer } from "./markdownDocViewerContext";
import { useMarkdownRuntime } from "./runtimeContext";

type PathLinkChipProps = React.ComponentProps<"code"> & {
  /** The token's literal text — the path candidate to resolve. */
  text: string;
};

type ResolutionState =
  /** Nothing asked yet. No IPC has happened for this token. */
  | { status: "idle" }
  /** A resolution is in flight; further triggers are ignored. */
  | { status: "pending" }
  /** The resolver answered "not a link"; the token stays plain text. */
  | { status: "text" }
  /** The resolver found a file. */
  | { status: "link"; target: PathLinkTarget };

function errorMessage(error: unknown, fallback: string): string {
  return error instanceof Error && error.message ? error.message : fallback;
}

/**
 * An inline-code token that *looks* like a local file path.
 *
 * Nothing is resolved while a channel renders: the token is plain inline code
 * until a pointer enters it, focus lands on it, or it is clicked, and only
 * then does one `resolve_path_link` call ask the filesystem whether the path
 * is a real file inside an allowed root. A token that resolves to nothing
 * leaves the tab order and stays text forever after; a token that resolves
 * becomes a link that opens the file — a markdown document in the in-app
 * viewer panel, anything else with the OS default handler. Neither path ever
 * executes the file, and neither asks the relay.
 */
export function PathLinkChip({
  children,
  className,
  text,
  ...props
}: PathLinkChipProps) {
  const { pathLinkSenderPubkey } = useMarkdownRuntime();
  const openMarkdownDoc = useMarkdownDocViewer();
  const [state, setState] = React.useState<ResolutionState>({ status: "idle" });
  // Resolution outlives a hover, so the setter must not write into an
  // unmounted token.
  const mountedRef = React.useRef(true);
  React.useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  const senderPubkey = pathLinkSenderPubkey ?? null;
  // `resolve` is the only writer of both; the ref lets a click act on the
  // resolution it just triggered without waiting for a render.
  const stateRef = React.useRef<ResolutionState>({ status: "idle" });

  /**
   * Resolve once, and answer with the settled state so a click can act on the
   * result of the resolution it triggered rather than waiting for a render.
   */
  const resolve = React.useCallback(async (): Promise<ResolutionState> => {
    const current = stateRef.current;
    if (current.status !== "idle") return current;
    setState({ status: "pending" });
    stateRef.current = { status: "pending" };
    try {
      const target = await resolvePathLink(
        text,
        senderPubkey,
        (command, args) => invokeTauri(command, args),
      );
      const settled: ResolutionState = target
        ? { status: "link", target }
        : { status: "text" };
      stateRef.current = settled;
      if (mountedRef.current) setState(settled);
      return settled;
    } catch (error) {
      // A refusal from the resolver is a real failure, not a "not a link":
      // say so instead of silently leaving the token as text.
      const settled: ResolutionState = { status: "text" };
      stateRef.current = settled;
      if (mountedRef.current) setState(settled);
      toast.error(errorMessage(error, "Couldn't check that path."));
      return settled;
    }
  }, [senderPubkey, text]);

  const open = React.useCallback(
    (target: PathLinkTarget) => {
      if (target.kind === "markdown" && openMarkdownDoc) {
        openMarkdownDoc({
          url: localMarkdownDocUrl(target.path),
          filename: target.filename,
        });
        return;
      }
      invokeTauri("open_path_link", {
        candidate: target.path,
        senderPubkey,
      }).catch((error: unknown) => {
        toast.error(errorMessage(error, `Couldn't open ${target.filename}.`));
      });
    },
    [openMarkdownDoc, senderPubkey],
  );

  const activate = React.useCallback(() => {
    void resolve().then((settled) => {
      if (settled.status === "link") open(settled.target);
    });
  }, [open, resolve]);

  const isLink = state.status === "link";
  // Resolution said "not a link": the token is ordinary text from here on, so
  // it drops its control and leaves the tab order rather than staying a focus
  // stop that does nothing.
  if (state.status === "text") {
    return (
      <code {...props} className={className} data-path-link="text">
        {children}
      </code>
    );
  }

  // A real <button> rather than a code element with a role: it is focusable
  // and Enter/Space-activated natively, so the pointer path and the keyboard
  // path cannot drift apart. Tailwind's preflight makes the button inherit the
  // surrounding code font, so the chip still reads as inline code.
  return (
    <code {...props} className={className} data-path-link={state.status}>
      <button
        aria-label={isLink ? `Open ${state.target.filename}` : undefined}
        className={cn(
          "font-[inherit] text-[inherit]",
          isLink &&
            "cursor-pointer underline decoration-dotted underline-offset-2",
        )}
        onClick={activate}
        onFocus={() => {
          void resolve();
        }}
        onPointerEnter={() => {
          void resolve();
        }}
        title={isLink ? state.target.path : undefined}
        type="button"
      >
        {children}
      </button>
    </code>
  );
}
