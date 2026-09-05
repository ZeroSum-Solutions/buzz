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
 * The retention caps evict rather than refuse ({@link admitRetained}): past a
 * cap the index gives up its least useful entry — one describing no retained
 * message first, the oldest otherwise — so live arrivals keep landing and a
 * deleted attachment never reappears because its deletion was dropped.
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
 * retained one (see {@link admitRetained}), so a channel with more attachments
 * than this still shows the recent ones the tab claims to show.
 */
export const MAX_INDEXED_ATTACHMENT_EVENTS = 5_000;
/**
 * Deletion targets retained. Deletions are ids only, but still unbounded input.
 * A deletion that hides a retained attachment is kept over one that hides
 * nothing; within a class, the newer over the older.
 */
export const MAX_INDEXED_DELETIONS = 5_000;
/**
 * Edited messages tracked. One entry per edited message, newest edit wins, and
 * the cap keeps the edits that can still change a retained row over the ones
 * that cannot.
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
 * The recency half of the retention order: oldest first, map key breaking ties.
 *
 * It is the same order {@link selectIndexedFiles} sorts rows into, so the
 * attachment a full source cap gives up is always the one that sorts off the
 * old end of the list, and the page a caller is reading cannot shuffle because
 * of an eviction.
 */
function isOlderRetained(left: RetentionKey, right: RetentionKey): boolean {
  if (left.created_at !== right.created_at) {
    return left.created_at < right.created_at;
  }
  return left.key < right.key;
}

/**
 * Whether a retained entry still describes a message the index holds.
 *
 * A deletion or edit keyed by a message the index never saw, or has already
 * evicted, changes nothing the tab can show, so it is the cap's first victim.
 * Sources are their own rows, so their cap uses {@link ALWAYS_RELEVANT}.
 */
type IsRelevant = (key: string) => boolean;

const ALWAYS_RELEVANT: IsRelevant = () => true;

/**
 * The eviction order: an entry that still describes a retained message
 * outranks one that does not, and inside a class the newer outranks the older.
 * `true` when a full cap should keep `candidate` over `victim`.
 */
function outranksForRetention(
  candidate: RetentionKey,
  victim: RetentionKey,
  isRelevant: IsRelevant,
): boolean {
  const candidateRelevant = isRelevant(candidate.key);
  if (candidateRelevant !== isRelevant(victim.key)) return candidateRelevant;
  return isOlderRetained(victim, candidate);
}

/**
 * The entry every other retained entry outranks — what a full cap gives up —
 * or `null` when the map is empty.
 *
 * Linear in the map, walked on every non-duplicate arrival once the cap is
 * full, refusals included: a refusal has to know what it is being compared
 * against. The history walk is bounded (`MAX_BACKFILL_PAGES` pages of
 * `BACKFILL_PAGE_LIMIT`), which bounds the total cost; an ordered index would
 * make it O(log n) and is a follow-up, not a correctness question.
 */
function lowestRanked<V>(
  map: ReadonlyMap<string, V>,
  createdAtOf: (value: V) => number,
  isRelevant: IsRelevant,
): RetentionKey | null {
  let lowest: RetentionKey | null = null;
  for (const [key, value] of map) {
    const entry = { key, created_at: createdAtOf(value) };
    if (lowest === null || outranksForRetention(lowest, entry, isRelevant)) {
      lowest = entry;
    }
  }
  return lowest;
}

function markTruncated(draft: IndexDraft): void {
  if (draft.truncated) return;
  draft.truncated = true;
  draft.changed = true;
}

/**
 * Make room for one entry in a capped retention map.
 *
 * A cap that only refused arrivals would freeze the index on whichever entries
 * it happened to see first: a channel past the cap would stop showing new
 * files entirely while the tab said it was showing the most recent ones. So
 * the cap gives up its lowest-ranked entry instead — an entry describing no
 * retained message before one that does, and the older before the newer — and
 * refuses the arrival only when the arrival is the lowest-ranked of all, which
 * is exactly what a bounded index exists to leave out. `onEvict` forgets
 * whatever else the evicted key stood for.
 *
 * Either outcome means something is missing, so both set `truncated`; the caps
 * still bound the entries retained, which is the memory the index costs.
 *
 * Returns true when the caller may store the entry.
 */
function admitRetained<V>(
  map: Map<string, V>,
  createdAtOf: (value: V) => number,
  candidate: RetentionKey,
  cap: number,
  draft: IndexDraft,
  isRelevant: IsRelevant,
  onEvict?: (evictedKey: string) => void,
): boolean {
  if (map.size < cap) return true;
  const victim = lowestRanked(map, createdAtOf, isRelevant);
  markTruncated(draft);
  if (victim === null || !outranksForRetention(candidate, victim, isRelevant)) {
    return false;
  }
  map.delete(victim.key);
  onEvict?.(victim.key);
  return true;
}

const editCreatedAt = (edit: IndexedEdit) => edit.created_at;
const sourceCreatedAt = (source: IndexedSource) => source.created_at;
const deletionCreatedAt = (createdAt: number) => createdAt;

/**
 * Forget a message and everything the index kept only to describe it.
 *
 * The projection honours a deletion keyed by the message id *and* one keyed by
 * the id of the edit that rewrote it, so both go. Anything left behind is a
 * capped slot spent on a row that is gone, and a later message reusing an id
 * would inherit a stranger's marker.
 */
function forgetSource(draft: IndexDraft, sourceId: string): void {
  const edit = draft.edits.get(sourceId);
  draft.sources.delete(sourceId);
  draft.edits.delete(sourceId);
  draft.deletions.delete(sourceId);
  if (edit) draft.deletions.delete(edit.id);
}

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
  // A deletion entry is the only thing hiding a deleted row. Giving one up
  // while the index still holds its message would put the file back on screen
  // — the opposite of what its author asked for, and a change no banner
  // announces. So a deletion covering a retained source outranks one covering
  // nothing, whatever their timestamps, and the orphans go first. When only
  // covering deletions are left, `forgetSource` takes the message away with
  // its deletion, so the pair is dropped rather than the file revealed.
  const coversRetainedSource: IsRelevant = (key) => draft.sources.has(key);
  for (const targetId of referencedEventIds(event.tags)) {
    if (draft.deletions.has(targetId)) continue;
    if (
      !admitRetained(
        draft.deletions,
        deletionCreatedAt,
        { key: targetId, created_at: createdAt },
        MAX_INDEXED_DELETIONS,
        draft,
        coversRetainedSource,
        (evictedKey) => {
          // Reached only when no orphan deletion was left to give up. An
          // orphan eviction frees a slot and hides nothing, so it cascades to
          // nothing either.
          if (draft.sources.has(evictedKey)) forgetSource(draft, evictedKey);
        },
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
  // An edit for a message the index holds can change a row the tab shows; one
  // for a message it never saw, or has already evicted, can change nothing. So
  // the cap bounds the edits that can matter and drops the rest first —
  // otherwise a burst of edits for unrelated messages would refuse a visible
  // row's genuine edit for good.
  if (
    !existing &&
    !admitRetained(
      draft.edits,
      editCreatedAt,
      { key: targetId, created_at: edit.created_at },
      MAX_INDEXED_EDITS,
      draft,
      (key) => draft.sources.has(key),
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
    !admitRetained(
      draft.sources,
      sourceCreatedAt,
      { key: source.id, created_at: source.created_at },
      MAX_INDEXED_ATTACHMENT_EVENTS,
      draft,
      // Every source is its own row, so recency alone ranks this cap.
      ALWAYS_RELEVANT,
      (evictedId) => forgetSource(draft, evictedId),
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
