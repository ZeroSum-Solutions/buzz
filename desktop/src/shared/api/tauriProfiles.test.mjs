import assert from "node:assert/strict";
import { test } from "node:test";
import { JSDOM } from "jsdom";

test("profile fetching partitions large file-author sets into native-sized requests", async () => {
  const dom = new JSDOM("", { url: "http://localhost" });
  globalThis.window = dom.window;
  globalThis.localStorage = dom.window.localStorage;
  const requests = [];
  const internals = {
    invoke: async (command, payload) => {
      assert.equal(command, "get_users_batch");
      requests.push(payload.pubkeys);
      assert.ok(payload.pubkeys.length <= 256, "native batch limit exceeded");
      return { profiles: {}, missing: payload.pubkeys };
    },
  };
  globalThis.__TAURI_INTERNALS__ = internals;
  dom.window.__TAURI_INTERNALS__ = internals;
  try {
    const { getUsersBatch } = await import("./tauriProfiles.ts");
    const pubkeys = Array.from({ length: 513 }, (_, i) =>
      i.toString(16).padStart(64, "0"),
    );
    const result = await getUsersBatch(pubkeys);
    assert.deepEqual(
      requests.map((r) => r.length),
      [256, 256, 1],
    );
    assert.deepEqual(result.missing, pubkeys);
  } finally {
    dom.window.close();
    delete globalThis.__TAURI_INTERNALS__;
    delete globalThis.window;
    delete globalThis.localStorage;
  }
});
