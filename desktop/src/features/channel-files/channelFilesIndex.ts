/**
 * The channel attachment index.
 *
 * The Files tab used to project the loaded message window
 * (`useChannelMessagesQuery`), which is top-level only and 200 events per page:
 * a file posted in a thread, or older than the window, was simply absent
 * (issue #4428). This module holds the tab's own model instead — every event
 * of the channel that carries an `imeta` tag, plus the deletion and edit
 * markers that change what those events mean — fed by the live subscription
 * and a keyset backfill (`channelFilesBackfill.ts`).
 *
 * Everything here is pure and immutable: {@link ingestIndexEvents} returns a
 * new index, or the identical object when nothing changed, so React bails out
 * of a render that would show the same rows.
 *
 * Relay data is bounded on the way in, once, by {@link boundIndexSource}: the
 * index is long-lived and re-projected on every arrival, so an unbounded event
 * kept here would cost unbounded work and memory for as long as the tab is
 * open, not just for one render.
 *
 * The retention caps evict rather than refuse ({@link admitNewest}): a channel
 * past a cap keeps the newest entries and drops the oldest, so live arrivals
 * keep landing in a tab whose banner promises the most recent attachments.
 */

import { applyEditTagOverlay } from "@/features/messages/lib/applyEditTagOverlay.mjs";
import {
  KIND_DELETION,
  KIND_NIP29_DELETE_EVENT,
  KIND_STREAM_MESSAGE_EDIT,
} from "@/shared/constants/kinds";
import type { RelayEvent } from "@/shared/api/types";
import {
  MAX_ATTACHMENTS_PER_EVENT,
  MAX_CONTENT_PREFIX_LENGTH,
  MAX_IMETA_PART_LENGTH,
  MAX_IMETA_PARTS_PER_TAG,
  MAX_IMETA_TAGS_SCANNED,
} from "./boundedImeta";
import { type ChannelFile, parseChannelFiles } from "./useChannelFiles";

/**
 * Attachment-bearing events retained for one channel. At the backfill's page
 * bound (50 pages of 100) the index sees at most this many events, so the cap
 * is the memory ceiling for a channel whose whole history is attachments.
 *
 * The cap keeps the newest entries: past it a newer event evicts the oldest
 * retained one (see {@link admitNewest}), so a channel with more attachments
 * than this still shows the recent ones the tab claims to show.
 */
export const MAX_INDEXED_ATTACHMENT_EVENTS = 5_000;
/**
 * Deletion targets retained. Deletions are ids only, but still unbounded input.
 * Retained newest-first on the deletion event's own timestamp.
 */
export const MAX_INDEXED_DELETIONS = 5_000;
/**
 * Edited messages tracked. One entry per edited message, newest edit wins, and
 * the cap retains the newest edits.
 */
export const MAX_INDEXED_EDITS = 5_000;
/** Pubkeys are 64 hex characters; the cap is headroom, not a format check. */
export const MAX_INDEXED_PUBKEY_LENGTH = 128;

const EVENT_ID_PATTERN = /^[0-9a-f]{64}$/i;

/** One attachment-bearing event, reduced to the capped fields the tab uses. */
export type IndexedSource = {
  id: string;
  pubkey: string;
  created_at: number;
  /** `imeta` tags only, count- and length-capped. */
  tags: string[][];
  /** Capped prefix of the message body, for the row caption. */
  content: string;
};

/** The newest edit seen for one message, reduced the same way. */
export type IndexedEdit = {
  id: string;
  pubkey: string;
  created_at: number;
  tags: string[][];
  content: string;
};

/** Immutable snapshot of one channel's attachment index. */
export type FilesIndex = {
  /** Attachment-bearing events by event id. */
  readonly sources: ReadonlyMap<string, IndexedSource>;
  /**
   * Ids of messages a deletion marker removed, each mapped to the timestamp of
   * the deletion that removed it — the recency the retention cap orders by.
   */
  readonly deletions: ReadonlyMap<string, number>;
  /** Newest edit per edited message id. */
  readonly edits: ReadonlyMap<string, IndexedEdit>;
  /**
   * True once any cap above refused or evicted an entry: older attachments are
   * missing from the list, so the tab must say the list is partial.
   */
  readonly truncated: boolean;
};

const EMPTY_INDEX: FilesIndex = {
  sources: new Map(),
  deletions: new Map(),
  edits: new Map(),
  truncated: false,
};

/** An index with nothing in it. */
export function emptyFilesIndex(): FilesIndex {
  return EMPTY_INDEX;
}

