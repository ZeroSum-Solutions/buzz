import assert from "node:assert/strict";
import { after, before, test } from "node:test";

import { JSDOM } from "jsdom";

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});

before(() => {
  Object.assign(globalThis, {
    document: dom.window.document,
    HTMLElement: dom.window.HTMLElement,
    IS_REACT_ACT_ENVIRONMENT: true,
    window: dom.window,
  });
});

after(() => dom.window.close());

const CHANNEL_ID = "channel-uuid";
const PUBKEY = "pubkey-hex";

/**
 * Drives the real hook against a fake relay: `signRelayEvent`,
 * `nip44EncryptToSelf` and `nip44DecryptFromSelf` go through the production
 * Tauri seam (stubbed at `window.__TAURI_INTERNALS__.invoke`), and
 * `relayClient.fetchEvents`/`publishEvent` are replaced so every assertion
 * below is about what the hook actually publishes.
 */
async function setup(options = {}) {
  const { act, cleanup, renderHook } = await import("@testing-library/react");
  const React = await import("react");
  const { QueryClient, QueryClientProvider } = await import(
    "@tanstack/react-query"
  );
  const { relayClient } = await import("@/shared/api/relayClient");
  const { useFileFolders } = await import("./useFileFolders.ts");

  const state = {
    /** Newest stored event, i.e. the relay's replaceable head. */
    head: null,
    published: [],
    fetches: 0,
    okMessages: options.okMessages ? [...options.okMessages] : [],
    fetchError: options.fetchError ?? null,
    plaintextByCiphertext: new Map(),
  };

  let signCounter = 0;
  const previousInternals = globalThis.window.__TAURI_INTERNALS__;
  globalThis.window.__TAURI_INTERNALS__ = {
    invoke: async (command, args) => {
      if (command === "sign_event") {
        signCounter += 1;
        return JSON.stringify({
          id: `signed-${signCounter}`,
          pubkey: PUBKEY,
          kind: args.kind,
          content: args.content,
          created_at: args.createdAt ?? 1,
          tags: args.tags,
          sig: "sig",
        });
      }
      if (command === "nip44_encrypt_to_self") {
        // Base64 stands in for NIP-44 here: it keeps the assertion that the
        // folder name never appears verbatim in the published content honest.
        const ciphertext = Buffer.from(args.plaintext, "utf8").toString(
          "base64",
        );
        state.plaintextByCiphertext.set(ciphertext, args.plaintext);
        return ciphertext;
      }
      if (command === "nip44_decrypt_from_self") {
        const plaintext = state.plaintextByCiphertext.get(args.ciphertext);
        if (plaintext === undefined) throw new Error("undecryptable");
        return plaintext;
      }
      throw new Error(`Unexpected Tauri command: ${command}`);
    },
  };

  const originalFetchEvents = relayClient.fetchEvents;
  const originalPublishEvent = relayClient.publishEvent;

  relayClient.fetchEvents = async (filter) => {
    state.fetches += 1;
    state.lastFilter = filter;
    if (state.fetchError) throw state.fetchError;
    return state.head ? [state.head] : [];
  };
  relayClient.publishEvent = async (event, _timeout, _fail, onOk) => {
    state.published.push(event);
    const okMessage = state.okMessages.shift() ?? "";
    // A superseded write is acknowledged but not stored.
    if (!/^inserted$/i.test(okMessage.trim()) && okMessage.trim().length > 0) {
      onOk?.(okMessage);
      return event;
    }
    state.head = event;
    onOk?.(okMessage);
    return event;
  };

  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0 } },
  });
  const wrapper = ({ children }) =>
    React.createElement(QueryClientProvider, { client: queryClient }, children);

  const rendered = renderHook(() => useFileFolders(CHANNEL_ID, PUBKEY), {
    wrapper,
  });

  async function settle() {
    await act(async () => {
      await Promise.resolve();
      await new Promise((resolve) => setTimeout(resolve, 0));
    });
  }
  await settle();

  async function mutate(action) {
    let error = null;
    await act(async () => {
      await action().catch((caught) => {
        error = caught;
      });
    });
    await settle();
    return error;
  }

  return {
    act,
    mutate,
    settle,
    state,
    result: rendered.result,
    teardown() {
      cleanup();
      queryClient.clear();
      relayClient.fetchEvents = originalFetchEvents;
      relayClient.publishEvent = originalPublishEvent;
      globalThis.window.__TAURI_INTERNALS__ = previousInternals;
    },
  };
}

