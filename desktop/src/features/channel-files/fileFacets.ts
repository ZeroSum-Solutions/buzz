/**
 * Facet classification and sorting for the Files tab.
 *
 * The tab renders the attachment index, which is already bounded (see
 * `channelFilesIndex.ts`), so this module never grows the set — it filters and
 * orders it. Two properties it has to keep:
 *
 * - **One array.** Filtering allocates exactly one array and the sort runs in
 *   place on it, so a facet change costs one copy of the *references*, never a
 *   second copy of the entries.
 * - **Every string it reads is capped.** The filename arrives from a relay
 *   event and the author name from a relay profile; both are compared through
 *   capped sort keys, so no comparison cost rides on an attacker-chosen
 *   length. The caps here are the ones `boundedImeta.ts` already applies —
 *   this module never widens one.
 */

import { MAX_FILENAME_LENGTH, attachmentLabel } from "./boundedImeta";
import type { ChannelFile } from "./useChannelFiles";

/** Characters of a filename suffix examined for a type. Longer: no extension. */
export const MAX_EXTENSION_LENGTH = 16;
/** Characters of a relay-supplied display name the author sort compares. */
export const MAX_AUTHOR_SORT_KEY_LENGTH = 100;
/** Characters accepted from the search box. */
export const MAX_SEARCH_QUERY_LENGTH = 200;

/** The type facets the tab offers. `all` is the unfiltered view. */
export type FileFacet = "all" | "image" | "video" | "document" | "other";

/** The facet a row is counted and filtered under. */
export type ConcreteFileFacet = Exclude<FileFacet, "all">;

/** The sort keys the tab offers. */
export type FileSortKey = "newest" | "oldest" | "name" | "size" | "author";

/**
 * Filename extensions that name a document whatever the MIME type says. A
 * Markdown file sent as `application/octet-stream` is the case this exists
 * for: the sender's client did not know the type, and the name did.
 */
const DOCUMENT_EXTENSIONS: ReadonlySet<string> = new Set([
  "md",
  "pdf",
  "html",
  "docx",
  "csv",
]);

/** The fields classification reads. */
export type FileFacetSource = Pick<ChannelFile, "filename" | "mimeType">;

/**
 * The lowercased extension of `filename`, or `undefined` when it has none, the
 * dot starts the name (a dotfile), or the suffix is longer than
 * {@link MAX_EXTENSION_LENGTH}.
 *
 * Exported so the length bound is testable directly: no real extension is long
 * enough for the cap to change a classification, so a test that only went
 * through {@link classifyFile} could not tell the guard from its absence.
 */
export function extensionOf(filename: string | undefined): string | undefined {
  if (!filename) return undefined;
  const lastSlash = filename.lastIndexOf("/");
  const base = lastSlash === -1 ? filename : filename.slice(lastSlash + 1);
  const dot = base.lastIndexOf(".");
  if (dot <= 0 || dot === base.length - 1) return undefined;
  const suffix = base.slice(dot + 1);
  if (suffix.length > MAX_EXTENSION_LENGTH) return undefined;
  return suffix.toLowerCase();
}

/** The facet a MIME type alone implies. */
function classifyMimeType(mimeType: string): ConcreteFileFacet {
  if (mimeType.startsWith("image/")) return "image";
  if (mimeType.startsWith("video/")) return "video";
  if (
    mimeType.includes("pdf") ||
    mimeType.includes("document") ||
    mimeType.includes("spreadsheet") ||
    mimeType.includes("presentation") ||
    mimeType.startsWith("text/") ||
    mimeType.includes("json") ||
    mimeType.includes("xml")
  ) {
    return "document";
  }
  return "other";
}

/**
 * The facet a row belongs to: the `imeta` filename decides first, the MIME
 * type second. Total — every row lands in exactly one facet.
 */
export function classifyFile(file: FileFacetSource): ConcreteFileFacet {
  const extension = extensionOf(file.filename);
  if (extension !== undefined && DOCUMENT_EXTENSIONS.has(extension)) {
    return "document";
  }
  return classifyMimeType(file.mimeType);
}

/** Row count per facet, for the facet buttons. Each row is counted once. */
export function countFacets(
  files: readonly ChannelFile[],
): Record<FileFacet, number> {
  const counts: Record<FileFacet, number> = {
    all: files.length,
    image: 0,
    video: 0,
    document: 0,
    other: 0,
  };
  for (const file of files) counts[classifyFile(file)] += 1;
  return counts;
}

