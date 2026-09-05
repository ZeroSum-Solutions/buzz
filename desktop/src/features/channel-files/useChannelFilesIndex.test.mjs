import assert from "node:assert/strict";
import test from "node:test";

import {
  MAX_INDEXED_ATTACHMENT_EVENTS,
  MAX_INDEXED_DELETIONS,
  MAX_INDEXED_EDITS,
  boundIndexSource,
  emptyFilesIndex,
  ingestIndexEvents,
  selectIndexedFiles,
} from "./channelFilesIndex.ts";
import {
  BACKFILL_PAGE_LIMIT,
  MAX_BACKFILL_PAGES,
  MAX_ERROR_DETAIL_LENGTH,
  createChannelFilesIndexController,
} from "./channelFilesBackfill.ts";
import {
  MAX_BULK_DROP_FILES,
  parseBulkDragDropEnabled,
  resolveBulkDropKeys,
} from "./bulkDropPreference.ts";
import { MAX_FILENAME_LENGTH } from "./boundedImeta.ts";

const SHA = "d".repeat(64);

function hexId(seed) {
  return seed.toString(16).padStart(64, "0");
}

function imetaTag(fields) {
  return [
    "imeta",
    ...Object.entries(fields).map(([key, value]) => `${key} ${value}`),
  ];
}

function fileEvent(overrides = {}) {
  const index = overrides.index ?? 1;
  return {
    id: overrides.id ?? hexId(index),
    pubkey: overrides.pubkey ?? "alice",
    created_at: overrides.created_at ?? 1_000 + index,
    kind: overrides.kind ?? 40002,
    content: overrides.content ?? `attachment ${index}`,
    sig: "sig",
    tags: overrides.tags ?? [
      ["h", "channel-1"],
      imetaTag({
        url: `https://relay.example/media/file-${index}.png`,
        m: "image/png",
        x: SHA,
        size: "2048",
        filename: `file-${index}.png`,
      }),
    ],
  };
}

function deletionEvent(targetId, overrides = {}) {
  return {
    id: overrides.id ?? hexId(900_000),
    pubkey: overrides.pubkey ?? "alice",
    created_at: overrides.created_at ?? 9_000,
    kind: overrides.kind ?? 5,
    content: "",
    sig: "sig",
    tags: [["e", targetId]],
  };
}

function editEvent(targetId, overrides = {}) {
  return {
    id: overrides.id ?? hexId(800_000),
    pubkey: overrides.pubkey ?? "alice",
    created_at: overrides.created_at ?? 8_000,
    kind: 40003,
    content: overrides.content ?? "edited",
    sig: "sig",
    tags: [
      ["e", targetId],
      ...(overrides.imeta ?? [
        imetaTag({
          url: "https://relay.example/media/edited.png",
          m: "image/png",
          filename: "edited.png",
        }),
      ]),
    ],
  };
}

/** A controller over scripted history pages and a controllable live channel. */
function harness({ pages = [], subscribeFails = false } = {}) {
  const calls = [];
  const requests = [];
  let deliver = null;
  let disposed = false;
  let pageIndex = 0;
  const snapshots = [];

  const controller = createChannelFilesIndexController({
    channelId: "channel-1",
    subscribeLive: async (channelId, onEvent) => {
      calls.push("subscribe");
      if (subscribeFails) throw new Error("relay socket refused");
      deliver = onEvent;
      return () => {
        disposed = true;
      };
    },
    fetchPage: async (request) => {
      calls.push("fetch");
      requests.push(request);
      const page = pages[pageIndex];
      pageIndex += 1;
      if (page instanceof Error) throw page;
      if (typeof page === "function") return page(request);
      return page ?? [];
    },
    onChange: (snapshot) => snapshots.push(snapshot),
  });

  return {
    calls,
    controller,
    requests,
    snapshots,
    deliver: (event) => deliver?.(event),
    isSubscriptionDisposed: () => disposed,
  };
}

function fileKeys(controller) {
  return selectIndexedFiles(controller.snapshot().index).files.map(
    (file) => file.key,
  );
}

// ---------------------------------------------------------------------------
// The six cases the ticket names.
// ---------------------------------------------------------------------------

