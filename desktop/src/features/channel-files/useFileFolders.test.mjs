import assert from "node:assert/strict";
import test from "node:test";

import {
  buildCreateFolderTags,
  buildFileFolderMap,
  buildRenameFolderTags,
  folderDTag,
  folderSlug,
  parseFolder,
  withFileAddedToFolder,
  withFileRemovedFromFolder,
  withFilesAddedToFolder,
  withFolderParent,
} from "./useFileFolders.ts";

const CHANNEL_ID = "chan-1";

function folderEvent(tags, overrides = {}) {
  return {
    id: "folder-event-id",
    pubkey: "owner-pubkey",
    created_at: 1000,
    kind: 30078,
    tags,
    content: "",
    sig: "sig",
    ...overrides,
  };
}

test("folder assignment round trip: create -> parse -> add file -> parse -> remove file -> parse", () => {
  // create
  const createTags = buildCreateFolderTags(CHANNEL_ID, "Design Docs");
  const created = parseFolder(folderEvent(createTags, { id: "e-create" }));
  assert.ok(created, "created event parses back into a folder");
  assert.equal(created.name, "Design Docs");
  assert.equal(created.dTag, folderDTag(CHANNEL_ID, folderSlug("Design Docs")));
  assert.deepEqual(created.fileEventIds, []);

  // add a file
  const addedTags = withFileAddedToFolder(created, "file-event-1");
  const withFile = parseFolder(folderEvent(addedTags, { id: "e-add" }));
  assert.ok(withFile);
  assert.deepEqual(withFile.fileEventIds, ["file-event-1"]);
  // name/dTag survive the round trip unchanged
  assert.equal(withFile.dTag, created.dTag);
  assert.equal(withFile.name, created.name);

  // adding the same file again does not duplicate the tag
  const reAddedTags = withFileAddedToFolder(withFile, "file-event-1");
  const reAdded = parseFolder(folderEvent(reAddedTags, { id: "e-readd" }));
  assert.deepEqual(reAdded.fileEventIds, ["file-event-1"]);

  // remove the file
  const removedTags = withFileRemovedFromFolder(withFile, "file-event-1");
  const withoutFile = parseFolder(folderEvent(removedTags, { id: "e-remove" }));
  assert.ok(withoutFile);
  assert.deepEqual(withoutFile.fileEventIds, []);
});

test("withFilesAddedToFolder merges new ids and returns null when everything is already present", () => {
  const base = parseFolder(
    folderEvent(buildCreateFolderTags(CHANNEL_ID, "Screenshots"), {
      id: "e-base",
    }),
  );
  const withOne = parseFolder(
    folderEvent(withFileAddedToFolder(base, "a"), { id: "e-one" }),
  );

  const mergedTags = withFilesAddedToFolder(withOne, ["a", "b", "c"]);
  assert.ok(mergedTags, "adds the two new ids");
  const merged = parseFolder(folderEvent(mergedTags, { id: "e-merged" }));
  assert.deepEqual(merged.fileEventIds.sort(), ["a", "b", "c"]);

  // every id already present -> no-op signal
  assert.equal(withFilesAddedToFolder(merged, ["a", "b"]), null);
});

test("buildRenameFolderTags changes the d-tag with the name and reports the change", () => {
  const original = parseFolder(
    folderEvent(buildCreateFolderTags(CHANNEL_ID, "Old Name"), {
      id: "e-orig",
    }),
  );
  const withFile = parseFolder(
    folderEvent(withFileAddedToFolder(original, "file-1"), { id: "e-file" }),
  );

  const { tags, newDTag, dTagChanged } = buildRenameFolderTags(
    withFile,
    CHANNEL_ID,
    "New Name",
  );
  assert.equal(dTagChanged, true);
  assert.equal(newDTag, folderDTag(CHANNEL_ID, folderSlug("New Name")));

  const renamed = parseFolder(folderEvent(tags, { id: "e-renamed" }));
  assert.equal(renamed.name, "New Name");
  assert.equal(renamed.dTag, newDTag);
  // file refs survive the rename
  assert.deepEqual(renamed.fileEventIds, ["file-1"]);

  // renaming to the same name does not change the d-tag
  const sameName = buildRenameFolderTags(renamed, CHANNEL_ID, "New Name");
  assert.equal(sameName.dTagChanged, false);
});

test("withFolderParent sets and clears the parent d-tag", () => {
  const folder = parseFolder(
    folderEvent(buildCreateFolderTags(CHANNEL_ID, "Nested"), { id: "e-n" }),
  );

  const nested = parseFolder(
    folderEvent(withFolderParent(folder, "parent-dtag"), { id: "e-nested" }),
  );
  assert.equal(nested.parentDTag, "parent-dtag");

  const unnested = parseFolder(
    folderEvent(withFolderParent(nested, undefined), { id: "e-unnested" }),
  );
  assert.equal(unnested.parentDTag, undefined);
});

test("parseFolder rejects events without the file-folder type tag", () => {
  const event = folderEvent([
    ["d", "files-chan-1:notes"],
    ["name", "Notes"],
  ]);
  assert.equal(parseFolder(event), null);
});

test("buildFileFolderMap groups every file id under its folder's d-tag", () => {
  const a = parseFolder(
    folderEvent(buildCreateFolderTags(CHANNEL_ID, "A"), { id: "e-a" }),
  );
  const aWithFiles = parseFolder(
    folderEvent(withFilesAddedToFolder(a, ["f1", "f2"]) ?? [], {
      id: "e-a-files",
    }),
  );
  const b = parseFolder(
    folderEvent(buildCreateFolderTags(CHANNEL_ID, "B"), { id: "e-b" }),
  );
  const bWithFiles = parseFolder(
    folderEvent(withFileAddedToFolder(b, "f3"), { id: "e-b-files" }),
  );

  const map = buildFileFolderMap([aWithFiles, bWithFiles]);
  assert.equal(map.get("f1"), aWithFiles.dTag);
  assert.equal(map.get("f2"), aWithFiles.dTag);
  assert.equal(map.get("f3"), bWithFiles.dTag);
  assert.equal(map.size, 3);
});