function payloadOf(event) {
  return JSON.parse(Buffer.from(event.content, "base64").toString("utf8"));
}

test("the published folder event hides the channel id and encrypts its payload", async () => {
  const harness = await setup();
  try {
    await harness.mutate(() =>
      harness.result.current.createFolder("Design Docs", null),
    );

    assert.equal(harness.state.published.length, 1);
    const event = harness.state.published[0];
    const dTag = event.tags.find((t) => t[0] === "d")[1];
    assert.match(dTag, /^files-v2-[0-9a-f]{32}$/);
    assert.ok(!dTag.includes(CHANNEL_ID), "no raw channel id on the wire");
    assert.equal(event.tags.find((t) => t[0] === "t")[1], "file-folders");
    assert.ok(
      !event.content.includes("Design Docs"),
      "the folder name is never a plaintext tag or plaintext content",
    );
    assert.equal(payloadOf(event).folders[0].name, "Design Docs");

    // Exactly one query coordinate — no limit games, no cross-feature crowding.
    assert.deepEqual(harness.state.lastFilter["#d"], [dTag]);
    assert.equal(harness.state.lastFilter.limit, 1);
  } finally {
    harness.teardown();
  }
});

test("a whole-folder delete is one write and one consistent state", async () => {
  const harness = await setup();
  try {
    await harness.mutate(() =>
      harness.result.current.createFolder("Parent", null),
    );
    const parentId = harness.result.current.folders[0].id;
    await harness.mutate(() =>
      harness.result.current.createFolder("Child", parentId),
    );
    const childId = harness.result.current.folders.find(
      (folder) => folder.id !== parentId,
    ).id;

    const before = harness.state.published.length;
    await harness.mutate(() => harness.result.current.deleteFolder(parentId));

    assert.equal(
      harness.state.published.length - before,
      1,
      "delete-with-children must not be a multi-write cascade",
    );
    const payload = payloadOf(
      harness.state.published[harness.state.published.length - 1],
    );
    assert.deepEqual(
      payload.folders.map((folder) => [folder.id, folder.parent]),
      [[childId, null]],
      "the child reparents inside the same write instead of being orphaned",
    );
  } finally {
    harness.teardown();
  }
});

test("two concurrent drops into the same folder both survive", async () => {
  const harness = await setup();
  try {
    await harness.mutate(() =>
      harness.result.current.createFolder("Inbox", null),
    );
    const folderId = harness.result.current.folders[0].id;
    const keyA = `${"a".repeat(64)}:${"1".repeat(16)}`;
    const keyB = `${"b".repeat(64)}:${"2".repeat(16)}`;

    // Fired without awaiting the first: the queue is what keeps the second
    // from being built against the pre-first-drop head.
    await harness.act(async () => {
      const first = harness.result.current.assignFiles([keyA], folderId);
      const second = harness.result.current.assignFiles([keyB], folderId);
      await Promise.all([first, second]);
    });

    const payload = payloadOf(
      harness.state.published[harness.state.published.length - 1],
    );
    assert.deepEqual(payload.files, { [keyA]: folderId, [keyB]: folderId });
  } finally {
    harness.teardown();
  }
});

test("a bulk move is one publish, not one per file", async () => {
  const harness = await setup();
  try {
    await harness.mutate(() =>
      harness.result.current.createFolder("Bulk", null),
    );
    const folderId = harness.result.current.folders[0].id;
    const keys = Array.from(
      { length: 5 },
      (_, index) => `${String(index).repeat(64)}:${"c".repeat(16)}`,
    );

    const before = harness.state.published.length;
    await harness.mutate(() =>
      harness.result.current.assignFiles(keys, folderId),
    );

    assert.equal(harness.state.published.length - before, 1);
    const payload = payloadOf(
      harness.state.published[harness.state.published.length - 1],
    );
    assert.equal(Object.keys(payload.files).length, 5);
  } finally {
    harness.teardown();
  }
});

