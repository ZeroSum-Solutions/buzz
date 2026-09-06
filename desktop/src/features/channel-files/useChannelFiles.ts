import { rewriteRelayUrl } from "@/shared/lib/mediaUrl";
import type { RelayEvent } from "@/shared/api/types";
import { extractCaption, parseBoundedImeta } from "./boundedImeta";
import { fileKeyFor } from "./folderStore";

export type ChannelFile = {
  /** Stable per-attachment identity: message id + a digest of the URL. */
  key: string;
  /** The relay-rewritten media URL for download/display. */
  url: string;
  /** Raw media URL from imeta (validated http(s), length-capped). */
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
};

/**
 * The fields {@link parseChannelFiles} reads. A {@link RelayEvent} satisfies
 * it, and so does the capped copy the attachment index keeps, so both project
 * through the same code.
 */
export type ChannelFileSourceEvent = Pick<
  RelayEvent,
  "id" | "pubkey" | "created_at" | "content" | "tags"
>;

/**
 * Total attachments one projection will produce for a channel. The projection
 * runs over every attachment-bearing event the index holds, which grows as the
 * backfill pages back, so the bound has to sit on the thing that costs — rows
 * produced — rather than on any single event.
 */
export const MAX_CHANNEL_FILES = 2_000;

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
 * Project a list of channel message events into a flat, newest-first list of
 * {@link ChannelFile} rows. Pure, bounded, and exported so the parsing rules
 * (caption extraction, filename-over-".bin" labelling, field caps, the total
 * row cap) are tested against the code path the Files tab renders from.
 *
 * Returns `truncated: true` when the row cap stopped the projection, so the
 * UI can say the list is partial instead of presenting it as the whole set.
 */
export function parseChannelFiles(events: readonly ChannelFileSourceEvent[]): {
  files: ChannelFile[];
  truncated: boolean;
} {
  const files: ChannelFile[] = [];

  for (let i = events.length - 1; i >= 0; i--) {
    if (files.length >= MAX_CHANNEL_FILES) {
      return { files, truncated: true };
    }
    const event = events[i];
    const tags = event.tags;
    if (!tags || tags.length === 0) continue;

    const attachments = parseBoundedImeta(tags as string[][]);
    if (attachments.length === 0) continue;

    const caption = extractCaption(event.content ?? null);

    for (const attachment of attachments) {
      if (files.length >= MAX_CHANNEL_FILES) {
        return { files, truncated: true };
      }
      files.push({
        key: fileKeyFor(event.id, attachment.url),
        url: rewriteRelayUrl(attachment.url),
        rawUrl: attachment.url,
        mimeType: attachment.mimeType,
        size: attachment.size,
        filename: attachment.filename,
        sha256: attachment.sha256,
        thumb: attachment.thumb ? rewriteRelayUrl(attachment.thumb) : undefined,
        dim: attachment.dim,
        blurhash: attachment.blurhash,
        pubkey: event.pubkey,
        createdAt: event.created_at,
        eventId: event.id,
        caption,
      });
    }
  }

  return { files, truncated: false };
}
