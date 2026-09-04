import assert from "node:assert/strict";
import test from "node:test";

import {
  categorizeFile,
  parseChannelFiles,
  sortFiles,
} from "./useChannelFiles.ts";

function relayEvent(overrides = {}) {
  return {
    id: "event-id",
    pubkey: "pubkey",
    created_at: 1000,
    kind: 40002,
    tags: [],
    content: "",
    sig: "sig",
    ...overrides,
  };
}

function imetaTag(fields) {
  return [
    "imeta",
    ...Object.entries(fields).map(([key, value]) => `${key} ${value}`),
  ];
}

test("parseChannelFiles extracts a file from an imeta tag", () => {
  const event = relayEvent({
    id: "event-1",
    created_at: 100,
    pubkey: "alice",
    tags: [
      imetaTag({
        url: "https://relay.example/media/abc.png",
        m: "image/png",
        x: "deadbeef",
        size: "2048",
      }),
    ],
  });

  const files = parseChannelFiles([event]);

  assert.equal(files.length, 1);
  assert.equal(files[0].url, "https://relay.example/media/abc.png");
  assert.equal(files[0].mimeType, "image/png");
  assert.equal(files[0].sha256, "deadbeef");
  assert.equal(files[0].size, 2048);
  assert.equal(files[0].pubkey, "alice");
  assert.equal(files[0].eventId, "event-1");
});

test("parseChannelFiles labels a Markdown attachment by its imeta filename, not the raw .bin URL", () => {
  const event = relayEvent({
    id: "event-md",
    tags: [
      imetaTag({
        url: "https://relay.example/media/deadbeefdeadbeef.bin",
        m: "text/markdown",
        x: "deadbeefdeadbeef",
        filename: "notes.md",
      }),
    ],
  });

  const [file] = parseChannelFiles([event]);

  assert.equal(file.filename, "notes.md");
  assert.notEqual(file.filename, "deadbeefdeadbeef.bin");
  assert.equal(categorizeFile(file.mimeType), "document");
});

test("parseChannelFiles orders files newest-first regardless of input event order", () => {
  const older = relayEvent({
    id: "older",
    created_at: 100,
    tags: [
      imetaTag({
        url: "https://relay.example/media/older.png",
        m: "image/png",
      }),
    ],
  });
  const newer = relayEvent({
    id: "newer",
    created_at: 200,
    tags: [
      imetaTag({
        url: "https://relay.example/media/newer.png",
        m: "image/png",
      }),
    ],
  });

  // Input intentionally oldest-first — parseChannelFiles walks events
  // newest-first internally.
  const files = parseChannelFiles([older, newer]);

  assert.deepEqual(
    files.map((f) => f.eventId),
    ["newer", "older"],
  );
});

test("parseChannelFiles skips events with no imeta tags", () => {
  const plain = relayEvent({ id: "plain-text", tags: [] });
  const files = parseChannelFiles([plain]);
  assert.deepEqual(files, []);
});

test("parseChannelFiles extracts a caption from the message content's first line", () => {
  const event = relayEvent({
    id: "event-caption",
    content: "lunch photo\n![image](https://relay.example/media/abc.png)",
    tags: [
      imetaTag({
        url: "https://relay.example/media/abc.png",
        m: "image/png",
      }),
    ],
  });

  const [file] = parseChannelFiles([event]);
  assert.equal(file.caption, "lunch photo");
});

test("parseChannelFiles yields no caption when the first content line is only the markdown image token", () => {
  const event = relayEvent({
    id: "event-no-caption",
    content: "![image](https://relay.example/media/abc.png)",
    tags: [
      imetaTag({
        url: "https://relay.example/media/abc.png",
        m: "image/png",
      }),
    ],
  });

  const [file] = parseChannelFiles([event]);
  assert.equal(file.caption, undefined);
});

test("sortFiles orders newest-first by default and supports name/size/oldest", () => {
  const files = [
    { filename: "b.txt", size: 10, createdAt: 100 },
    { filename: "a.txt", size: 30, createdAt: 300 },
    { filename: "c.txt", size: 20, createdAt: 200 },
  ];

  assert.deepEqual(
    sortFiles(files, "newest").map((f) => f.filename),
    ["a.txt", "c.txt", "b.txt"],
  );
  assert.deepEqual(
    sortFiles(files, "oldest").map((f) => f.filename),
    ["b.txt", "c.txt", "a.txt"],
  );
  assert.deepEqual(
    sortFiles(files, "name").map((f) => f.filename),
    ["a.txt", "b.txt", "c.txt"],
  );
  assert.deepEqual(
    sortFiles(files, "size").map((f) => f.filename),
    ["a.txt", "c.txt", "b.txt"],
  );
});

test("categorizeFile maps mime types to categories", () => {
  assert.equal(categorizeFile("image/png"), "image");
  assert.equal(categorizeFile("video/mp4"), "video");
  assert.equal(categorizeFile("application/pdf"), "document");
  assert.equal(categorizeFile("text/markdown"), "document");
  assert.equal(categorizeFile("application/octet-stream"), "other");
});
