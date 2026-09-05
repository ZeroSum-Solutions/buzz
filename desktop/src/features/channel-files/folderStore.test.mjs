import assert from "node:assert/strict";
import test from "node:test";

import {
  MAX_ASSIGNMENTS,
  MAX_FOLDERS,
  MAX_FOLDER_DEPTH,
  MAX_FOLDER_NAME_LENGTH,
  MAX_PAYLOAD_BYTES,
  buildFileFolderMap,
  channelFolderDTag,
  emptySnapshot,
  fileKeyFor,
  flattenFolders,
  folderDepths,
  hasSiblingNamed,
  isFileKey,
  isFolderId,
  newFolderId,
  parseFolderPayload,
  parseSnapshot,
  serializeSnapshot,
  withFilesAssigned,
  withFolderCreated,
  withFolderDeleted,
  withFolderMoved,
  withFolderRenamed,
} from "./folderStore.ts";

const ID_A = "a".repeat(32);
const ID_B = "b".repeat(32);
const ID_C = "c".repeat(32);
const KEY_1 = `${"1".repeat(64)}:${"f".repeat(16)}`;
const KEY_2 = `${"2".repeat(64)}:${"e".repeat(16)}`;

function snapshot(folders, files = {}) {
  return { folders, files };
}

function unwrap(result) {
  assert.equal(
    result.ok,
    true,
    `expected ok, got ${result.error ?? result.reason}`,
  );
  return result.snapshot;
}

// ── identity ────────────────────────────────────────────────────────────

test("folder ids are random 16-byte hex, not derived from the name", () => {
  const first = newFolderId();
  const second = newFolderId();
  assert.match(first, /^[0-9a-f]{32}$/);
  assert.notEqual(first, second);
  assert.ok(isFolderId(first));
  assert.ok(!isFolderId("Foo"));
});

test("the d tag hashes the channel id instead of embedding it", () => {
  const channelId = "0f2b6a3e-private-channel";
  const dTag = channelFolderDTag(channelId);
  assert.match(dTag, /^files-v2-[0-9a-f]{32}$/);
  assert.ok(!dTag.includes(channelId));
  assert.equal(dTag, channelFolderDTag(channelId), "stable for a channel");
  assert.notEqual(dTag, channelFolderDTag(`${channelId}x`));
});

test("two attachments on one message get distinct file keys", () => {
  const eventId = "9".repeat(64);
  const first = fileKeyFor(eventId, "https://media/one.png");
  const second = fileKeyFor(eventId, "https://media/two.png");
  assert.notEqual(first, second);
  assert.ok(isFileKey(first));
  assert.ok(isFileKey(second));
  assert.ok(!isFileKey(eventId), "a bare event id is not a file key");
});

// ── bounded parsing ─────────────────────────────────────────────────────

test("a well-formed payload round trips through serialize and parse", () => {
  const source = snapshot(
    [
      { id: ID_A, name: "Design", parent: null },
      { id: ID_B, name: "Specs", parent: ID_A },
    ],
    { [KEY_1]: ID_B },
  );
  const parsed = unwrap(parseFolderPayload(serializeSnapshot(source)));
  assert.deepEqual(parsed, source);
  assert.deepEqual([...buildFileFolderMap(parsed)], [[KEY_1, ID_B]]);
});

test("the folder count bound rejects an over-limit payload instead of truncating", () => {
  const folders = Array.from({ length: MAX_FOLDERS + 1 }, (_, index) => ({
    id: index.toString(16).padStart(32, "0"),
    name: `f${index}`,
    parent: null,
  }));
  const result = parseSnapshot({ v: 1, folders, files: {} });
  assert.equal(result.ok, false);
  assert.equal(result.reason, "too-many-folders");
});

test("the assignment count bound rejects an over-limit payload", () => {
  const files = {};
  for (let index = 0; index <= MAX_ASSIGNMENTS; index += 1) {
    files[`${index.toString(16).padStart(64, "0")}:${"a".repeat(16)}`] = ID_A;
  }
  const result = parseSnapshot({
    v: 1,
    folders: [{ id: ID_A, name: "F", parent: null }],
    files,
  });
  assert.equal(result.ok, false);
  assert.equal(result.reason, "too-many-assignments");
});