function isEventId(value: unknown): value is string {
  return typeof value === "string" && EVENT_ID_PATTERN.test(value);
}

/**
 * A relay-supplied string field, type-checked and length-capped.
 *
 * `RelayEvent` describes what a well-behaved relay sends, not what arrives:
 * the inbound frame is `JSON.parse`d and cast (`relayClientSession.ts`,
 * `handleWsMessage`), so `pubkey` or `content` can be a number, an object or
 * absent. Reading one as a string without this check throws a `TypeError` out
 * of the ingest — on the live path, out of the relay's shared event dispatcher,
 * which then drops the rest of that batch for every other subscription.
 * Returning `""` keeps every field total, and the cap is the same one every
 * other relay-sourced string in this module gets.
 */
function boundRelayString(value: unknown, maxLength: number): string {
  return typeof value === "string" ? value.slice(0, maxLength) : "";
}

function isDeletionKind(kind: number): boolean {
  return kind === KIND_DELETION || kind === KIND_NIP29_DELETE_EVENT;
}

/**
 * Copy the `imeta` tags of an event, bounded on every axis a relay controls:
 * how many tags are examined, how many are kept, how many parts each keeps and
 * how long a part may be. Non-imeta tags are dropped — the tab reads
 * attachments, and an edit replaces the imeta set wholesale, so nothing else
 * is needed to project a row.
 */
function boundImetaTags(tags: string[][] | undefined): string[][] {
  if (!Array.isArray(tags)) return [];
  const kept: string[][] = [];
  const scanLimit = Math.min(tags.length, MAX_IMETA_TAGS_SCANNED);
  for (let index = 0; index < scanLimit; index += 1) {
    if (kept.length >= MAX_ATTACHMENTS_PER_EVENT) break;
    const tag = tags[index];
    if (!Array.isArray(tag) || tag[0] !== "imeta") continue;
    const parts: string[] = ["imeta"];
    const partCount = Math.min(tag.length, MAX_IMETA_PARTS_PER_TAG + 1);
    for (let part = 1; part < partCount; part += 1) {
      const value = tag[part];
      if (typeof value !== "string") continue;
      if (value.length > MAX_IMETA_PART_LENGTH) continue;
      parts.push(value);
    }
    kept.push(parts);
  }
  return kept;
}

/**
 * Reduce a relay event to the capped {@link IndexedSource} the index stores,
 * or `null` when it is not an attachment-bearing event this index can trust
 * (bad id, bad timestamp, or no `imeta` tag inside the scan budget).
 */
export function boundIndexSource(event: RelayEvent): IndexedSource | null {
  if (!isEventId(event?.id)) return null;
  if (!Number.isSafeInteger(event.created_at) || event.created_at < 0) {
    return null;
  }
  const tags = boundImetaTags(event.tags);
  if (tags.length === 0) return null;
  return {
    id: event.id,
    pubkey: boundRelayString(event.pubkey, MAX_INDEXED_PUBKEY_LENGTH),
    created_at: event.created_at,
    tags,
    content: boundRelayString(event.content, MAX_CONTENT_PREFIX_LENGTH),
  };
}

function boundIndexEdit(event: RelayEvent): IndexedEdit | null {
  if (!isEventId(event?.id)) return null;
  if (!Number.isSafeInteger(event.created_at) || event.created_at < 0) {
    return null;
  }
  return {
    id: event.id,
    pubkey: boundRelayString(event.pubkey, MAX_INDEXED_PUBKEY_LENGTH),
    created_at: event.created_at,
    tags: boundImetaTags(event.tags),
    content: boundRelayString(event.content, MAX_CONTENT_PREFIX_LENGTH),
  };
}

/** Ids referenced by a marker event's `e` tags. */
function referencedEventIds(tags: string[][] | undefined): string[] {
  if (!Array.isArray(tags)) return [];
  const ids: string[] = [];
  const scanLimit = Math.min(tags.length, MAX_IMETA_TAGS_SCANNED);
  for (let index = 0; index < scanLimit; index += 1) {
    const tag = tags[index];
    if (Array.isArray(tag) && tag[0] === "e" && isEventId(tag[1])) {
      ids.push(tag[1]);
    }
  }
  return ids;
}

/**
 * An edit may only rewrite the attachments of a message signed by the same
 * pubkey. The timeline additionally honours an agent owner's key, which it
 * resolves from loaded profiles; the index has no profile lookup, so it takes
 * the strictly narrower rule — an unauthorized edit never changes a row.
 */
