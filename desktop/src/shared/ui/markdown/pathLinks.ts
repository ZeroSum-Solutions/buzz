/**
 * Shape gate and IPC wrapper for local file paths written as inline code in a
 * message.
 *
 * Agents post report paths as text (`audit/verify/report.md`,
 * `buzz/approvals/item-7.html`). This module decides which inline-code tokens
 * *look* like a path and carries the click/hover resolution to the native
 * `resolve_path_link` command, which is the only place a filesystem is
 * touched. Nothing here reads a file, and nothing is resolved while a channel
 * renders — a token stays plain text until a hover or a click asks.
 *
 * The gate here is a cheap pre-filter, not the security boundary. Root
 * containment, symlink escape and the regular-file check all live in Rust
 * (`commands/path_links.rs`); this file only keeps obvious non-paths and
 * anything over the DTO's byte cap off the IPC channel entirely.
 *
 * Kept DOM-free so it is unit-testable without a webview.
 */

/**
 * Maximum UTF-8 byte length of a candidate the DTO will carry.
 *
 * Message content is relay-sourced and unbounded, so the cap is applied
 * before the string reaches the command. The native side enforces the same
 * number (`MAX_PATH_LINK_BYTES` in `commands/path_links.rs`) and refuses an
 * over-length candidate before any filesystem call — keep the two in sync.
 */
export const MAX_PATH_LINK_BYTES = 4096;

/**
 * Extensions that make a slash-free token a path candidate.
 *
 * A token with no slash is a path only when it names a document; a bare word
 * (`cargo`, `SIGKILL`) never qualifies, and neither does a slash-free script
 * or binary name — those are the tokens most likely to be prose or a command.
 *
 * The same list is the *native* allowlist for what a resolved link may be
 * (`OPENABLE_EXTENSIONS` in `commands/path_links.rs`): a token with a slash
 * still reaches the resolver, and the resolver refuses anything the OS
 * default handler would run rather than display. Keep the two in sync.
 */
const DOCUMENT_EXTENSIONS = [
  ".md",
  ".markdown",
  ".mdx",
  ".html",
  ".htm",
  ".pdf",
  ".csv",
  ".json",
  ".txt",
  ".log",
  ".yml",
  ".yaml",
  ".toml",
] as const;

/** Extensions the in-app markdown viewer renders. */
const MARKDOWN_EXTENSIONS = [".md", ".markdown", ".mdx"] as const;

/**
 * Scheme prefix marking a `?doc=` panel target as a local file rather than a
 * relay `/media/` URL. The panel reads local documents through
 * `read_path_link_markdown`, which re-resolves the path against the allowed
 * roots; a relay URL keeps the authenticated media fetch.
 */
const LOCAL_DOC_SCHEME = "buzz-local-file:";

/** What the native resolver decided a candidate is. */
export type PathLinkKind = "markdown" | "file";

/** A candidate that resolved to a real file inside an allowed root. */
export type PathLinkTarget = {
  /** Canonical absolute path, as resolved and contained natively. */
  path: string;
  /** Final path component, for the link title and the panel header. */
  filename: string;
  /**
   * `markdown` opens in the in-app viewer; `file` goes to the OS opener, and
   * the resolver has already proven it is an inert, non-executable document.
   */
  kind: PathLinkKind;
  /** Size of the resolved file in bytes. */
  sizeBytes: number;
};

/** The Tauri `invoke` seam, narrowed to what this module sends. */
export type PathLinkInvoke = (
  command: string,
  payload: Record<string, unknown>,
) => Promise<unknown>;

function utf8ByteLength(value: string): number {
  return new TextEncoder().encode(value).length;
}

/**
 * True when an inline-code token looks enough like a local path to be worth
 * one native resolution on hover or click.
 *
 * Refused outright, before any IPC: anything over the DTO byte cap, anything
 * carrying whitespace or a control character, a URL, a `~`-relative path, a
 * `..` traversal segment, and any bare word without a slash or a document
 * extension. A `true` here means "ask the resolver", never "this is a link" —
 * the resolver still has to find a regular file inside an allowed root.
 */