test("the byte bound rejects an oversized payload before JSON.parse", () => {
  const oversized = `{"v":1,"folders":[],"files":{},"pad":"${"x".repeat(
    MAX_PAYLOAD_BYTES,
  )}"}`;
  const result = parseFolderPayload(oversized);
  assert.equal(result.ok, false);
  assert.equal(result.reason, "payload-too-large");
});

test("a cyclic parent chain is rejected, not walked", () => {
  const result = parseSnapshot({
    v: 1,
    folders: [
      { id: ID_A, name: "A", parent: ID_B },
      { id: ID_B, name: "B", parent: ID_A },
    ],
    files: {},
  });
  assert.equal(result.ok, false);
  assert.equal(result.reason, "cyclic-or-orphaned-folders");
});

test("an orphan parent reference is rejected", () => {
  const result = parseSnapshot({
    v: 1,
    folders: [{ id: ID_A, name: "A", parent: ID_C }],
    files: {},
  });
  assert.equal(result.ok, false);
  assert.equal(result.reason, "cyclic-or-orphaned-folders");
});

test("a payload nested past the depth bound is rejected", () => {
  const folders = [];
  for (let index = 0; index <= MAX_FOLDER_DEPTH; index += 1) {
    folders.push({
      id: index.toString(16).padStart(32, "0"),
      name: `d${index}`,
      parent: index === 0 ? null : (index - 1).toString(16).padStart(32, "0"),
    });
  }
  const result = parseSnapshot({ v: 1, folders, files: {} });
  assert.equal(result.ok, false);
  assert.equal(result.reason, "cyclic-or-orphaned-folders");
});

test("an over-long or empty folder name is rejected at parse", () => {
  for (const name of ["", "   ", "x".repeat(MAX_FOLDER_NAME_LENGTH + 1)]) {
    const result = parseSnapshot({
      v: 1,
      folders: [{ id: ID_A, name, parent: null }],
      files: {},
    });
    assert.equal(result.ok, false, `rejects ${JSON.stringify(name)}`);
    assert.equal(result.reason, "invalid-folder-name");
  }
});

test("an assignment naming an unknown folder is rejected", () => {
  const result = parseSnapshot({
    v: 1,
    folders: [{ id: ID_A, name: "A", parent: null }],
    files: { [KEY_1]: ID_B },
  });
  assert.equal(result.ok, false);
  assert.equal(result.reason, "assignment-to-unknown-folder");
});

test("a malformed file key is rejected", () => {
  const result = parseSnapshot({
    v: 1,
    folders: [{ id: ID_A, name: "A", parent: null }],
    files: { "not-a-file-key": ID_A },
  });
  assert.equal(result.ok, false);
  assert.equal(result.reason, "invalid-file-key");
});

test("invalid JSON and wrong versions are rejected", () => {
  assert.equal(parseFolderPayload("{oops").reason, "invalid-json");
  assert.equal(parseFolderPayload('{"v":2}').reason, "unsupported-version");
  assert.equal(parseSnapshot([]).reason, "not-an-object");
});

// ── transforms ──────────────────────────────────────────────────────────

test("create refuses a name past the length bound and an over-full channel", () => {
  const tooLong = withFolderCreated(emptySnapshot(), {
    id: ID_A,
    name: "x".repeat(MAX_FOLDER_NAME_LENGTH + 1),
    parent: null,
  });
  assert.equal(tooLong.ok, false);

  const full = snapshot(
    Array.from({ length: MAX_FOLDERS }, (_, index) => ({
      id: index.toString(16).padStart(32, "0"),
      name: `f${index}`,
      parent: null,
    })),
  );
  const overflow = withFolderCreated(full, {
    id: ID_A,
    name: "one more",
    parent: null,
  });
  assert.equal(overflow.ok, false);
  assert.match(overflow.error, /at most 200 folders/);
});