function isAuthorizedFileEdit(source: IndexedSource, edit: IndexedEdit) {
  return source.pubkey.toLowerCase() === edit.pubkey.toLowerCase();
}

type IndexDraft = {
  sources: Map<string, IndexedSource>;
  deletions: Map<string, number>;
  edits: Map<string, IndexedEdit>;
  truncated: boolean;
  changed: boolean;
};

/** One retained entry reduced to what the retention order compares. */
type RetentionKey = { key: string; created_at: number };

/**
 * The total order the caps retain by: oldest first, map key breaking ties.
 *
 * It is the same order {@link selectIndexedFiles} sorts rows into, so the entry
 * a cap evicts is always the one that sorts off the old end of the list, and
 * the page a caller is looking at cannot shuffle because of an eviction.
 */
function isOlderRetained(left: RetentionKey, right: RetentionKey): boolean {
  if (left.created_at !== right.created_at) {
    return left.created_at < right.created_at;
  }
  return left.key < right.key;
}

/**
 * The oldest retained entry of a capped map — the retention floor — or `null`
 * when the map is empty.
 *
 * Linear in the map, and only ever walked when a cap is already full and the
 * arriving entry is not a duplicate. A backfill runs newest-first, so it fills
 * a cap once and then has every older page refused by the floor without a
 * scan; the scan is the live path's cost, one arrival at a time.
 */
function oldestRetained<V>(
  map: ReadonlyMap<string, V>,
  createdAtOf: (value: V) => number,
): RetentionKey | null {
  let oldest: RetentionKey | null = null;
  for (const [key, value] of map) {
    const candidate = { key, created_at: createdAtOf(value) };
    if (oldest === null || isOlderRetained(candidate, oldest)) {
      oldest = candidate;
    }
  }
  return oldest;
}

function markTruncated(draft: IndexDraft): void {
  if (draft.truncated) return;
  draft.truncated = true;
  draft.changed = true;
}

/**
 * Make room for one entry in a capped retention map, keeping the newest.
 *
 * A cap that only refused arrivals would freeze the index on whichever entries
 * it happened to see first: a channel past the cap would stop showing new
 * files entirely while the tab said it was showing the most recent ones. So
 * the cap evicts by recency instead — an entry newer than the floor takes the
 * floor's place, and only an entry older than the floor is refused, which is
 * exactly the entry a "keep the newest N" cap exists to leave out. `onEvict`
 * removes whatever else the evicted key owned.
 *
 * Either outcome means something is missing from the list, so both set
 * `truncated`; the caps still bound the entries retained, which is the memory
 * the index actually costs.
 *
 * Returns true when the caller may store the entry.
 */
function admitNewest<V>(
  map: Map<string, V>,
  createdAtOf: (value: V) => number,
  candidate: RetentionKey,
  cap: number,
  draft: IndexDraft,
  onEvict?: (evictedKey: string) => void,
): boolean {
  if (map.size < cap) return true;
  const floor = oldestRetained(map, createdAtOf);
  markTruncated(draft);
  if (floor === null || !isOlderRetained(floor, candidate)) return false;
  map.delete(floor.key);
  onEvict?.(floor.key);
  return true;
}

const editCreatedAt = (edit: IndexedEdit) => edit.created_at;
const sourceCreatedAt = (source: IndexedSource) => source.created_at;
const deletionCreatedAt = (createdAt: number) => createdAt;

/**
 * A relay-supplied timestamp, or `0` when it is unusable.
 *
 * Deletions order by this. A deletion the relay did not timestamp readably
 * sorts oldest, so it is the first evicted and the one refused at a full cap,
 * and it can never displace a deletion that carries a real timestamp.
 */
function boundEventTimestamp(value: unknown): number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0
    ? value
    : 0;
}

function ingestDeletion(event: RelayEvent, draft: IndexDraft): void {
  const createdAt = boundEventTimestamp(event.created_at);
  for (const targetId of referencedEventIds(event.tags)) {
    if (draft.deletions.has(targetId)) continue;
    if (
      !admitNewest(
        draft.deletions,
        deletionCreatedAt,
        { key: targetId, created_at: createdAt },
        MAX_INDEXED_DELETIONS,
        draft,
      )
    ) {
      continue;
    }
    draft.deletions.set(targetId, createdAt);
    draft.changed = true;
  }
}

