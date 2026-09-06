import * as React from "react";
import { toast } from "sonner";

import { invokeTauri } from "@/shared/api/tauri";
import { cn } from "@/shared/lib/cn";

import { usePathLinkResolution } from "./pathLinkResolution";
import { localMarkdownDocUrl, type PathLinkTarget } from "./pathLinks";
import { useMarkdownDocViewer } from "./markdownDocViewerContext";
import { useMarkdownRuntime } from "./runtimeContext";

type PathLinkChipProps = React.ComponentProps<"code"> & {
  /** The token's literal text — the path candidate to resolve. */
  text: string;
};

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
 *
 * The resolution lives in `usePathLinkResolution`, which retires an answer
 * whose message has since been edited, so what a click opens is always what
 * the token it was aimed at said.
 */
export function PathLinkChip({
  children,
  className,
  text,
  ...props
}: PathLinkChipProps) {
  const { pathLinkSenderPubkey } = useMarkdownRuntime();
  const openMarkdownDoc = useMarkdownDocViewer();
  const senderPubkey = pathLinkSenderPubkey ?? null;

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

  const { state, resolve, activate } = usePathLinkResolution({
    invoke: (command, args) => invokeTauri(command, args),
    onError: (message) => toast.error(message),
    onOpen: open,
    senderPubkey,
    text,
  });

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
        onFocus={resolve}
        onPointerEnter={resolve}
        title={isLink ? state.target.path : undefined}
        type="button"
      >
        {children}
      </button>
    </code>
  );
}