test("create refuses a missing parent and refuses to exceed the depth bound", () => {
  const orphan = withFolderCreated(emptySnapshot(), {
    id: ID_A,
    name: "Child",
    parent: ID_B,
  });
  assert.equal(orphan.ok, false);

  let current = emptySnapshot();
  let parent = null;
  for (let index = 0; index < MAX_FOLDER_DEPTH; index += 1) {
    const id = index.toString(16).padStart(32, "0");
    current = unwrap(
      withFolderCreated(current, { id, name: `d${index}`, parent }),
    );
    parent = id;
  }
  const tooDeep = withFolderCreated(current, {
    id: ID_A,
    name: "one deeper",
    parent,
  });
  assert.equal(tooDeep.ok, false);
  assert.match(tooDeep.error, /at most 8 deep/);
});

test("rename changes only the name, keeping id, parent and every assignment", () => {
  const before = snapshot(
    [
      { id: ID_A, name: "Old", parent: null },
      { id: ID_B, name: "Child", parent: ID_A },
    ],
    { [KEY_1]: ID_A, [KEY_2]: ID_B },
  );
  const after = unwrap(withFolderRenamed(before, ID_A, "  New  "));
  assert.equal(after.folders[0].name, "New");
  assert.equal(after.folders[0].id, ID_A);
  assert.equal(after.folders[1].parent, ID_A, "children keep pointing at it");
  assert.deepEqual(after.files, before.files);
  assert.equal(withFolderRenamed(before, ID_C, "Nope").ok, false);
  assert.equal(withFolderRenamed(before, ID_A, "   ").ok, false);
});

test("delete cascades in one snapshot: children reparent, their files stay filed", () => {
  const before = snapshot(
    [
      { id: ID_A, name: "Parent", parent: null },
      { id: ID_B, name: "Child", parent: ID_A },
    ],
    { [KEY_1]: ID_A, [KEY_2]: ID_B },
  );
  const after = unwrap(withFolderDeleted(before, ID_A));
  assert.deepEqual(after.folders, [{ id: ID_B, name: "Child", parent: null }]);
  assert.equal(after.files[KEY_2], ID_B, "the child's file stays assigned");
  assert.equal(
    KEY_1 in after.files,
    false,
    "the deleted folder's file unfiles",
  );
  assert.equal(
    parseSnapshot({ v: 1, ...after }).ok,
    true,
    "the post-delete snapshot is itself valid — no dangling parent",
  );
});

test("moving a folder under its own descendant is refused", () => {
  const before = snapshot([
    { id: ID_A, name: "A", parent: null },
    { id: ID_B, name: "B", parent: ID_A },
    { id: ID_C, name: "C", parent: ID_B },
  ]);
  assert.equal(withFolderMoved(before, ID_A, ID_C).ok, false);
  assert.equal(withFolderMoved(before, ID_A, ID_A).ok, false);
  assert.equal(withFolderMoved(before, ID_B, null).ok, true);
});

test("assigning a file moves it: exactly one owner, no add-without-remove", () => {
  const before = snapshot(
    [
      { id: ID_A, name: "A", parent: null },
      { id: ID_B, name: "B", parent: null },
    ],
    { [KEY_1]: ID_A },
  );
  const moved = unwrap(withFilesAssigned(before, [KEY_1], ID_B));
  assert.deepEqual(moved.files, { [KEY_1]: ID_B });
  const map = buildFileFolderMap(moved);
  assert.equal(map.size, 1, "no duplicate ownership after a move");

  const unfiled = unwrap(withFilesAssigned(moved, [KEY_1], null));
  assert.deepEqual(unfiled.files, {});
});

test("assignment refuses unknown folders, bad keys, empty selections and the count bound", () => {
  const base = snapshot([{ id: ID_A, name: "A", parent: null }]);
  assert.equal(withFilesAssigned(base, [KEY_1], ID_B).ok, false);
  assert.equal(withFilesAssigned(base, ["nope"], ID_A).ok, false);
  assert.equal(withFilesAssigned(base, [], ID_A).ok, false);

  const files = {};
  for (let index = 0; index < MAX_ASSIGNMENTS; index += 1) {
    files[`${index.toString(16).padStart(64, "0")}:${"a".repeat(16)}`] = ID_A;
  }
  const atCap = snapshot(base.folders, files);
  const overflow = withFilesAssigned(atCap, [KEY_1], ID_A);
  assert.equal(overflow.ok, false);
  assert.match(overflow.error, /at most 2000 filed files/);
});