export function isPathLinkCandidate(text: string): boolean {
  // UTF-8 is never shorter than the UTF-16 code-unit count, so this rejects
  // an over-cap string without encoding it.
  if (text.length > MAX_PATH_LINK_BYTES) return false;
  if (text.length === 0) return false;
  if (utf8ByteLength(text) > MAX_PATH_LINK_BYTES) return false;

  // Whitespace (including the NBSP that pasted prose carries) and control
  // characters: a path token in a report never has them, and allowing them
  // would let one token smuggle a second argument to the opener.
  if (/[\s\p{Cc}]/u.test(text)) return false;

  // A URL is handled by the anchor renderer, never by the path resolver.
  if (text.includes("://")) return false;
  // No home expansion: `~` is not resolved anywhere in this path.
  if (text.startsWith("~")) return false;

  const segments = text.split("/");
  // `..` as a whole segment is a traversal attempt. `two..dots.md` is not.
  if (segments.some((segment) => segment === "..")) return false;

  // Must name something. `///` and `...` are not paths.
  if (!/[A-Za-z0-9]/.test(text)) return false;

  const lower = text.toLowerCase();
  const hasDocumentExtension = DOCUMENT_EXTENSIONS.some((extension) =>
    lower.endsWith(extension),
  );
  return text.includes("/") || hasDocumentExtension;
}

/** True when a resolved target opens in the in-app markdown viewer. */
export function isMarkdownPathLink(target: PathLinkTarget): boolean {
  const lower = target.filename.toLowerCase();
  return (
    target.kind === "markdown" &&
    MARKDOWN_EXTENSIONS.some((extension) => lower.endsWith(extension))
  );
}

/**
 * Validate the resolver's answer at the boundary.
 *
 * Returns `null` for `null`/`undefined` (the "not a link" answer) and for any
 * shape that is not a complete target, so a malformed reply renders as text
 * instead of a link pointing nowhere.
 */
export function parsePathLinkTarget(value: unknown): PathLinkTarget | null {
  if (value === null || typeof value !== "object") return null;
  const candidate = value as Partial<PathLinkTarget>;
  if (typeof candidate.path !== "string" || candidate.path.length === 0) {
    return null;
  }
  if (
    typeof candidate.filename !== "string" ||
    candidate.filename.length === 0
  ) {
    return null;
  }
  if (candidate.kind !== "markdown" && candidate.kind !== "file") return null;
  if (
    typeof candidate.sizeBytes !== "number" ||
    !Number.isFinite(candidate.sizeBytes)
  ) {
    return null;
  }
  return {
    path: candidate.path,
    filename: candidate.filename,
    kind: candidate.kind,
    sizeBytes: candidate.sizeBytes,
  };
}

/**
 * Resolve one candidate through the native command.
 *
 * Returns `null` without any IPC when the candidate fails the shape gate, and
 * `null` when the resolver reports "not a link" (a missing file, or a file
 * outside every allowed root). A resolver *error* — an over-length candidate
 * that got past the gate, an unreadable root — is re-thrown so the caller can
 * surface it rather than silently rendering plain text.
 */
export async function resolvePathLink(
  candidate: string,
  senderPubkey: string | null,
  invoke: PathLinkInvoke,
): Promise<PathLinkTarget | null> {
  if (!isPathLinkCandidate(candidate)) return null;
  const answer = await invoke("resolve_path_link", { candidate, senderPubkey });
  return parsePathLinkTarget(answer);
}

/** The `?doc=` panel target for a locally resolved markdown document. */
export function localMarkdownDocUrl(path: string): string {
  return `${LOCAL_DOC_SCHEME}${encodeURIComponent(path)}`;
}

/**
 * The local path behind a `?doc=` panel target, or `null` when the target is
 * a relay media URL (which the panel fetches over the authenticated path).
 */
export function parseLocalMarkdownDocUrl(url: string): string | null {
  if (!url.startsWith(LOCAL_DOC_SCHEME)) return null;
  try {
    const path = decodeURIComponent(url.slice(LOCAL_DOC_SCHEME.length));
    return path.length > 0 ? path : null;
  } catch {
    // A malformed percent-escape is not a path; the panel shows its load
    // error rather than sending the raw text to the filesystem.
    return null;
  }
}
