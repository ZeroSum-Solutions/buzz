import type * as React from "react";

import { cn } from "@/shared/lib/cn";
import { INLINE_CODE_CHIP_CLASS } from "@/shared/ui/mentionChip";

import {
  CODE_BLOCK_CLASS,
  extractLanguage,
  SyntaxHighlightedCode,
} from "./CodeBlock";
import { isPathLinkCandidate } from "./pathLinks";
import { PathLinkChip } from "./PathLinkChip";
import { useMarkdownRuntime } from "./runtimeContext";

/**
 * The Markdown `code` element renderer: fenced blocks, indented blocks, and
 * the inline chip.
 *
 * Module-level rather than closed over per mount, so the component map stays
 * identity-stable for the parsed-element cache (`nodeCache.ts`). Per-mount
 * state — which surface may resolve path links — arrives through the Markdown
 * runtime context instead.
 *
 * An inline token that looks like a local file path becomes a
 * {@link PathLinkChip} on message surfaces (the ones whose runtime sets
 * `pathLinkSenderPubkey`). The chip resolves nothing until it is hovered,
 * focused or clicked, so this branch costs one string test per inline code
 * token at render time and no IPC at all.
 */
export function MarkdownCode({
  children,
  className,
  ...props
}: React.ComponentProps<"code">) {
  const { pathLinkSenderPubkey } = useMarkdownRuntime();
  const rawCode = String(children);
  const code = rawCode.replace(/\n$/, "");
  const isFencedCodeBlock =
    typeof className === "string" && className.includes("language-");

  if (isFencedCodeBlock || rawCode.endsWith("\n") || code.includes("\n")) {
    const language = extractLanguage(className);

    if (language) {
      return (
        <SyntaxHighlightedCode code={code} language={language} {...props} />
      );
    }

    const lines = code.split("\n");
    return (
      <code {...props} className={CODE_BLOCK_CLASS}>
        {lines.map((line, i) => (
          // biome-ignore lint/suspicious/noArrayIndexKey: lines are positional
          <span key={i} data-line="">
            {line}
          </span>
        ))}
      </code>
    );
  }

  const inlineClassName = cn(INLINE_CODE_CHIP_CLASS, className);
  if (pathLinkSenderPubkey !== undefined && isPathLinkCandidate(code)) {
    return (
      <PathLinkChip {...props} className={inlineClassName} text={code}>
        {children}
      </PathLinkChip>
    );
  }

  return (
    <code {...props} className={inlineClassName}>
      {children}
    </code>
  );
}
