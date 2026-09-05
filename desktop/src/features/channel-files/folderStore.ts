import { sha256 } from "@noble/hashes/sha2.js";
import { bytesToHex } from "@noble/hashes/utils.js";

/**
 * Pure snapshot model for channel file folders (folders v2).
 *
 * One encrypted kind:30078 event per (user, channel) holds the whole folder
 * tree and every file assignment, so a user action is exactly one write and
 * every intermediate state a reader can observe is a consistent one. Nothing
 * in this module touches the relay, react-query, or the DOM — the hook layer
 * owns that — which is what lets every bound and every transform below be
 * bound by a unit test.
 */

/** Folders per channel. Bounds the tree the UI walks and renders. */
export const MAX_FOLDERS = 200;
/** File→folder assignments per channel. Bounds the map the UI builds. */
export const MAX_ASSIGNMENTS = 2_000;
/** Display-name length, measured after trimming. */
export const MAX_FOLDER_NAME_LENGTH = 80;
/** Nesting depth, root folders being depth 1. */
export const MAX_FOLDER_DEPTH = 8;
/**
 * Plaintext payload budget, checked before JSON.parse on read and before
 * signing on write. This is the byte bound that actually costs: every other
 * count below is only reachable inside a payload that already fits here.
 */
export const MAX_PAYLOAD_BYTES = 64 * 1_024;

const FOLDER_ID_PATTERN = /^[0-9a-f]{32}$/;
const FILE_KEY_PATTERN = /^[A-Za-z0-9_-]{1,64}:[0-9a-f]{16}$/;

export type FolderNode = {
  /** Immutable 16-byte hex id. Never derived from the name. */
  id: string;
  name: string;
  parent: string | null;
};

export type FolderSnapshot = {
  folders: FolderNode[];
  /** fileKey → folder id. Absent key means "unfiled". */
  files: Record<string, string>;
};

export type ParseResult =
  | { ok: true; snapshot: FolderSnapshot }
  | { ok: false; reason: string };

export type TransformResult =
  | { ok: true; snapshot: FolderSnapshot }
  | { ok: false; error: string };

export function emptySnapshot(): FolderSnapshot {
  return { folders: [], files: {} };
}

function hex(value: string): string {
  return bytesToHex(sha256(new TextEncoder().encode(value)));
}

/** 16 random bytes as hex — a folder id that no rename can ever change. */
export function newFolderId(): string {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  return bytesToHex(bytes);
}

export function isFolderId(value: unknown): value is string {
  return typeof value === "string" && FOLDER_ID_PATTERN.test(value);
}

export function isFileKey(value: unknown): value is string {
  return typeof value === "string" && FILE_KEY_PATTERN.test(value);
}

/**
 * The `d` tag for a channel's folder event. The channel id is hashed, never
 * embedded: kind:30078 is relay-global, so a raw channel id in the tag would
 * publish private-channel membership to anyone who can read the author's
 * events.
 */
export function channelFolderDTag(channelId: string): string {
  return `files-v2-${hex(channelId).slice(0, 32)}`;
}

/**
 * Stable identity for one attachment: the carrying message plus a digest of
 * the attachment URL, so two attachments on one message are two file keys.
 */
export function fileKeyFor(eventId: string, url: string): string {
  return `${eventId.slice(0, 64)}:${hex(url).slice(0, 16)}`;
}

export function normalizeFolderName(name: string): string | null {
  const trimmed = name.trim();
  if (trimmed.length === 0 || trimmed.length > MAX_FOLDER_NAME_LENGTH) {
    return null;
  }
  return trimmed;
}

export function byteLength(value: string): number {
  return new TextEncoder().encode(value).length;
}

export function serializeSnapshot(snapshot: FolderSnapshot): string {
  return JSON.stringify({
    v: 1,
    folders: snapshot.folders,
    files: snapshot.files,
  });
}

function isPlainRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * Depth of every folder, or `null` when the parent graph is cyclic, points at
 * a missing folder, or nests deeper than {@link MAX_FOLDER_DEPTH}. Iterative
 * and visited-bounded: relay-sourced topology is never trusted to terminate.
 */
export function folderDepths(
  folders: FolderNode[],
): Map<string, number> | null {
  const byId = new Map(folders.map((folder) => [folder.id, folder]));
  const depths = new Map<string, number>();

  for (const folder of folders) {
    if (depths.has(folder.id)) continue;
    const chain: string[] = [];
    const seen = new Set<string>();
    let current: FolderNode | undefined = folder;
    let baseDepth = 0;

    while (current) {
      if (seen.has(current.id)) return null;
      const known = depths.get(current.id);
      if (known !== undefined) {
        baseDepth = known;
        break;
      }
      seen.add(current.id);
      chain.push(current.id);
      if (current.parent === null) break;
      const parent: FolderNode | undefined = byId.get(current.parent);
      if (!parent) return null;
      current = parent;
    }

    for (let index = chain.length - 1; index >= 0; index -= 1) {
      baseDepth += 1;
      if (baseDepth > MAX_FOLDER_DEPTH) return null;
      depths.set(chain[index], baseDepth);
    }
  }

  return depths;
}

/**
 * Validate a decrypted folder payload in one bounded pass. Anything past a
 * bound, malformed, or cyclic is rejected outright — the caller renders an
 * error and disables mutations rather than treating a partial read as the
 * user's real folder tree.
 */
export function parseSnapshot(value: unknown): ParseResult {
  if (!isPlainRecord(value)) return { ok: false, reason: "not-an-object" };
  if (value.v !== 1) return { ok: false, reason: "unsupported-version" };
  if (!Array.isArray(value.folders)) {
    return { ok: false, reason: "folders-not-an-array" };
  }
  if (value.folders.length > MAX_FOLDERS) {
    return { ok: false, reason: "too-many-folders" };
  }

  const folders: FolderNode[] = [];
  const ids = new Set<string>();
  for (const raw of value.folders) {
    if (!isPlainRecord(raw)) return { ok: false, reason: "malformed-folder" };
    if (!isFolderId(raw.id)) return { ok: false, reason: "invalid-folder-id" };
    if (ids.has(raw.id)) return { ok: false, reason: "duplicate-folder-id" };
    if (typeof raw.name !== "string") {
      return { ok: false, reason: "invalid-folder-name" };
    }
    const name = normalizeFolderName(raw.name);
    if (name === null) return { ok: false, reason: "invalid-folder-name" };
    const parent = raw.parent ?? null;
    if (parent !== null && !isFolderId(parent)) {
      return { ok: false, reason: "invalid-folder-parent" };
    }
    ids.add(raw.id);
    folders.push({ id: raw.id, name, parent });
  }

  if (folderDepths(folders) === null) {
    return { ok: false, reason: "cyclic-or-orphaned-folders" };
  }

  if (!isPlainRecord(value.files)) {
    return { ok: false, reason: "files-not-an-object" };
  }
  const files: Record<string, string> = {};
  let assignments = 0;
  for (const [key, folderId] of Object.entries(value.files)) {
    assignments += 1;
    if (assignments > MAX_ASSIGNMENTS) {
      return { ok: false, reason: "too-many-assignments" };
    }
    if (!isFileKey(key)) return { ok: false, reason: "invalid-file-key" };
    if (!isFolderId(folderId) || !ids.has(folderId)) {
      return { ok: false, reason: "assignment-to-unknown-folder" };
    }
    files[key] = folderId;
  }

  return { ok: true, snapshot: { folders, files } };
}

/**
 * Decrypted-payload entry point: byte bound first, then JSON, then structure.
 * The byte check runs before `JSON.parse` so an oversized payload is never
 * materialised as objects.
 */
export function parseFolderPayload(plaintext: string): ParseResult {
  if (byteLength(plaintext) > MAX_PAYLOAD_BYTES) {
    return { ok: false, reason: "payload-too-large" };
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(plaintext);
  } catch {
    return { ok: false, reason: "invalid-json" };
  }
  return parseSnapshot(parsed);
}