test("more than 250 entries paginate without loss", async () => {
  const first = Array.from({ length: BACKFILL_PAGE_LIMIT }, (_unused, i) =>
    fileEvent({ index: 1_000 - i }),
  );
  const second = Array.from({ length: BACKFILL_PAGE_LIMIT }, (_unused, i) =>
    fileEvent({ index: 900 - i }),
  );
  const third = Array.from({ length: 60 }, (_unused, i) =>
    fileEvent({ index: 800 - i }),
  );
  const h = harness({ pages: [first, second, third] });

  await h.controller.start();

  const keys = fileKeys(h.controller);
  assert.equal(keys.length, 260);
  assert.equal(new Set(keys).size, 260, "no duplicate rows");
  assert.equal(h.controller.snapshot().complete, true);
  // Deterministic keyset paging: every page after the first asks for history
  // strictly older than the oldest entry the previous page returned.
  assert.equal(h.requests[1].until, 1_000 + 901);
  assert.equal(h.requests[1].beforeId, hexId(901));
  assert.equal(h.requests[2].until, 1_000 + 801);
  // Newest first, and nothing dropped at a page boundary.
  const files = selectIndexedFiles(h.controller.snapshot().index).files;
  assert.equal(files[0].filename, "file-1000.png");
  assert.equal(files.at(-1).filename, "file-741.png");
});

test("a reply attachment appears", async () => {
  const reply = fileEvent({ index: 7 });
  reply.tags = [
    ["h", "channel-1"],
    ["e", hexId(6), "", "root"],
    ["e", hexId(6), "", "reply"],
    imetaTag({
      url: "https://relay.example/media/reply.png",
      m: "image/png",
      filename: "reply.png",
    }),
  ];
  const h = harness({ pages: [[fileEvent({ index: 6 }), reply]] });

  await h.controller.start();

  const files = selectIndexedFiles(h.controller.snapshot().index).files;
  assert.deepEqual(
    files.map((file) => file.filename),
    ["reply.png", "file-6.png"],
  );
});

test("an arrival during backfill appears once", async () => {
  const live = fileEvent({ index: 42 });
  let deliverDuringPage = null;
  const h = harness({
    pages: [
      () => {
        deliverDuringPage?.();
        // The same event is also inside the history page that was in flight.
        return [fileEvent({ index: 41 }), live];
      },
    ],
  });
  deliverDuringPage = () => h.deliver(live);

  await h.controller.start();

  const keys = fileKeys(h.controller);
  assert.equal(keys.length, 2);
  assert.equal(new Set(keys).size, 2, "the live arrival is indexed once");
});

test("a deleted message's file disappears", async () => {
  const target = fileEvent({ index: 3 });
  const h = harness({ pages: [[fileEvent({ index: 2 }), target]] });

  await h.controller.start();
  assert.equal(fileKeys(h.controller).length, 2);

  h.deliver(deletionEvent(target.id));

  const files = selectIndexedFiles(h.controller.snapshot().index).files;
  assert.deepEqual(
    files.map((file) => file.filename),
    ["file-2.png"],
  );
});

test("an interrupted backfill resumes without duplicates", async () => {
  const first = Array.from({ length: BACKFILL_PAGE_LIMIT }, (_unused, i) =>
    fileEvent({ index: 500 - i }),
  );
  const h = harness({
    pages: [
      first,
      new Error("relay page timed out"),
      [fileEvent({ index: 1 })],
    ],
  });

  await h.controller.start();

  const interrupted = h.controller.snapshot();
  assert.equal(interrupted.complete, false);
  assert.match(interrupted.error ?? "", /relay page timed out/);
  assert.equal(fileKeys(h.controller).length, BACKFILL_PAGE_LIMIT);

  await h.controller.loadMore();

  const keys = fileKeys(h.controller);
  assert.equal(keys.length, BACKFILL_PAGE_LIMIT + 1);
  assert.equal(new Set(keys).size, keys.length, "resume adds no duplicates");
  assert.equal(h.controller.snapshot().error, null);
  // The resumed page continues from the interrupted cursor, not from the head.
  assert.equal(h.requests[2].until, h.requests[1].until);
  assert.equal(h.requests[2].beforeId, h.requests[1].beforeId);
});

