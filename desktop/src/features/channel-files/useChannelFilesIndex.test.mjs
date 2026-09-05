import assert from "node:assert/strict";
import { after, afterEach, before, mock, test } from "node:test";
import { JSDOM } from "jsdom";

import {
  MAX_INDEXED_ATTACHMENT_EVENTS,
  MAX_INDEXED_DELETIONS,
  MAX_INDEXED_EDITS,
  MAX_INDEXED_PUBKEY_LENGTH,
  boundIndexSource,
  emptyFilesIndex,
  ingestIndexEvents,
  selectIndexedFiles,
} from "./channelFilesIndex.ts";
import {
  BACKFILL_PAGE_LIMIT,
  MAX_BACKFILL_PAGES,
  MAX_ERROR_DETAIL_LENGTH,
  MAX_LIVE_SUBSCRIBE_ATTEMPTS,
  createChannelFilesIndexController,
  oldestPageCursor,
} from "./channelFilesBackfill.ts";
import { isFilesIndexEnabled } from "./filesTabActivation.ts";
import { relayClient } from "@/shared/api/relayClient";
import { useChannelFilesIndex } from "./useChannelFilesIndex.ts";
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
  // `true` fails for ever; a number fails that many attempts and then opens.
  let failuresLeft =
    subscribeFails === true ? Number.POSITIVE_INFINITY : subscribeFails || 0;
  const snapshots = [];

  const controller = createChannelFilesIndexController({
    channelId: "channel-1",
    subscribeLive: async (_channelId, onEvent) => {
      calls.push("subscribe");
      if (failuresLeft > 0) {
        failuresLeft -= 1;
        throw new Error("relay socket refused");
      }
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
    subscribeCalls: () => calls.filter((call) => call === "subscribe").length,
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

// ---------------------------------------------------------------------------
// Retention caps: keep the newest, evict the oldest.
//
// A cap that only refused arrivals would freeze a busy channel's index on the
// first N events it saw and drop every later file, while the tab kept saying it
// showed the most recent attachments. These bind the eviction rule instead —
// newer in, oldest out, and only an entry older than the retained floor is
// refused. Every assertion here fails if its guard is taken out.
// ---------------------------------------------------------------------------

/** `MAX_INDEXED_ATTACHMENT_EVENTS` sources, created_at 1001 ascending. */
let filledSources = null;
function sourcesAtCap() {
  filledSources ??= ingestIndexEvents(
    emptyFilesIndex(),
    Array.from({ length: MAX_INDEXED_ATTACHMENT_EVENTS }, (_unused, i) =>
      fileEvent({ index: i + 1 }),
    ),
  );
  return filledSources;
}

test("a newer attachment evicts the oldest instead of being refused", () => {
  const full = sourcesAtCap();
  assert.equal(full.sources.size, MAX_INDEXED_ATTACHMENT_EVENTS);
  assert.equal(
    full.truncated,
    false,
    "a cap that is merely full has dropped nothing yet",
  );

  const arrival = fileEvent({ index: MAX_INDEXED_ATTACHMENT_EVENTS + 1 });
  const after = ingestIndexEvents(full, [arrival]);

  assert.equal(after.sources.size, MAX_INDEXED_ATTACHMENT_EVENTS);
  assert.ok(after.sources.has(arrival.id), "the live arrival is indexed");
  assert.equal(
    after.sources.has(hexId(1)),
    false,
    "the oldest retained attachment made way for it",
  );
  assert.equal(
    after.truncated,
    true,
    "and the tab is told older attachments are missing",
  );
});

test("an attachment older than the retained floor is refused", () => {
  const full = sourcesAtCap();
  // created_at 1000, one second below the floor at 1001.
  const older = fileEvent({ index: 0 });
  const after = ingestIndexEvents(full, [older]);

  assert.equal(after.sources.size, MAX_INDEXED_ATTACHMENT_EVENTS);
  assert.equal(
    after.sources.has(older.id),
    false,
    "an entry older than the floor is what the cap exists to leave out",
  );
  assert.ok(after.sources.has(hexId(1)), "so the floor itself is still held");
  assert.equal(after.truncated, true);
});

test("evicting an attachment drops the edit and deletion it owned", () => {
  const full = ingestIndexEvents(sourcesAtCap(), [
    editEvent(hexId(1)),
    deletionEvent(hexId(1)),
  ]);
  assert.ok(full.edits.has(hexId(1)));
  assert.ok(full.deletions.has(hexId(1)));

  const after = ingestIndexEvents(full, [
    fileEvent({ index: MAX_INDEXED_ATTACHMENT_EVENTS + 1 }),
  ]);

  assert.equal(after.sources.has(hexId(1)), false);
  assert.equal(
    after.edits.has(hexId(1)),
    false,
    "the evicted message's edit goes with it",
  );
  assert.equal(
    after.deletions.has(hexId(1)),
    false,
    "and so does its deletion",
  );
});

test("a newer edit evicts the oldest edit, an older one is refused", () => {
  const target = (n) => hexId(200_000 + n);
  const atCap = ingestIndexEvents(
    emptyFilesIndex(),
    Array.from({ length: MAX_INDEXED_EDITS }, (_unused, i) =>
      editEvent(target(i + 1), {
        id: hexId(600_000 + i),
        created_at: 8_000 + i + 1,
      }),
    ),
  );
  assert.equal(atCap.edits.size, MAX_INDEXED_EDITS);
  assert.equal(atCap.truncated, false);

  const newerTarget = target(MAX_INDEXED_EDITS + 1);
  const evicted = ingestIndexEvents(atCap, [
    editEvent(newerTarget, {
      id: hexId(700_001),
      created_at: 8_000 + MAX_INDEXED_EDITS + 1,
    }),
  ]);
  assert.equal(evicted.edits.size, MAX_INDEXED_EDITS);
  assert.ok(evicted.edits.has(newerTarget), "the newer edit is tracked");
  assert.equal(
    evicted.edits.has(target(1)),
    false,
    "the oldest edit made way for it",
  );
  assert.equal(evicted.truncated, true);

  const refused = ingestIndexEvents(atCap, [
    editEvent(target(0), { id: hexId(700_002), created_at: 8_000 }),
  ]);
  assert.equal(
    refused.edits.has(target(0)),
    false,
    "an edit older than the floor is refused",
  );
  assert.ok(refused.edits.has(target(1)), "and the floor is kept");
  assert.equal(refused.truncated, true);
});

test("a newer deletion evicts the oldest deletion, an older one is refused", () => {
  const target = (n) => hexId(300_000 + n);
  const atCap = ingestIndexEvents(
    emptyFilesIndex(),
    Array.from({ length: MAX_INDEXED_DELETIONS }, (_unused, i) =>
      deletionEvent(target(i + 1), {
        id: hexId(500_000 + i),
        created_at: 9_000 + i + 1,
      }),
    ),
  );
  assert.equal(atCap.deletions.size, MAX_INDEXED_DELETIONS);
  assert.equal(atCap.truncated, false);

  const newerTarget = target(MAX_INDEXED_DELETIONS + 1);
  const evicted = ingestIndexEvents(atCap, [
    deletionEvent(newerTarget, {
      id: hexId(510_001),
      created_at: 9_000 + MAX_INDEXED_DELETIONS + 1,
    }),
  ]);
  assert.equal(evicted.deletions.size, MAX_INDEXED_DELETIONS);
  assert.ok(evicted.deletions.has(newerTarget), "the newer deletion applies");
  assert.equal(
    evicted.deletions.has(target(1)),
    false,
    "the oldest deletion made way for it",
  );
  assert.equal(evicted.truncated, true);

  const refused = ingestIndexEvents(atCap, [
    deletionEvent(target(0), { id: hexId(510_002), created_at: 9_000 }),
  ]);
  assert.equal(
    refused.deletions.has(target(0)),
    false,
    "a deletion older than the floor is refused",
  );
  assert.ok(refused.deletions.has(target(1)), "and the floor is kept");
  assert.equal(refused.truncated, true);
});

test("a deletion with an unusable timestamp sorts oldest and never displaces one", () => {
  const target = (n) => hexId(310_000 + n);
  const atCap = ingestIndexEvents(
    emptyFilesIndex(),
    Array.from({ length: MAX_INDEXED_DELETIONS }, (_unused, i) =>
      deletionEvent(target(i + 1), {
        id: hexId(530_000 + i),
        created_at: 9_000 + i + 1,
      }),
    ),
  );

  const after = ingestIndexEvents(atCap, [
    deletionEvent(target(0), { id: hexId(540_000), created_at: Number.NaN }),
  ]);
  assert.equal(
    after.deletions.has(target(0)),
    false,
    "an untimestamped deletion cannot evict a timestamped one",
  );
  assert.ok(after.deletions.has(target(1)));

  // Under the cap the missing timestamp costs nothing: the file still goes.
  const small = ingestIndexEvents(emptyFilesIndex(), [
    fileEvent({ index: 1 }),
    deletionEvent(hexId(1), { created_at: Number.NaN }),
  ]);
  assert.equal(selectIndexedFiles(small).files.length, 0);

  // And it must not jam the cap. An unnormalised timestamp compares false
  // against every other, so a retained one would be an entry nothing can ever
  // displace and no later deletion would land again.
  const jammed = ingestIndexEvents(emptyFilesIndex(), [
    deletionEvent(target(0), { id: hexId(540_001), created_at: Number.NaN }),
    ...Array.from({ length: MAX_INDEXED_DELETIONS - 1 }, (_unused, i) =>
      deletionEvent(target(i + 1), {
        id: hexId(530_000 + i),
        created_at: 9_000 + i + 1,
      }),
    ),
  ]);
  assert.equal(jammed.deletions.size, MAX_INDEXED_DELETIONS);
  const newerTarget = target(MAX_INDEXED_DELETIONS + 1);
  const unjammed = ingestIndexEvents(jammed, [
    deletionEvent(newerTarget, {
      id: hexId(540_002),
      created_at: 9_000 + MAX_INDEXED_DELETIONS + 1,
    }),
  ]);
  assert.ok(
    unjammed.deletions.has(newerTarget),
    "a newer deletion still lands",
  );
  assert.equal(
    unjammed.deletions.has(target(0)),
    false,
    "the untimestamped deletion is the oldest, so the first evicted",
  );
});

test("a deletion that hides a retained attachment is never the eviction victim", () => {
  // The failure this guards: the deletion entry is the ONLY thing hiding a
  // deleted row, so evicting it while the message is still indexed puts the
  // file back on screen — and the banner never says a deleted file returned.
  const secret = fileEvent({ index: 1 });
  const hidden = ingestIndexEvents(emptyFilesIndex(), [
    secret,
    // The oldest deletion of the lot, so pure recency would pick it.
    deletionEvent(secret.id, { id: hexId(590_000), created_at: 9_000 }),
    ...Array.from({ length: MAX_INDEXED_DELETIONS - 1 }, (_unused, i) =>
      deletionEvent(hexId(340_000 + i + 1), {
        id: hexId(591_000 + i),
        created_at: 9_001 + i,
      }),
    ),
  ]);
  assert.equal(hidden.deletions.size, MAX_INDEXED_DELETIONS);
  assert.equal(
    selectIndexedFiles(hidden).files.length,
    0,
    "the deleted attachment is hidden",
  );

  const after = ingestIndexEvents(hidden, [
    deletionEvent(hexId(345_000), {
      id: hexId(596_000),
      created_at: 9_000 + MAX_INDEXED_DELETIONS + 10,
    }),
  ]);

  assert.ok(
    after.deletions.has(secret.id),
    "an unrelated newer deletion evicts an orphan, not the one doing work",
  );
  assert.equal(
    selectIndexedFiles(after).files.length,
    0,
    "so the deleted attachment does not come back",
  );
});

test("an edit for a visible row outranks edits for messages the index never saw", () => {
  // Otherwise a burst of edits for unrelated messages fills the cap and the
  // genuine edit of a row on screen is refused for good.
  const visible = fileEvent({ index: 1 });
  const index = ingestIndexEvents(emptyFilesIndex(), [
    visible,
    ...Array.from({ length: MAX_INDEXED_EDITS }, (_unused, i) =>
      editEvent(hexId(210_000 + i + 1), {
        id: hexId(630_000 + i),
        created_at: 20_000 + i,
      }),
    ),
  ]);
  assert.equal(index.edits.size, MAX_INDEXED_EDITS);
  assert.deepEqual(
    selectIndexedFiles(index).files.map((file) => file.filename),
    ["file-1.png"],
  );

  // Older than every one of them, and admitted anyway: it is the only edit in
  // the map that can change anything the tab shows.
  const after = ingestIndexEvents(index, [
    editEvent(visible.id, { id: hexId(640_000), created_at: 8_000 }),
  ]);

  assert.equal(after.edits.size, MAX_INDEXED_EDITS);
  assert.deepEqual(
    selectIndexedFiles(after).files.map((file) => file.filename),
    ["edited.png"],
    "the visible row takes its edit",
  );
});

test("evicting an attachment drops a deletion keyed by its edit as well", () => {
  // The projection honours a deletion of the EDIT (a deleted edit stops
  // rewriting its message), so that entry describes the evicted row too.
  const edit = editEvent(hexId(1), { id: hexId(650_000) });
  const full = ingestIndexEvents(sourcesAtCap(), [
    edit,
    deletionEvent(edit.id, { id: hexId(660_000) }),
  ]);
  assert.ok(full.deletions.has(edit.id));

  const after = ingestIndexEvents(full, [
    fileEvent({ index: MAX_INDEXED_ATTACHMENT_EVENTS + 1 }),
  ]);

  assert.equal(after.sources.has(hexId(1)), false);
  assert.equal(
    after.deletions.has(edit.id),
    false,
    "the marker for the evicted message's edit goes with it",
  );
});

test("at equal timestamps the retention order breaks ties on the entry id", () => {
  // Ties resolve the way the projection sorts them. Without that, two entries
  // sharing one timestamp make eviction, refusal and page order disagree, and
  // a relay that stamps a whole batch alike could not evict at all.
  const target = (n) => hexId(330_000 + n);
  const atCap = ingestIndexEvents(
    emptyFilesIndex(),
    Array.from({ length: MAX_INDEXED_DELETIONS }, (_unused, i) =>
      deletionEvent(target(i + 1), {
        id: hexId(570_000 + i),
        created_at: 9_000,
      }),
    ),
  );
  assert.equal(atCap.deletions.size, MAX_INDEXED_DELETIONS);

  const higher = target(MAX_INDEXED_DELETIONS + 1);
  const evicted = ingestIndexEvents(atCap, [
    deletionEvent(higher, { id: hexId(580_000), created_at: 9_000 }),
  ]);
  assert.ok(evicted.deletions.has(higher), "the higher id wins the tie");
  assert.equal(
    evicted.deletions.has(target(1)),
    false,
    "and the lowest id was the floor",
  );

  const lower = target(0);
  const refused = ingestIndexEvents(atCap, [
    deletionEvent(lower, { id: hexId(580_001), created_at: 9_000 }),
  ]);
  assert.equal(
    refused.deletions.has(lower),
    false,
    "a lower id at the same timestamp sits below the floor",
  );
  assert.ok(refused.deletions.has(target(1)));
});

test("at a full deletion cap a newer deletion still hides its file, and the tab is told", () => {
  // Deletions, so the retained ROWS stay far under the projection's own row
  // cap: what reaches the banner here is the index's eviction and nothing else.
  const kept = fileEvent({ index: 1 });
  const removed = fileEvent({ index: 2 });
  const full = ingestIndexEvents(emptyFilesIndex(), [
    kept,
    removed,
    ...Array.from({ length: MAX_INDEXED_DELETIONS }, (_unused, i) =>
      deletionEvent(hexId(320_000 + i + 1), {
        id: hexId(550_000 + i),
        created_at: 9_000 + i + 1,
      }),
    ),
  ]);
  assert.equal(full.deletions.size, MAX_INDEXED_DELETIONS);
  assert.equal(selectIndexedFiles(full).files.length, 2);
  assert.equal(selectIndexedFiles(full).truncated, false);

  const after = ingestIndexEvents(full, [
    deletionEvent(removed.id, {
      id: hexId(560_000),
      created_at: 9_000 + MAX_INDEXED_DELETIONS + 1,
    }),
  ]);
  const projection = selectIndexedFiles(after);

  assert.deepEqual(
    projection.files.map((file) => file.eventId),
    [kept.id],
    "the newer deletion took effect over a full cap",
  );
  assert.equal(
    projection.truncated,
    true,
    "and the banner state reports that older entries were dropped",
  );
});

test("eviction and refusal leave the same page, whichever order events arrive in", () => {
  // Deterministic pagination: the live path arrives oldest-first and evicts,
  // the backfill arrives newest-first and is refused at the floor. Both must
  // retain the same newest entries in the same total order, or the page a
  // caller is reading would shuffle under it.
  const events = Array.from(
    { length: MAX_INDEXED_ATTACHMENT_EVENTS + 25 },
    (_unused, i) => fileEvent({ index: i + 1 }),
  );
  const evicting = ingestIndexEvents(emptyFilesIndex(), events);
  const refusing = ingestIndexEvents(emptyFilesIndex(), [...events].reverse());

  assert.equal(evicting.sources.size, MAX_INDEXED_ATTACHMENT_EVENTS);
  assert.equal(refusing.sources.size, MAX_INDEXED_ATTACHMENT_EVENTS);
  assert.equal(evicting.truncated, true);
  assert.equal(refusing.truncated, true);

  const evictedOrder = selectIndexedFiles(evicting).files.map(
    (file) => file.eventId,
  );
  const refusedOrder = selectIndexedFiles(refusing).files.map(
    (file) => file.eventId,
  );
  assert.deepEqual(evictedOrder, refusedOrder);
  assert.equal(
    evictedOrder[0],
    events.at(-1).id,
    "the newest attachment heads the first page",
  );
  assert.equal(
    evicting.sources.has(hexId(25)),
    false,
    "the 25 oldest were evicted",
  );
  assert.ok(evicting.sources.has(hexId(26)), "and the 26th is the new floor");
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

test("a history page that is not an array is refused, not thrown out of the walk", async () => {
  // `fetchPage` returns whatever the backend command sent. A contract
  // violation used to escape `backfill` as an unhandled rejection: the walk
  // stopped, nothing was surfaced, and the tab sat on a partial list saying
  // everything was fine.
  const h = harness({ pages: [() => null, [fileEvent({ index: 1 })]] });

  await assert.doesNotReject(() => h.controller.start());

  const snapshot = h.controller.snapshot();
  assert.match(snapshot.error ?? "", /Could not load older files/);
  // Named for what actually went wrong, not the TypeError it would otherwise
  // surface from deep inside ingestion.
  assert.match(snapshot.error ?? "", /page that is not a list/);
  assert.equal(snapshot.complete, false, "a refused page is not the end");
  assert.equal(snapshot.hasMore, true, "the user can still retry");
});

test("a page the index cannot read leaves the cursor where it was", async () => {
  // The ordering guard: events are ingested before the cursor advances, so a
  // failure in between costs a refetch of one page and never skips one.
  const first = Array.from({ length: BACKFILL_PAGE_LIMIT }, (_unused, i) =>
    fileEvent({ index: 500 - i }),
  );
  const hostile = Array.from({ length: BACKFILL_PAGE_LIMIT }, (_unused, i) =>
    fileEvent({ index: 400 - i }),
  );
  // A relay-shaped object whose `tags` cannot be read. Reading it is the first
  // thing ingestion does, so the page fails before anything is recorded.
  Object.defineProperty(hostile[0], "tags", {
    get() {
      throw new Error("tags unreadable");
    },
  });

  const h = harness({
    pages: [first, hostile, [fileEvent({ index: 1 })]],
  });

  await assert.doesNotReject(() => h.controller.start());

  const stopped = h.controller.snapshot();
  assert.match(stopped.error ?? "", /tags unreadable/);
  assert.equal(stopped.complete, false);
  assert.equal(fileKeys(h.controller).length, BACKFILL_PAGE_LIMIT);

  await h.controller.loadMore();

  // The retry asks for the SAME page again: the failed page advanced nothing.
  assert.equal(h.requests[2].until, h.requests[1].until);
  assert.equal(h.requests[2].beforeId, h.requests[1].beforeId);
  const keys = fileKeys(h.controller);
  assert.equal(new Set(keys).size, keys.length, "the retry adds no duplicates");
});

// ---------------------------------------------------------------------------
// The live subscription recovers, and the banner's Retry is what recovers it.
// ---------------------------------------------------------------------------

test("Retry opens the live subscription again and the next live event lands", async () => {
  // The failure this binds: `subscribe` used to run only from `start`, which
  // latched after one call, so a refused socket left the tab following nothing
  // for its whole life and the banner's Retry only re-walked history.
  const h = harness({ pages: [[fileEvent({ index: 1 })]], subscribeFails: 1 });

  await h.controller.start();

  assert.equal(h.subscribeCalls(), 1);
  assert.match(h.controller.snapshot().error ?? "", /live updates/i);
  assert.equal(h.controller.snapshot().liveConnected, false);

  await h.controller.retry();

  assert.equal(h.subscribeCalls(), 2, "Retry re-subscribes");
  assert.equal(h.controller.snapshot().liveConnected, true);
  assert.equal(
    h.controller.snapshot().error,
    null,
    "a subscription that opened clears the banner",
  );

  h.deliver(fileEvent({ index: 2 }));

  assert.deepEqual(
    fileKeys(h.controller).length,
    2,
    "live events reach the index",
  );
});

test("a live retry while the subscription is open does not reach the relay", async () => {
  const h = harness({ pages: [[fileEvent({ index: 1 })]] });

  await h.controller.start();
  await h.controller.retryLive();
  await h.controller.retryLive();

  assert.equal(h.subscribeCalls(), 1, "an open subscription is not reopened");
});

test("a retry in flight leaves the banner standing until it succeeds", async () => {
  // Clearing the banner when the attempt STARTS would tell the user the tab is
  // following the channel again while it is still not.
  let attempts = 0;
  let release;
  const gate = new Promise((resolve) => {
    release = resolve;
  });
  const controller = createChannelFilesIndexController({
    channelId: "channel-1",
    subscribeLive: async () => {
      attempts += 1;
      if (attempts === 1) throw new Error("relay socket refused");
      await gate;
      return () => {};
    },
    fetchPage: async () => [],
  });

  await controller.start();
  const banner = controller.snapshot().error;
  assert.match(banner ?? "", /live updates/i);

  const retrying = controller.retryLive();
  await Promise.resolve();
  assert.equal(controller.snapshot().error, banner);

  release();
  await retrying;

  assert.equal(controller.snapshot().error, null);
});

test("the live subscription stops asking once its attempt budget is spent", async () => {
  const h = harness({
    pages: [[fileEvent({ index: 1 })]],
    subscribeFails: true,
  });

  await h.controller.start();
  for (
    let attempt = 0;
    attempt < MAX_LIVE_SUBSCRIBE_ATTEMPTS + 3;
    attempt += 1
  ) {
    await h.controller.retryLive();
  }

  assert.equal(h.subscribeCalls(), MAX_LIVE_SUBSCRIBE_ATTEMPTS);
  assert.equal(h.controller.snapshot().liveTerminal, true);
  assert.match(h.controller.snapshot().error ?? "", /Reopen the Files tab/);
});

test("a malformed live event leaves the index unchanged and never throws", () => {
  // The relay's EVENT payload is `JSON.parse`d and cast (`relayClientSession`
  // `handleWsMessage`), so every field is untrusted shape, not just untrusted
  // content. Reading one as a string is what threw a TypeError out of the
  // dispatcher and discarded the rest of the batch.
  const hostile = [
    { ...fileEvent({ index: 1 }), pubkey: 42 },
    { ...fileEvent({ index: 2 }), content: { body: "x" } },
    { ...fileEvent({ index: 3 }), tags: "not-a-list" },
    { ...fileEvent({ index: 4 }), id: 7 },
    { ...fileEvent({ index: 5 }), created_at: "soon" },
    null,
    "an event",
  ];

  for (const event of hostile) {
    const before = emptyFilesIndex();
    const after = ingestIndexEvents(before, [event]);
    if (after !== before) {
      // The two that survive are indexable; their bad fields must be dropped,
      // never read.
      for (const source of after.sources.values()) {
        assert.equal(typeof source.pubkey, "string");
        assert.equal(typeof source.content, "string");
      }
    }
  }

  assert.equal(
    ingestIndexEvents(emptyFilesIndex(), [
      { ...fileEvent({ index: 1 }), pubkey: 42 },
    ]).sources.get(hexId(1)).pubkey,
    "",
    "a non-string pubkey becomes empty, so the projection stays total",
  );
  assert.equal(
    ingestIndexEvents(emptyFilesIndex(), [
      { ...fileEvent({ index: 3 }), tags: "not-a-list" },
    ]),
    emptyFilesIndex(),
    "an event with no readable imeta tag is not indexed at all",
  );
});

test("a failing snapshot sink becomes a banner, not a throw into the relay", async () => {
  // `onChange` is React's state setter, so it carries a render. The live
  // callback runs inside the relay client's shared dispatcher, which walks one
  // buffer for every subscription in a bare loop: a throw here discards the
  // rest of that batch for the timeline, unread counts and huddles too.
  let live = null;
  let armed = false;
  const controller = createChannelFilesIndexController({
    channelId: "channel-1",
    subscribeLive: async (_channelId, onEvent) => {
      live = onEvent;
      return () => {};
    },
    fetchPage: async () => [],
    onChange: () => {
      if (armed) throw new Error("render failed");
    },
  });

  await controller.start();
  armed = true;

  assert.doesNotThrow(() => live(fileEvent({ index: 9 })));
  assert.match(
    controller.snapshot().error ?? "",
    /live updates/i,
    "the failure is surfaced to the user, not swallowed",
  );
});

test("a live event that lands while dispose is in flight is ignored", async () => {
  // `dispose` marks the controller disposed and then awaits the unsubscribe.
  // A buffered event flushed in that window would otherwise index into a
  // channel view that is already gone.
  let live = null;
  let release;
  const closing = new Promise((resolve) => {
    release = resolve;
  });
  const controller = createChannelFilesIndexController({
    channelId: "channel-1",
    subscribeLive: async (_channelId, onEvent) => {
      live = onEvent;
      return async () => {
        await closing;
      };
    },
    fetchPage: async () => [],
  });

  await controller.start();
  const disposing = controller.dispose();
  live(fileEvent({ index: 7 }));
  release();
  await disposing;

  assert.equal(controller.snapshot().index.sources.size, 0);
});

test("dispose closes a subscription that was still opening", async () => {
  let release;
  const gate = new Promise((resolve) => {
    release = resolve;
  });
  let closed = false;
  const controller = createChannelFilesIndexController({
    channelId: "channel-1",
    subscribeLive: async () => {
      await gate;
      return () => {
        closed = true;
      };
    },
    fetchPage: async () => [],
  });

  const started = controller.start();
  const disposing = controller.dispose();
  release();
  await disposing;

  assert.equal(
    closed,
    true,
    "dispose waits for the subscription it must close",
  );
  await started;
});

test("a disposed controller offers no more history", async () => {
  const h = harness({ pages: [new Error("relay down")] });

  await h.controller.start();
  assert.equal(h.controller.snapshot().hasMore, true);

  await h.controller.dispose();

  assert.equal(h.controller.snapshot().hasMore, false);
});

test("a page cursor ignores an unusable timestamp", () => {
  assert.deepEqual(
    oldestPageCursor([
      { id: hexId(1), created_at: Number.NaN },
      { id: hexId(2), created_at: 50 },
      { id: hexId(3), created_at: 70 },
    ]),
    { until: 50, beforeId: hexId(2) },
  );
});

// ---------------------------------------------------------------------------
// The activation gate.
// ---------------------------------------------------------------------------

test("the index runs only for the channel whose Files tab was opened", () => {
  assert.equal(isFilesIndexEnabled(null, "channel-a"), false);
  assert.equal(isFilesIndexEnabled("channel-a", "channel-a"), true);
  // A-Files then B-Chat: the previous channel's activation must not carry.
  assert.equal(isFilesIndexEnabled("channel-a", "channel-b"), false);
  assert.equal(isFilesIndexEnabled("channel-a", null), false);
});

// ---------------------------------------------------------------------------
// Caps and retention.
// ---------------------------------------------------------------------------

test("an over-long pubkey is truncated and an unusable timestamp is refused", () => {
  const bounded = boundIndexSource(
    fileEvent({ index: 3, pubkey: "f".repeat(4_000) }),
  );

  assert.ok(bounded);
  assert.equal(bounded.pubkey.length, MAX_INDEXED_PUBKEY_LENGTH);

  assert.equal(
    boundIndexSource(fileEvent({ index: 3, created_at: 1.5 })),
    null,
    "a non-integer timestamp is refused",
  );
  assert.equal(
    boundIndexSource(fileEvent({ index: 3, created_at: -1 })),
    null,
    "a negative timestamp is refused",
  );
  assert.equal(
    boundIndexSource(fileEvent({ index: 3, created_at: Number.NaN })),
    null,
  );
});

test("an index that hit a retention cap reports a truncated list", () => {
  // Deletions, so the retained ROWS stay far under the projection's own row
  // cap: this is the index's retention cap reaching the tab's notice, which is
  // the #4428 property, and nothing else can produce it here.
  const index = ingestIndexEvents(emptyFilesIndex(), [
    fileEvent({ index: 1 }),
    ...Array.from({ length: MAX_INDEXED_DELETIONS + 5 }, (_unused, i) =>
      deletionEvent(hexId(700_000 + i), { id: hexId(400_000 + i) }),
    ),
  ]);

  assert.equal(index.truncated, true);
  const projection = selectIndexedFiles(index);
  assert.equal(
    projection.files.length,
    1,
    "the rows themselves are not capped",
  );
  assert.equal(
    projection.truncated,
    true,
    "the tab is told the list is partial",
  );
});

// ---------------------------------------------------------------------------
// Edit and deletion overlay rules.
// ---------------------------------------------------------------------------

test("an older edit cannot displace a newer one", () => {
  const target = fileEvent({ index: 6 });
  let index = ingestIndexEvents(emptyFilesIndex(), [target]);
  index = ingestIndexEvents(index, [
    editEvent(target.id, {
      id: hexId(810_001),
      created_at: 9_000,
      imeta: [
        [
          "imeta",
          "url https://relay.example/media/newest.png",
          "m image/png",
          "filename newest.png",
        ],
      ],
    }),
  ]);
  index = ingestIndexEvents(index, [
    editEvent(target.id, {
      id: hexId(810_002),
      created_at: 1_000,
      imeta: [
        [
          "imeta",
          "url https://relay.example/media/stale.png",
          "m image/png",
          "filename stale.png",
        ],
      ],
    }),
  ]);

  assert.deepEqual(
    selectIndexedFiles(index).files.map((file) => file.filename),
    ["newest.png"],
  );
});

test("a deleted edit stops rewriting the message it edited", () => {
  const target = fileEvent({ index: 7 });
  const edit = editEvent(target.id, { id: hexId(820_001) });
  let index = ingestIndexEvents(emptyFilesIndex(), [target, edit]);

  assert.deepEqual(
    selectIndexedFiles(index).files.map((file) => file.filename),
    ["edited.png"],
  );

  index = ingestIndexEvents(index, [
    deletionEvent(edit.id, { id: hexId(830_001) }),
  ]);

  assert.deepEqual(
    selectIndexedFiles(index).files.map((file) => file.filename),
    ["file-7.png"],
    "the edit is gone, the message it edited is not",
  );
});

test("an edit with an unusable id cannot rewrite a message", () => {
  const target = fileEvent({ index: 8 });
  const index = ingestIndexEvents(emptyFilesIndex(), [
    target,
    editEvent(target.id, { id: "not-a-hex-id" }),
  ]);

  assert.deepEqual(
    selectIndexedFiles(index).files.map((file) => file.filename),
    ["file-8.png"],
  );
});

test("rows with the same timestamp keep one total order", () => {
  const older = fileEvent({ index: 11, id: hexId(0x11), created_at: 5_000 });
  const newer = fileEvent({ index: 12, id: hexId(0x22), created_at: 5_000 });
  // Ingested largest id first, so insertion order and id order disagree.
  const index = ingestIndexEvents(emptyFilesIndex(), [newer, older]);

  assert.deepEqual(
    selectIndexedFiles(index).files.map((file) => file.filename),
    ["file-12.png", "file-11.png"],
  );
});

test("an edit that arrived before its message is dropped when it proves forged", () => {
  const target = fileEvent({ index: 13 });
  // The edit lands first, when there is no author to check it against.
  let index = ingestIndexEvents(emptyFilesIndex(), [
    editEvent(target.id, { pubkey: "mallory" }),
  ]);
  assert.equal(index.edits.size, 1);

  index = ingestIndexEvents(index, [target]);

  assert.equal(index.edits.size, 0, "the forged edit is not retained");
  assert.deepEqual(
    selectIndexedFiles(index).files.map((file) => file.filename),
    ["file-13.png"],
  );
});

test("the projection refuses an edit signed by anyone but the author", () => {
  // Built directly, because ingestion drops a forged edit before this point.
  // The projection is the last thing between a forged edit and the user, and
  // it holds on its own.
  const source = boundIndexSource(fileEvent({ index: 14 }));
  const forged = {
    id: hexId(840_001),
    pubkey: "mallory",
    created_at: 9_999,
    content: "forged",
    tags: [
      [
        "imeta",
        "url https://relay.example/media/forged.png",
        "m image/png",
        "filename forged.png",
      ],
    ],
  };

  const projection = selectIndexedFiles({
    sources: new Map([[source.id, source]]),
    deletions: new Map(),
    edits: new Map([[source.id, forged]]),
    truncated: false,
  });

  assert.deepEqual(
    projection.files.map((file) => file.filename),
    ["file-14.png"],
  );
});

// ---------------------------------------------------------------------------
// The hook, through the seams the app really uses: `relayClient` for the live
// subscription and the Tauri command for history.
// ---------------------------------------------------------------------------

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});

before(() => {
  Object.assign(globalThis, {
    document: dom.window.document,
    HTMLElement: dom.window.HTMLElement,
    IS_REACT_ACT_ENVIRONMENT: true,
    Node: dom.window.Node,
    window: dom.window,
  });
});

afterEach(async () => {
  const { cleanup } = await import("@testing-library/react");
  cleanup();
  mock.restoreAll();
});

after(() => dom.window.close());

const CHANNEL = { id: "channel-a" };

/**
 * Render the hook with both relay seams counted. `live` decides what the
 * subscription does; `page` decides what one history page resolves to.
 */
async function renderFilesIndex({
  enabled = true,
  live = async () => () => {},
  page = async () => [],
} = {}) {
  const { renderHook } = await import("@testing-library/react");
  const calls = { subscribe: 0, fetch: 0 };
  // The seam the hook really uses: `subscribeLive`, which is the only relay
  // entry point that reports readiness and a later terminal CLOSED.
  mock.method(relayClient, "subscribeLive", async (_filter, onEvent) => {
    calls.subscribe += 1;
    return live(onEvent);
  });
  globalThis.window.__TAURI_INTERNALS__ = {
    invoke: async (command) => {
      if (command !== "get_channel_reconnect_repair") return null;
      calls.fetch += 1;
      return page();
    },
  };
  const view = renderHook((props) => useChannelFilesIndex(CHANNEL, props), {
    initialProps: enabled,
  });
  return { calls, view };
}

test("nothing reaches the relay until the Files tab is opened", async () => {
  const { waitFor } = await import("@testing-library/react");
  const { calls, view } = await renderFilesIndex({
    enabled: false,
    page: async () => [fileEvent({ index: 1 })],
  });

  // Give an effect that ignored the gate every chance to run.
  await new Promise((resolve) => setTimeout(resolve, 10));

  assert.equal(calls.subscribe, 0, "no live subscription before Files is open");
  assert.equal(calls.fetch, 0, "no history page before Files is open");

  view.rerender(true);

  await waitFor(() => {
    assert.equal(calls.subscribe, 1);
    assert.equal(calls.fetch, 1);
  });
});

test("a live failure over rows is a banner, with no rows it is the error state", async () => {
  const { waitFor } = await import("@testing-library/react");
  const withRows = await renderFilesIndex({
    live: async () => {
      throw new Error("relay socket refused");
    },
    page: async () => [fileEvent({ index: 1 })],
  });

  await waitFor(() =>
    assert.equal(withRows.view.result.current.files.length, 1),
  );
  assert.match(withRows.view.result.current.error ?? "", /live updates/i);
  assert.equal(
    withRows.view.result.current.isError,
    false,
    "rows are shown under the banner, never replaced by an error screen",
  );
  assert.equal(withRows.view.result.current.isLoading, false);

  const { cleanup } = await import("@testing-library/react");
  cleanup();
  mock.restoreAll();

  const empty = await renderFilesIndex({
    live: async () => {
      throw new Error("relay socket refused");
    },
    page: async () => [],
  });

  await waitFor(() => assert.equal(empty.view.result.current.isError, true));
  assert.equal(empty.view.result.current.files.length, 0);
});

test("loading stops as soon as there is something to say", async () => {
  const { act, waitFor } = await import("@testing-library/react");

  const failing = await renderFilesIndex({
    live: async () => {
      throw new Error("relay socket refused");
    },
    page: () => new Promise(() => {}),
  });

  await waitFor(() => assert.notEqual(failing.view.result.current.error, null));
  assert.equal(
    failing.view.result.current.isLoading,
    false,
    "a surfaced failure is not a loading state",
  );

  const { cleanup } = await import("@testing-library/react");
  cleanup();
  mock.restoreAll();

  let deliver;
  const streaming = await renderFilesIndex({
    live: async (onEvent) => {
      deliver = onEvent;
      return () => {};
    },
    page: () => new Promise(() => {}),
  });

  await waitFor(() => assert.ok(deliver));
  await act(async () => {
    deliver(fileEvent({ index: 2 }));
  });

  assert.equal(streaming.view.result.current.files.length, 1);
  assert.equal(
    streaming.view.result.current.isLoading,
    false,
    "rows are on screen, so the tab is not loading",
  );
});
