import assert from "node:assert/strict";
import test from "node:test";

import { MAX_CHANNEL_FILES, parseChannelFiles } from "./useChannelFiles.ts";
import { classifyFile } from "./fileFacets.ts";
import { MAX_IMETA_TAGS_SCANNED } from "./boundedImeta.ts";

const SHA = "d".repeat(64);

/** parseChannelFiles reports truncation alongside the rows; most tests only
 * care about the rows. */
function parse(events) {
  return parseChannelFiles(events).files;
}

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
        x: SHA,
        size: "2048",
      }),
    ],
  });

  const files = parse([event]);

  assert.equal(files.length, 1);
  assert.equal(files[0].url, "https://relay.example/media/abc.png");
  assert.equal(files[0].mimeType, "image/png");
  assert.equal(files[0].sha256, SHA);
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
        x: SHA,
        filename: "notes.md",
      }),
    ],
  });

  const [file] = parse([event]);

  assert.equal(file.filename, "notes.md");
  assert.notEqual(file.filename, "deadbeefdeadbeef.bin");
  assert.equal(classifyFile(file), "document");
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
  const files = parse([older, newer]);

  assert.deepEqual(
    files.map((f) => f.eventId),
    ["newer", "older"],
  );
});

test("parseChannelFiles skips events with no imeta tags", () => {
  const plain = relayEvent({ id: "plain-text", tags: [] });
  const files = parse([plain]);
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

  const [file] = parse([event]);
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

  const [file] = parse([event]);
  assert.equal(file.caption, undefined);
});

test("parseChannelFiles caps an oversized relay-sourced filename and caption", () => {
  const event = relayEvent({
    id: "event-oversized-strings",
    content: "x".repeat(1000),
    tags: [
      imetaTag({
        url: "https://relay.example/media/abc.png",
        m: "image/png",
        filename: "n".repeat(1000),
      }),
    ],
  });

  const [file] = parse([event]);

  assert.ok(
    file.filename.length <= 300,
    `filename should be capped, got ${file.filename.length} chars`,
  );
  assert.ok(
    file.caption.length <= 500,
    `caption should be capped, got ${file.caption.length} chars`,
  );
});

test("parseChannelFiles caps an oversized relay-sourced mimeType", () => {
  const event = relayEvent({
    id: "event-oversized-mime",
    tags: [
      imetaTag({
        url: "https://relay.example/media/abc.png",
        m: "x".repeat(1000),
      }),
    ],
  });

  const [file] = parse([event]);

  assert.ok(
    file.mimeType.length <= 100,
    `mimeType should be capped, got ${file.mimeType.length} chars`,
  );
});

test("parseChannelFiles caps the number of attachments pulled from a single event", () => {
  const fields = [];
  for (let i = 0; i < 40; i++) {
    fields.push([
      "imeta",
      `url https://relay.example/media/file-${i}.png`,
      "m image/png",
    ]);
  }
  const event = relayEvent({ id: "event-many-attachments", tags: fields });

  const files = parse([event]);

  assert.ok(
    files.length <= 20,
    `expected the per-event attachment count to be capped, got ${files.length}`,
  );
});

test("parseChannelFiles gives each attachment on one message its own key", () => {
  const event = relayEvent({
    id: "e".repeat(64),
    tags: [
      imetaTag({ url: "https://relay.example/media/one.png", m: "image/png" }),
      imetaTag({ url: "https://relay.example/media/two.png", m: "image/png" }),
    ],
  });

  const files = parse([event]);

  assert.equal(files.length, 2);
  assert.notEqual(files[0].key, files[1].key, "selection must address one row");
  assert.equal(files[0].eventId, files[1].eventId);
});

test("parseChannelFiles rejects a non-http attachment URL", () => {
  const event = relayEvent({
    id: "event-bad-scheme",
    tags: [
      imetaTag({ url: "javascript:alert(1)", m: "text/html" }),
      imetaTag({ url: `https://relay.example/${"a".repeat(4000)}.png` }),
    ],
  });

  assert.deepEqual(parse([event]), []);
});

test("parseChannelFiles drops malformed hash, size and dim fields", () => {
  const event = relayEvent({
    id: "event-bad-fields",
    tags: [
      imetaTag({
        url: "https://relay.example/media/abc.png",
        m: "image/png",
        x: "not-a-hash",
        size: "99999999999999999999",
        dim: "x".repeat(500),
      }),
    ],
  });

  const [file] = parse([event]);

  assert.equal(file.sha256, undefined);
  assert.equal(file.size, undefined);
  assert.equal(file.dim, undefined);
});

test("parseChannelFiles caps the total row count and reports truncation", () => {
  const events = [];
  for (let i = 0; i < MAX_CHANNEL_FILES + 5; i++) {
    events.push(
      relayEvent({
        id: `event-${i}`,
        created_at: 1000 + i,
        tags: [
          imetaTag({
            url: `https://relay.example/media/f-${i}.png`,
            m: "image/png",
          }),
        ],
      }),
    );
  }

  const result = parseChannelFiles(events);

  assert.equal(result.files.length, MAX_CHANNEL_FILES);
  assert.equal(result.truncated, true, "a capped list must not read as whole");
  assert.equal(parseChannelFiles(events.slice(0, 3)).truncated, false);
});

test("parseChannelFiles bounds the tag scan by every tag, not only imeta tags", () => {
  const tags = [];
  for (let i = 0; i < 5_000; i++) tags.push(["p", `pubkey-${i}`]);
  tags.push(
    imetaTag({ url: "https://relay.example/media/late.png", m: "image/png" }),
  );

  let indexReads = 0;
  const counted = new Proxy(tags, {
    get(target, property, receiver) {
      if (typeof property === "string" && /^\d+$/.test(property)) {
        indexReads += 1;
      }
      return Reflect.get(target, property, receiver);
    },
  });

  const files = parse([relayEvent({ id: "tag-flood", tags: counted })]);

  assert.equal(
    files.length,
    0,
    "an imeta tag past the scan budget is never reached",
  );
  assert.ok(
    indexReads <= MAX_IMETA_TAGS_SCANNED,
    `the scan must stop at the budget; it read ${indexReads} tags`,
  );
});