test("the setting gates bulk drop", () => {
  const selection = ["a", "b", "c"];

  assert.deepEqual(
    resolveBulkDropKeys({
      draggedKey: "b",
      selectedKeys: selection,
      enabled: false,
    }),
    { keys: ["b"] },
    "with the setting off a drop moves only the dragged file",
  );
  assert.deepEqual(
    resolveBulkDropKeys({
      draggedKey: "b",
      selectedKeys: selection,
      enabled: true,
    }),
    { keys: selection },
    "with the setting on a drop moves the whole selection",
  );
  assert.equal(
    parseBulkDragDropEnabled(null),
    false,
    "the setting is off by default",
  );
  assert.equal(parseBulkDragDropEnabled("true"), true);
});

// ---------------------------------------------------------------------------
// Guards. Each of these fails if the guard it names is removed.
// ---------------------------------------------------------------------------

test("the live subscription opens before the first history page", async () => {
  const h = harness({ pages: [[fileEvent({ index: 1 })]] });

  await h.controller.start();

  assert.equal(h.calls[0], "subscribe");
  assert.equal(h.calls[1], "fetch");
});

test("a failed live subscription is surfaced and history still loads", async () => {
  const h = harness({
    pages: [[fileEvent({ index: 1 })]],
    subscribeFails: true,
  });

  await h.controller.start();

  const snapshot = h.controller.snapshot();
  assert.match(snapshot.error ?? "", /live updates/i);
  assert.match(snapshot.error ?? "", /relay socket refused/);
  assert.equal(fileKeys(h.controller).length, 1);
});

test("a history page that does not advance the cursor stops with an error", async () => {
  // Every entry is the same event, so the keyset cursor after the page equals
  // the cursor the page was fetched with. Without the guard this pages for
  // ever against a relay that ignores `before_id`.
  const stuck = Array.from({ length: BACKFILL_PAGE_LIMIT }, () =>
    fileEvent({ index: 5 }),
  );
  const h = harness({ pages: [stuck, stuck, stuck, stuck] });

  await h.controller.start();

  const snapshot = h.controller.snapshot();
  assert.match(snapshot.error ?? "", /did not advance/);
  assert.equal(h.requests.length, 2, "a stuck cursor stops paging");
});

test("the backfill stops at its page bound", async () => {
  const pages = Array.from({ length: MAX_BACKFILL_PAGES + 5 }, (_unused, p) =>
    Array.from({ length: BACKFILL_PAGE_LIMIT }, (_unused, i) =>
      fileEvent({ index: 1_000_000 - p * BACKFILL_PAGE_LIMIT - i }),
    ),
  );
  const h = harness({ pages });

  await h.controller.start();

  assert.equal(h.requests.length, MAX_BACKFILL_PAGES);
  assert.equal(h.controller.snapshot().complete, false);
  assert.equal(h.controller.snapshot().hasMore, true);
});

test("a page that resolves after dispose does not touch the index", async () => {
  let release;
  let fetchStarted;
  const gate = new Promise((resolve) => {
    release = resolve;
  });
  const fetchBegan = new Promise((resolve) => {
    fetchStarted = resolve;
  });
  const h = harness({
    pages: [
      async () => {
        fetchStarted();
        await gate;
        return [fileEvent({ index: 1 })];
      },
    ],
  });

  const started = h.controller.start();
  await fetchBegan;
  await h.controller.dispose();
  release();
  await started;

  assert.equal(fileKeys(h.controller).length, 0);
  assert.equal(h.isSubscriptionDisposed(), true);
});

test("an edit by another pubkey cannot rewrite the attachments", () => {
  const target = fileEvent({ index: 4 });
  let index = ingestIndexEvents(emptyFilesIndex(), [target]);
  index = ingestIndexEvents(index, [
    editEvent(target.id, { pubkey: "mallory" }),
  ]);

  assert.deepEqual(
    selectIndexedFiles(index).files.map((file) => file.filename),
    ["file-4.png"],
  );

  const authorized = ingestIndexEvents(index, [
    editEvent(target.id, { pubkey: target.pubkey }),
  ]);
  assert.deepEqual(
    selectIndexedFiles(authorized).files.map((file) => file.filename),
    ["edited.png"],
  );
});