test("a transform whose result would not fit the byte budget is refused", () => {
  const folders = [];
  const files = {};
  for (let index = 0; index < MAX_FOLDERS; index += 1) {
    const id = index.toString(16).padStart(32, "0");
    folders.push({
      id,
      name: "x".repeat(MAX_FOLDER_NAME_LENGTH),
      parent: null,
    });
  }
  for (let index = 0; index < 500; index += 1) {
    files[`${index.toString(16).padStart(64, "0")}:${"a".repeat(16)}`] =
      folders[0].id;
  }
  const big = snapshot(folders, files);
  assert.ok(
    serializeSnapshot(big).length > MAX_PAYLOAD_BYTES,
    "fixture is over the byte budget",
  );
  const result = withFolderRenamed(big, folders[0].id, "still too big");
  assert.equal(result.ok, false);
  assert.match(result.error, /too large to save/);
});

// ── rendering helpers ───────────────────────────────────────────────────

test("flattenFolders expands only expanded folders and terminates on a cycle", () => {
  const tree = snapshot([
    { id: ID_A, name: "A", parent: null },
    { id: ID_B, name: "B", parent: ID_A },
  ]);
  assert.deepEqual(
    flattenFolders(tree, new Set()).map((entry) => entry.folder.id),
    [ID_A],
  );
  assert.deepEqual(
    flattenFolders(tree, new Set([ID_A])).map((entry) => [
      entry.folder.id,
      entry.depth,
    ]),
    [
      [ID_A, 0],
      [ID_B, 1],
    ],
  );

  // Topology that validation would have rejected: a self-parented folder
  // reachable from the root. The walk must still stop.
  const cyclic = snapshot([
    { id: ID_A, name: "A", parent: null },
    { id: ID_B, name: "B", parent: ID_A },
    { id: ID_C, name: "C", parent: ID_B },
  ]);
  cyclic.folders[1].parent = ID_C;
  cyclic.folders.push({ id: ID_B, name: "dupe", parent: ID_A });
  const flat = flattenFolders(cyclic, new Set([ID_A, ID_B, ID_C]));
  assert.ok(flat.length <= MAX_FOLDERS);
  assert.equal(new Set(flat.map((e) => e.folder.id)).size, flat.length);
});

test("folderDepths reports null for cycles and depths otherwise", () => {
  assert.equal(
    folderDepths([
      { id: ID_A, name: "A", parent: ID_B },
      { id: ID_B, name: "B", parent: ID_A },
    ]),
    null,
  );
  const depths = folderDepths([
    { id: ID_A, name: "A", parent: null },
    { id: ID_B, name: "B", parent: ID_A },
  ]);
  assert.equal(depths.get(ID_A), 1);
  assert.equal(depths.get(ID_B), 2);
});

test("hasSiblingNamed is case-insensitive and scoped to one parent", () => {
  const tree = snapshot([
    { id: ID_A, name: "Design", parent: null },
    { id: ID_B, name: "Design", parent: ID_A },
  ]);
  assert.equal(hasSiblingNamed(tree, null, "design"), true);
  assert.equal(hasSiblingNamed(tree, null, "design", ID_A), false);
  assert.equal(hasSiblingNamed(tree, ID_A, "Specs"), false);
});

test("creating a folder whose id is already stored leaves the snapshot alone", () => {
  const stored = snapshot([{ id: ID_A, name: "Design", parent: null }], {
    [KEY_1]: ID_A,
  });

  const replayed = unwrap(
    withFolderCreated(stored, { id: ID_A, name: "Design", parent: null }),
  );

  assert.deepEqual(
    replayed.folders,
    [{ id: ID_A, name: "Design", parent: null }],
    "a replayed create must not put two folders under one id",
  );
  assert.deepEqual(replayed.files, { [KEY_1]: ID_A });
});