/** Guard every write shares: the signed payload must fit the byte budget. */
export function checkPayloadFits(snapshot: FolderSnapshot): TransformResult {
  if (byteLength(serializeSnapshot(snapshot)) > MAX_PAYLOAD_BYTES) {
    return {
      ok: false,
      error: "Folder data is too large to save. Delete some folders first.",
    };
  }
  return { ok: true, snapshot };
}

function assignmentCount(snapshot: FolderSnapshot): number {
  return Object.keys(snapshot.files).length;
}

export function withFolderCreated(
  snapshot: FolderSnapshot,
  folder: { id: string; name: string; parent: string | null },
): TransformResult {
  const name = normalizeFolderName(folder.name);
  if (name === null) {
    return {
      ok: false,
      error: `Folder names must be 1–${MAX_FOLDER_NAME_LENGTH} characters.`,
    };
  }
  if (!isFolderId(folder.id)) {
    return { ok: false, error: "Invalid folder id." };
  }
  if (snapshot.folders.some((existing) => existing.id === folder.id)) {
    // Replaying a create whose write already landed (another device wrote on
    // top of it before our acknowledgement arrived). Appending again would put
    // two folders under one id; the stored folder is already the result.
    return { ok: true, snapshot };
  }
  if (snapshot.folders.length >= MAX_FOLDERS) {
    return {
      ok: false,
      error: `A channel can hold at most ${MAX_FOLDERS} folders.`,
    };
  }
  if (
    folder.parent !== null &&
    !snapshot.folders.some((existing) => existing.id === folder.parent)
  ) {
    return { ok: false, error: "The parent folder no longer exists." };
  }
  const next: FolderSnapshot = {
    folders: [
      ...snapshot.folders,
      { id: folder.id, name, parent: folder.parent },
    ],
    files: snapshot.files,
  };
  if (folderDepths(next.folders) === null) {
    return {
      ok: false,
      error: `Folders can nest at most ${MAX_FOLDER_DEPTH} deep.`,
    };
  }
  return checkPayloadFits(next);
}

export function withFolderRenamed(
  snapshot: FolderSnapshot,
  id: string,
  newName: string,
): TransformResult {
  const name = normalizeFolderName(newName);
  if (name === null) {
    return {
      ok: false,
      error: `Folder names must be 1–${MAX_FOLDER_NAME_LENGTH} characters.`,
    };
  }
  if (!snapshot.folders.some((folder) => folder.id === id)) {
    return { ok: false, error: "That folder no longer exists." };
  }
  return checkPayloadFits({
    folders: snapshot.folders.map((folder) =>
      folder.id === id ? { ...folder, name } : folder,
    ),
    files: snapshot.files,
  });
}

/**
 * Delete one folder in a single write: its children reparent to its own
 * parent (keeping their files assigned) and files filed directly in it become
 * unfiled. No prefix of this operation is observable, so no subtree can be
 * stranded by a half-applied delete.
 */
export function withFolderDeleted(
  snapshot: FolderSnapshot,
  id: string,
): TransformResult {
  const target = snapshot.folders.find((folder) => folder.id === id);
  if (!target) return { ok: false, error: "That folder no longer exists." };

  const files: Record<string, string> = {};
  for (const [key, folderId] of Object.entries(snapshot.files)) {
    if (folderId !== id) files[key] = folderId;
  }

  return checkPayloadFits({
    folders: snapshot.folders
      .filter((folder) => folder.id !== id)
      .map((folder) =>
        folder.parent === id ? { ...folder, parent: target.parent } : folder,
      ),
    files,
  });
}