test("relay-sourced strings and counts are capped when an event is indexed", () => {
  const hostile = fileEvent({
    index: 9,
    content: "x".repeat(10_000),
    tags: [
      ...Array.from({ length: 60 }, (_unused, i) =>
        imetaTag({
          url: `https://relay.example/media/hostile-${i}.png`,
          m: "image/png",
          filename: "z".repeat(5_000),
        }),
      ),
      ...Array.from({ length: 5_000 }, () => ["p", "y".repeat(5_000)]),
    ],
  });

  const bounded = boundIndexSource(hostile);

  assert.ok(bounded, "a valid event is indexed");
  assert.ok(
    bounded.tags.length <= 20,
    `imeta tags kept: ${bounded.tags.length}`,
  );
  assert.ok(bounded.content.length <= 2_000);
  for (const tag of bounded.tags) {
    for (const part of tag) assert.ok(part.length <= 4_096);
  }

  const index = ingestIndexEvents(emptyFilesIndex(), [hostile]);
  const files = selectIndexedFiles(index).files;
  assert.ok(files.length <= 20, `rows produced: ${files.length}`);
  for (const file of files) {
    assert.ok((file.filename ?? "").length <= MAX_FILENAME_LENGTH);
  }
});

test("an event with an unusable id is refused", () => {
  assert.equal(boundIndexSource(fileEvent({ id: "not-hex" })), null);
  assert.equal(boundIndexSource(fileEvent({ id: "a".repeat(65) })), null);
});

test("the index bounds the events, deletions and edits it retains", () => {
  let index = emptyFilesIndex();
  index = ingestIndexEvents(
    index,
    Array.from({ length: MAX_INDEXED_ATTACHMENT_EVENTS + 10 }, (_unused, i) =>
      fileEvent({ index: i + 1 }),
    ),
  );
  assert.equal(index.sources.size, MAX_INDEXED_ATTACHMENT_EVENTS);
  assert.equal(index.truncated, true);

  let deletions = emptyFilesIndex();
  deletions = ingestIndexEvents(
    deletions,
    Array.from({ length: MAX_INDEXED_DELETIONS + 5 }, (_unused, i) =>
      deletionEvent(hexId(i + 1), { id: hexId(500_000 + i) }),
    ),
  );
  assert.equal(deletions.deletions.size, MAX_INDEXED_DELETIONS);
  assert.equal(deletions.truncated, true);

  let edits = emptyFilesIndex();
  edits = ingestIndexEvents(
    edits,
    Array.from({ length: MAX_INDEXED_EDITS + 5 }, (_unused, i) =>
      editEvent(hexId(i + 1), { id: hexId(600_000 + i) }),
    ),
  );
  assert.equal(edits.edits.size, MAX_INDEXED_EDITS);
  assert.equal(edits.truncated, true);
});

test("ingesting nothing new returns the same index object", () => {
  const event = fileEvent({ index: 1 });
  const index = ingestIndexEvents(emptyFilesIndex(), [event]);

  assert.equal(ingestIndexEvents(index, [event]), index);
  assert.equal(ingestIndexEvents(index, []), index);
  // A non-attachment message never enters the index.
  assert.equal(
    ingestIndexEvents(index, [
      { ...fileEvent({ index: 2 }), tags: [["h", "channel-1"]] },
    ]),
    index,
  );
});

test("a bulk drop refuses a selection larger than the batch cap", () => {
  const selection = Array.from(
    { length: MAX_BULK_DROP_FILES + 1 },
    (_unused, i) => `file-${i}`,
  );

  const plan = resolveBulkDropKeys({
    draggedKey: selection[0],
    selectedKeys: selection,
    enabled: true,
  });

  assert.equal(plan.keys, undefined, "an over-cap drop moves nothing");
  assert.match(plan.refusedReason ?? "", /20/);
});

test("a bulk drop of a file outside the selection moves only that file", () => {
  assert.deepEqual(
    resolveBulkDropKeys({
      draggedKey: "z",
      selectedKeys: ["a", "b"],
      enabled: true,
    }),
    { keys: ["z"] },
  );
});

test("a relay failure message is capped before it reaches the user", async () => {
  const h = harness({ pages: [new Error("x".repeat(5_000))] });

  await h.controller.start();

  const error = h.controller.snapshot().error ?? "";
  assert.match(error, /Could not load older files/);
  assert.ok(
    error.length <= 80 + MAX_ERROR_DETAIL_LENGTH,
    `error length ${error.length}`,
  );
});
