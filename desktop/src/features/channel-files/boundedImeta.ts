/**
 * Bounded imeta parsing for the Files tab.
 *
 * The shared {@link import("@/shared/ui/markdown/parseImeta").parseImetaTags}
 * walks every tag and every part of a relay event and materialises the whole
 * map before any caller-side cap runs. The Files tab projects *every* loaded
 * message, so that scan is the hot path: the bounds below stop the scan
 * itself, validate each field before it is stored, and keep nothing but the
 * capped copies — a hostile event costs a bounded amount of work, memory and
 * DOM, not a bounded-looking render over unbounded strings.
 */

/** A real message carries a handful of attachments; generous headroom. */
export const MAX_ATTACHMENTS_PER_EVENT = 20;
/** Tags scanned before giving up on an event, however many it declares. */
export const MAX_IMETA_TAGS_SCANNED = 64;
/** Parts scanned inside one imeta tag. */
export const MAX_IMETA_PARTS_PER_TAG = 32;
/** A single part longer than this is skipped rather than sliced and kept. */
export const MAX_IMETA_PART_LENGTH = 4_096;
export const MAX_URL_LENGTH = 2_048;
export const MAX_FILENAME_LENGTH = 300;
export const MAX_MIME_TYPE_LENGTH = 100;
export const MAX_DIM_LENGTH = 32;
export const MAX_BLURHASH_LENGTH = 256;
/** Content bytes read before caption extraction. */
export const MAX_CONTENT_PREFIX_LENGTH = 2_000;
export const MAX_CAPTION_LENGTH = 500;
/** Largest attachment size accepted from a relay-supplied `size` field (1 TiB). */
export const MAX_FILE_SIZE_BYTES = 1_099_511_627_776;

const SHA256_PATTERN = /^[0-9a-f]{64}$/;
const DIM_PATTERN = /^\d{1,6}x\d{1,6}$/;
const HTTP_URL_PATTERN = /^https?:\/\/\S+$/i;

/** Only capped, validated fields — the raw parsed entry is never retained. */
export type BoundedAttachment = {
  url: string;
  mimeType: string;
  size: number | undefined;
  filename: string | undefined;
  sha256: string | undefined;
  thumb: string | undefined;
  dim: string | undefined;
  blurhash: string | undefined;
};

function cap(value: string, maxLength: number): string {
  return value.length > maxLength ? value.slice(0, maxLength) : value;
}

function acceptUrl(value: string): string | undefined {
  if (value.length > MAX_URL_LENGTH) return undefined;
  return HTTP_URL_PATTERN.test(value) ? value : undefined;
}

function acceptSize(value: string): number | undefined {
  if (value.length > 20) return undefined;
  const parsed = Number.parseInt(value, 10);
  if (!Number.isSafeInteger(parsed)) return undefined;
  if (parsed <= 0 || parsed > MAX_FILE_SIZE_BYTES) return undefined;
  return parsed;
}

/**
 * Parse at most {@link MAX_ATTACHMENTS_PER_EVENT} attachments out of an
 * event's tags, stopping the scan as soon as that many are found.
 */
export function parseBoundedImeta(tags: string[][]): BoundedAttachment[] {
  const result: BoundedAttachment[] = [];
  const seen = new Set<string>();
  let tagsScanned = 0;

  for (const tag of tags) {
    if (result.length >= MAX_ATTACHMENTS_PER_EVENT) break;
    if (tagsScanned >= MAX_IMETA_TAGS_SCANNED) break;
    if (tag[0] !== "imeta") continue;
    tagsScanned += 1;

    let url: string | undefined;
    let mimeType: string | undefined;
    let size: number | undefined;
    let filename: string | undefined;
    let sha256: string | undefined;
    let thumb: string | undefined;
    let dim: string | undefined;
    let blurhash: string | undefined;

    const partCount = Math.min(tag.length, MAX_IMETA_PARTS_PER_TAG + 1);
    for (let index = 1; index < partCount; index += 1) {
      const part = tag[index];
      if (typeof part !== "string") continue;
      if (part.length > MAX_IMETA_PART_LENGTH) continue;
      const spaceIdx = part.indexOf(" ");
      if (spaceIdx === -1) continue;
      const key = part.slice(0, spaceIdx);
      const value = part.slice(spaceIdx + 1);
      switch (key) {
        case "url":
          url = acceptUrl(value);
          break;
        case "m":
          mimeType = cap(value, MAX_MIME_TYPE_LENGTH);
          break;
        case "x":
          sha256 = SHA256_PATTERN.test(value) ? value : undefined;
          break;
        case "size":
          size = acceptSize(value);
          break;
        case "dim":
          dim = DIM_PATTERN.test(value)
            ? cap(value, MAX_DIM_LENGTH)
            : undefined;
          break;
        case "blurhash":
          blurhash = cap(value, MAX_BLURHASH_LENGTH);
          break;
        case "thumb":
          thumb = acceptUrl(value);
          break;
        case "filename":
          filename = cap(value, MAX_FILENAME_LENGTH);
          break;
        default:
          break;
      }
    }

    if (!url || seen.has(url)) continue;
    seen.add(url);
    result.push({
      url,
      mimeType: mimeType ?? "application/octet-stream",
      size,
      filename,
      sha256,
      thumb,
      dim,
      blurhash,
    });
  }

  return result;
}

/** First non-empty line of a bounded prefix of the message body. */
export function extractCaption(content: string | null): string | undefined {
  if (content == null) return undefined;
  const prefix = content.slice(0, MAX_CONTENT_PREFIX_LENGTH);
  const firstLine = prefix.trim().split("\n")[0];
  if (!firstLine) return undefined;
  const stripped = firstLine
    .replace(/!\[(?:image|video)\]\([^)]+\)/g, "")
    .trim();
  if (!stripped) return undefined;
  return cap(stripped, MAX_CAPTION_LENGTH);
}

/** Display label for a row: the imeta filename, else a bounded URL tail. */
export function attachmentLabel(
  filename: string | undefined,
  rawUrl: string,
): string {
  if (filename) return filename;
  const tail = rawUrl.split("/").pop() ?? "";
  return cap(tail, MAX_FILENAME_LENGTH) || "file";
}