export function withFolderMoved(
  snapshot: FolderSnapshot,
  id: string,
  parent: string | null,
): TransformResult {
  if (!snapshot.folders.some((folder) => folder.id === id)) {
    return { ok: false, error: "That folder no longer exists." };
  }
  if (parent !== null && !snapshot.folders.some((f) => f.id === parent)) {
    return { ok: false, error: "The destination folder no longer exists." };
  }
  if (parent === id) {
    return { ok: false, error: "A folder cannot contain itself." };
  }
  const next: FolderSnapshot = {
    folders: snapshot.folders.map((folder) =>
      folder.id === id ? { ...folder, parent } : folder,
    ),
    files: snapshot.files,
  };
  const depths = folderDepths(next.folders);
  if (depths === null) {
    return {
      ok: false,
      error: `That move would nest folders more than ${MAX_FOLDER_DEPTH} deep or inside themselves.`,
    };
  }
  return checkPayloadFits(next);
}

/**
 * Assign (or, with `folderId === null`, unfile) a set of files in one write.
 * A move is therefore never "add to the target, then remove from the source":
 * there is exactly one owning folder per file key at every instant.
 */
export function withFilesAssigned(
  snapshot: FolderSnapshot,
  fileKeys: string[],
  folderId: string | null,
): TransformResult {
  if (fileKeys.length === 0) {
    return { ok: false, error: "No files were selected." };
  }
  for (const key of fileKeys) {
    if (!isFileKey(key)) return { ok: false, error: "Invalid file reference." };
  }
  if (
    folderId !== null &&
    !snapshot.folders.some((folder) => folder.id === folderId)
  ) {
    return { ok: false, error: "That folder no longer exists." };
  }

  const files = { ...snapshot.files };
  for (const key of fileKeys) {
    if (folderId === null) delete files[key];
    else files[key] = folderId;
  }
  const next: FolderSnapshot = { folders: snapshot.folders, files };
  if (assignmentCount(next) > MAX_ASSIGNMENTS) {
    return {
      ok: false,
      error: `A channel can hold at most ${MAX_ASSIGNMENTS} filed files.`,
    };
  }
  return checkPayloadFits(next);
}

/** fileKey → owning folder id. Exactly one owner per key, by construction. */
export function buildFileFolderMap(
  snapshot: FolderSnapshot,
): Map<string, string> {
  return new Map(Object.entries(snapshot.files));
}

/** True when a sibling of `parent` already carries `name` (UI-only policy). */
export function hasSiblingNamed(
  snapshot: FolderSnapshot,
  parent: string | null,
  name: string,
  exceptId?: string,
): boolean {
  const target = name.trim().toLowerCase();
  return snapshot.folders.some(
    (folder) =>
      folder.id !== exceptId &&
      folder.parent === parent &&
      folder.name.toLowerCase() === target,
  );
}

export type FlatFolder = { folder: FolderNode; depth: number };

/**
 * Depth-annotated render order for the folder tree, expanding only the ids in
 * `expanded`. Iterative with a visited set and a node budget, so a snapshot
 * that slipped past validation still cannot spin the renderer.
 */
export function flattenFolders(
  snapshot: FolderSnapshot,
  expanded: ReadonlySet<string>,
): FlatFolder[] {
  const children = new Map<string | null, FolderNode[]>();
  for (const folder of snapshot.folders) {
    const list = children.get(folder.parent) ?? [];
    list.push(folder);
    children.set(folder.parent, list);
  }

  const result: FlatFolder[] = [];
  const visited = new Set<string>();
  const stack: FlatFolder[] = [...(children.get(null) ?? [])]
    .reverse()
    .map((folder) => ({ folder, depth: 0 }));

  while (stack.length > 0 && result.length < MAX_FOLDERS) {
    const entry = stack.pop();
    if (!entry) break;
    if (visited.has(entry.folder.id)) continue;
    visited.add(entry.folder.id);
    result.push(entry);
    if (!expanded.has(entry.folder.id)) continue;
    if (entry.depth + 1 >= MAX_FOLDER_DEPTH) continue;
    const kids = children.get(entry.folder.id) ?? [];
    for (let index = kids.length - 1; index >= 0; index -= 1) {
      stack.push({ folder: kids[index], depth: entry.depth + 1 });
    }
  }

  return result;
}