function ingestEdit(event: RelayEvent, draft: IndexDraft): void {
  const targets = referencedEventIds(event.tags);
  const targetId = targets.at(-1);
  if (!targetId) return;
  const edit = boundIndexEdit(event);
  if (!edit) return;
  const target = draft.sources.get(targetId);
  // Refuse an edit signed by anyone but the message's author outright, so a
  // forged edit with a future timestamp cannot shadow the author's real one.
  if (target && !isAuthorizedFileEdit(target, edit)) return;
  const existing = draft.edits.get(targetId);
  if (existing && existing.created_at >= edit.created_at) return;
  if (
    !existing &&
    !admitNewest(
      draft.edits,
      editCreatedAt,
      { key: targetId, created_at: edit.created_at },
      MAX_INDEXED_EDITS,
      draft,
    )
  ) {
    return;
  }
  draft.edits.set(targetId, edit);
  draft.changed = true;
}

function ingestSource(event: RelayEvent, draft: IndexDraft): void {
  const source = boundIndexSource(event);
  if (!source || draft.sources.has(source.id)) return;
  if (
    !admitNewest(
      draft.sources,
      sourceCreatedAt,
      { key: source.id, created_at: source.created_at },
      MAX_INDEXED_ATTACHMENT_EVENTS,
      draft,
      (evictedId) => {
        // The evicted message's overlay entries describe a row that is gone.
        // Left behind they would be retention the caps no longer bound, and a
        // later message reusing the id would inherit a stranger's deletion.
        draft.edits.delete(evictedId);
        draft.deletions.delete(evictedId);
      },
    )
  ) {
    return;
  }
  draft.sources.set(source.id, source);
  // An edit may arrive before the message it edits, when authorship cannot be
  // checked yet. Now it can be: drop the ones that turn out to be forged.
  const pendingEdit = draft.edits.get(source.id);
  if (pendingEdit && !isAuthorizedFileEdit(source, pendingEdit)) {
    draft.edits.delete(source.id);
  }
  draft.changed = true;
}

/**
 * Fold relay events into the index, from either the live subscription or a
 * backfill page. Keyed by event id throughout, so the same event delivered by
 * both paths — the overlap that keeps the backfill from dropping an arrival —
 * is indexed once.
 *
 * Returns the same index object when nothing changed.
 */
export function ingestIndexEvents(
  index: FilesIndex,
  events: readonly RelayEvent[],
): FilesIndex {
  if (events.length === 0) return index;
  const draft: IndexDraft = {
    sources: new Map(index.sources),
    deletions: new Map(index.deletions),
    edits: new Map(index.edits),
    truncated: index.truncated,
    changed: false,
  };

  for (const event of events) {
    if (!event || typeof event.kind !== "number") continue;
    if (isDeletionKind(event.kind)) {
      ingestDeletion(event, draft);
    } else if (event.kind === KIND_STREAM_MESSAGE_EDIT) {
      ingestEdit(event, draft);
    } else {
      ingestSource(event, draft);
    }
  }

  if (!draft.changed) return index;
  return {
    sources: draft.sources,
    deletions: draft.deletions,
    edits: draft.edits,
    truncated: draft.truncated,
  };
}

const projections = new WeakMap<
  FilesIndex,
  { files: ChannelFile[]; truncated: boolean }
>();

/**
 * Project the index into the tab's rows, newest first, with deletions removed
 * and authorized edits overlaid. Memoised per index snapshot, which is why
 * {@link ingestIndexEvents} returns the identical object on a no-op.
 */
export function selectIndexedFiles(index: FilesIndex): {
  files: ChannelFile[];
  truncated: boolean;
} {
  const cached = projections.get(index);
  if (cached) return cached;

  const events: IndexedSource[] = [];
  for (const source of index.sources.values()) {
    if (index.deletions.has(source.id)) continue;
    const edit = index.edits.get(source.id);
    if (
      edit &&
      !index.deletions.has(edit.id) &&
      isAuthorizedFileEdit(source, edit)
    ) {
      events.push({
        ...source,
        // Same rule the renderer applies: the edit owns the whole imeta set.
        tags: applyEditTagOverlay(source.tags, edit.tags),
        content: edit.content,
      });
      continue;
    }
    events.push(source);
  }

  // Oldest first, because parseChannelFiles walks its input backwards to emit
  // newest first. Ties break on event id so the order is total and the same
  // on every projection — the page a caller sees cannot shuffle under it.
  events.sort(
    (left, right) =>
      left.created_at - right.created_at || (left.id < right.id ? -1 : 1),
  );

  const projected = parseChannelFiles(events);
  const result = {
    files: projected.files,
    truncated: projected.truncated || index.truncated,
  };
  projections.set(index, result);
  return result;
}