/** Options for {@link applyFileFacets}. Every field has a stable default. */
export type FileFacetOptions = {
  /** Type filter; `all` keeps every row. */
  facet?: FileFacet;
  /** Free-text filter over the filename and the caption. */
  query?: string;
  /** Ordering. Every key is total: ties break on the entry id. */
  sort?: FileSortKey;
  /** pubkey → display name, used only by the `author` sort. */
  authorNames?: ReadonlyMap<string, string>;
};

function cap(value: string, maxLength: number): string {
  return value.length > maxLength ? value.slice(0, maxLength) : value;
}

function matchesQuery(file: ChannelFile, needle: string): boolean {
  return (
    (file.filename ?? "").toLowerCase().includes(needle) ||
    (file.caption ?? "").toLowerCase().includes(needle)
  );
}

/** Compare entry ids so every sort is total and independent of input order. */
function compareIds(a: ChannelFile, b: ChannelFile): number {
  if (a.key === b.key) return 0;
  return a.key < b.key ? -1 : 1;
}

/**
 * The capped, lowercased label the name sort compares — the same label the row
 * displays, so the order the user reads is the order they see.
 */
function nameSortKey(file: ChannelFile): string {
  return cap(
    attachmentLabel(file.filename, file.rawUrl),
    MAX_FILENAME_LENGTH,
  ).toLowerCase();
}

/**
 * The capped, lowercased display name, or `null` when this author has no
 * usable name. A blank name is not a name.
 */
function authorSortKey(
  file: ChannelFile,
  authorNames: ReadonlyMap<string, string> | undefined,
): string | null {
  const name = authorNames?.get(file.pubkey)?.trim();
  if (!name) return null;
  return cap(name, MAX_AUTHOR_SORT_KEY_LENGTH).toLowerCase();
}

/**
 * Filter and order the tab's rows.
 *
 * Returns one newly allocated array — never the caller's — holding references
 * to the same rows, ordered by `sort`. The caller's array is never mutated.
 */
export function applyFileFacets(
  files: readonly ChannelFile[],
  options: FileFacetOptions = {},
): ChannelFile[] {
  const { facet = "all", query = "", sort = "newest", authorNames } = options;
  const needle = query.trim().toLowerCase();

  // One allocation: the rows that survive the filter. The sort below runs in
  // place on it, so a render never holds two copies of the entry list.
  const result: ChannelFile[] = [];
  for (const file of files) {
    if (facet !== "all" && classifyFile(file) !== facet) continue;
    if (needle !== "" && !matchesQuery(file, needle)) continue;
    result.push(file);
  }

  switch (sort) {
    case "oldest":
      result.sort((a, b) => a.createdAt - b.createdAt || compareIds(a, b));
      break;
    case "name": {
      // Sort keys are derived once per row, not once per comparison, so the
      // string work stays O(rows) while the comparisons stay O(rows log rows).
      const keys = new Map<ChannelFile, string>();
      for (const file of result) keys.set(file, nameSortKey(file));
      result.sort(
        (a, b) =>
          (keys.get(a) ?? "").localeCompare(keys.get(b) ?? "") ||
          compareIds(a, b),
      );
      break;
    }
    case "size":
      // An unknown size reads as -1, below every accepted size (which is
      // always positive), so it sorts last instead of among the smallest.
      result.sort(
        (a, b) => (b.size ?? -1) - (a.size ?? -1) || compareIds(a, b),
      );
      break;
    case "author": {
      const keys = new Map<ChannelFile, string | null>();
      for (const file of result)
        keys.set(file, authorSortKey(file, authorNames));
      result.sort((a, b) => {
        const left = keys.get(a) ?? null;
        const right = keys.get(b) ?? null;
        if (left !== null && right !== null) {
          const byName = left.localeCompare(right);
          if (byName !== 0) return byName;
        } else if (left !== right) {
          // An unknown author sorts last; the list still shows the row.
          return left === null ? 1 : -1;
        }
        if (a.pubkey !== b.pubkey) return a.pubkey < b.pubkey ? -1 : 1;
        return compareIds(a, b);
      });
      break;
    }
    default:
      result.sort((a, b) => b.createdAt - a.createdAt || compareIds(a, b));
      break;
  }

  return result;
}