test("moving a folder under its own descendant is rejected at the hook seam", async () => {
  const harness = await setup();
  try {
    await harness.mutate(() => harness.result.current.createFolder("A", null));
    const idA = harness.result.current.folders[0].id;
    await harness.mutate(() => harness.result.current.createFolder("B", idA));
    const idB = harness.result.current.folders.find((f) => f.id !== idA).id;

    const before = harness.state.published.length;
    const rejection = await harness.mutate(() =>
      harness.result.current.moveFolder(idA, idB),
    );

    assert.ok(rejection, "the caller sees the failure");
    assert.equal(
      harness.state.published.length,
      before,
      "a cyclic move never reaches the relay",
    );
  } finally {
    harness.teardown();
  }
});

test("a duplicate: OK is retried against a fresh head, not reported as saved", async () => {
  const harness = await setup({
    okMessages: ["Inserted", "duplicate: exists"],
  });
  try {
    await harness.mutate(() =>
      harness.result.current.createFolder("First", null),
    );
    assert.equal(harness.state.published.length, 1);

    await harness.mutate(() =>
      harness.result.current.createFolder("Second", null),
    );

    assert.equal(
      harness.state.published.length,
      3,
      "the superseded write is replayed against the re-read head",
    );
    const payload = payloadOf(
      harness.state.published[harness.state.published.length - 1],
    );
    assert.deepEqual(
      payload.folders.map((folder) => folder.name).sort(),
      ["First", "Second"],
      "the retry merges onto the head instead of clobbering it",
    );
  } finally {
    harness.teardown();
  }
});

test("a persistently superseded write surfaces an error and mutates nothing", async () => {
  const harness = await setup({
    okMessages: ["duplicate: exists", "duplicate: exists"],
  });
  try {
    const rejection = await harness.mutate(() =>
      harness.result.current.createFolder("Never lands", null),
    );

    assert.match(
      rejection?.message ?? "",
      /Another device changed these folders/,
    );
    assert.equal(harness.result.current.folders.length, 0);
  } finally {
    harness.teardown();
  }
});

test("a failed folder read is an error state, not an authoritative empty one", async () => {
  const harness = await setup({ fetchError: new Error("relay unreachable") });
  try {
    await harness.settle();
    await harness.settle();
    assert.equal(harness.result.current.isError, true);
    assert.equal(
      harness.result.current.canMutate,
      false,
      "mutations stay disabled while the folder state is unknown",
    );
    assert.deepEqual(harness.result.current.folders, []);
  } finally {
    harness.teardown();
  }
});

test("an unreadable stored payload marks the state invalid and blocks mutation", async () => {
  const harness = await setup();
  try {
    harness.state.head = {
      id: "corrupt",
      pubkey: PUBKEY,
      kind: 30078,
      created_at: 10,
      tags: [["d", "files-v2-x"]],
      content: "enc-that-was-never-registered",
      sig: "sig",
    };
    await harness.mutate(() => harness.result.current.refetch());
    await harness.settle();

    assert.equal(harness.result.current.invalidReason, "decrypt-failed");
    assert.equal(harness.result.current.canMutate, false);

    const rejection = await harness.mutate(() =>
      harness.result.current.createFolder("Nope", null),
    );
    assert.match(rejection?.message ?? "", /could not be read/);
    assert.equal(harness.state.published.length, 0);
  } finally {
    harness.teardown();
  }
});

test("every folder write carries a created_at strictly newer than the head", async () => {
  const harness = await setup();
  try {
    await harness.mutate(() =>
      harness.result.current.createFolder("One", null),
    );
    harness.state.head = { ...harness.state.head, created_at: 4_000_000_000 };
    await harness.mutate(() => harness.result.current.refetch());
    await harness.settle();
    await harness.mutate(() =>
      harness.result.current.createFolder("Two", null),
    );

    const last = harness.state.published[harness.state.published.length - 1];
    assert.ok(
      last.created_at > 4_000_000_000,
      `expected a monotonic created_at, got ${last.created_at}`,
    );
  } finally {
    harness.teardown();
  }
});
