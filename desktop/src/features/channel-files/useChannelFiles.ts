import { useMemo } from "react";
import { useChannelMessagesQuery } from "@/features/messages/hooks";
import { parseImetaTags } from "@/shared/ui/markdown/parseImeta";
import { rewriteRelayUrl } from "@/shared/lib/mediaUrl";
import type { Channel, RelayEvent } from "@/shared/api/types";
import type { ParsedImetaEntry } from "@/shared/ui/markdown/parseImeta";

export type ChannelFile = {
  /** Unique key (event id + url). */
  key: string;
  /** The relay-rewritten media URL for download/display. */
  url: string;
  /** Raw media URL from imeta. */
  rawUrl: string;
  /** MIME type (e.g. "image/png", "application/pdf"). */
  mimeType: string;
  /** File size in bytes, if available. */
  size: number | undefined;
  /** Original filename. */
  filename: string | undefined;
  /** SHA-256 hex, if available. */
  sha256: string | undefined;
  /** Thumbnail URL, if available. */
  thumb: string | undefined;
  /** Dimensions string (WxH), if available. */
  dim: string | undefined;
  /** Blurhash, if available. */
  blurhash: string | undefined;
  /** Sender pubkey. */
  pubkey: string;
  /** When the message was created (Unix seconds). */
  createdAt: number;
  /** The parent message event ID. */
  eventId: string;
  /** First line of the message body (caption/description). */
  caption: string | undefined;
  /** All parsed imeta fields. */
  imeta: ParsedImetaEntry;
};

// Relay-sourced string/count caps applied at the DTO boundary — a hostile
// or buggy event must not be able to hand the UI an unbounded string to
// render or an unbounded number of attachments to parse from one message.
const MAX_FILENAME_LENGTH = 300;
const MAX_CAPTION_LENGTH = 500;
const MAX_MIME_TYPE_LENGTH = 100;
/** A real message carries at most a handful of attachments; this is generous headroom, not a product limit. */
const MAX_IMETA_ENTRIES_PER_EVENT = 20;

function capString(value: string, maxLength: number): string {
  return value.length > maxLength ? value.slice(0, maxLength) : value;
}

/** File type categories for filtering. */
export type FileCategory = "all" | "image" | "video" | "document" | "other";

export function categorizeFile(mimeType: string): FileCategory {
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
  )
    return "document";
  return "other";
}

export type FileSort = "newest" | "oldest" | "name" | "size";

export function sortFiles(files: ChannelFile[], sort: FileSort): ChannelFile[] {
  const sorted = [...files];
  switch (sort) {
    case "oldest":
      sorted.sort((a, b) => a.createdAt - b.createdAt);
      break;
    case "name":
      sorted.sort((a, b) => {
        const na = (a.filename ?? "").toLowerCase();
        const nb = (b.filename ?? "").toLowerCase();
        return na.localeCompare(nb);
      });
      break;
    case "size":
      sorted.sort((a, b) => (b.size ?? 0) - (a.size ?? 0));
      break;
    default:
      sorted.sort((a, b) => b.createdAt - a.createdAt);
      break;
  }
  return sorted;
}

/**
 * Extract all file-bearing events from a list of channel message events and
 * parse their imeta tags into a flat list of {@link ChannelFile} objects,
 * ordered newest-first. Pure — exported so the parsing rules (caption
 * extraction, filename-over-".bin" labeling, imeta field mapping) are
 * testable directly against the production code path {@link useChannelFiles}
 * calls, not a test-only reimplementation.
 */
export function parseChannelFiles(events: RelayEvent[]): ChannelFile[] {
  const result: ChannelFile[] = [];

  for (let i = events.length - 1; i >= 0; i--) {
    const event = events[i];
    const tags = event.tags;
    if (!tags || tags.length === 0) continue;

    const entries = parseImetaTags(tags as string[][]);
    if (entries.size === 0) continue;

    // Extract first non-empty line of content as caption
    let caption: string | undefined;
    const content = event.content;
    if (content != null) {
      const firstLine = content.trim().split("\n")[0];
      if (firstLine) {
        caption =
          firstLine.replace(/!\[(?:image|video)\]\([^)]+\)/g, "").trim() ||
          undefined;
      }
    }
    if (caption != null) caption = capString(caption, MAX_CAPTION_LENGTH);

    // Cap the number of attachments pulled from a single event, not just
    // their string fields — an event with an unrealistic imeta-tag count
    // must not translate into an unbounded render list.
    let entryCount = 0;
    for (const [, entry] of entries) {
      if (entryCount >= MAX_IMETA_ENTRIES_PER_EVENT) break;
      entryCount += 1;
      result.push({
        key: `${event.id}-${entry.url}`,
        url: rewriteRelayUrl(entry.url),
        rawUrl: entry.url,
        mimeType: capString(
          entry.m ?? "application/octet-stream",
          MAX_MIME_TYPE_LENGTH,
        ),
        size: entry.size != null && entry.size > 0 ? entry.size : undefined,
        filename:
          entry.filename != null
            ? capString(entry.filename, MAX_FILENAME_LENGTH)
            : undefined,
        sha256: entry.x,
        thumb: entry.thumb ? rewriteRelayUrl(entry.thumb) : undefined,
        dim: entry.dim,
        blurhash: entry.blurhash,
        pubkey: event.pubkey,
        createdAt: event.created_at,
        eventId: event.id,
        caption: caption || undefined,
        imeta: entry,
      });
    }
  }

  return result;
}

/**
 * Extract all file-bearing events from a channel and parse their imeta tags
 * into a flat list of {@link ChannelFile} objects, ordered newest-first.
 */
export function useChannelFiles(activeChannel: Channel | null): {
  files: ChannelFile[];
  isLoading: boolean;
} {
  const messagesQuery = useChannelMessagesQuery(activeChannel);

  const files = useMemo(
    () => parseChannelFiles(messagesQuery.data ?? []),
    [messagesQuery.data],
  );

  return {
    files,
    isLoading: messagesQuery.isPending,
  };
}
