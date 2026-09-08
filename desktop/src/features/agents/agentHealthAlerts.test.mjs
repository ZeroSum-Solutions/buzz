// Binds the production notification-delivery wiring added in T17 step 10:
// syncAgentHealth (agentHealthHooks.ts) and ingestAgentHealthFrame
// (tauriManagedAgents.ts) must call sendDesktopNotification for every alert
// their Tauri command returns. Mocks only the Tauri IPC boundary
// (window.__TAURI_INTERNALS__.invoke) and the Notification constructor, then
// calls the real exported functions — deleting either "for (const alert of
// result.alerts) sendDesktopNotification(...)" loop fails this test.
import assert from "node:assert/strict";
import test from "node:test";

const sentNotifications = [];
const recordedAlerts = [];
let syncHealthShouldReject = false;
let syncHealthResultErrors = [];

globalThis.window = {
  Notification: class MockNotification {
    static permission = "granted";
    constructor(title, options) {
      sentNotifications.push({ title, options });
    }
    close() {}
  },
  __TAURI_INTERNALS__: {
    invoke: (cmd, args) => {
      if (cmd === "record_delivered_alerts") {
        recordedAlerts.push(...(args?.alerts ?? []));
        return Promise.resolve();
      }
      if (cmd === "get_agent_health_summary") {
        return Promise.resolve([]);
      }
      if (cmd === "sync_agent_health") {
        if (syncHealthShouldReject) {
          return Promise.reject(new Error("simulated sync failure"));
        }
        return Promise.resolve({
          inserted: 1,
          alerts: [
            {
              agent: "PM",
              rule: "parked_older_than_15_minutes",
              title: "PM",
              body: "PM has 3 saved messages waiting for 20 minutes",
            },
          ],
          errors: syncHealthResultErrors,
        });
      }
      if (cmd === "ingest_agent_health_frame") {
        return Promise.resolve({
          inserted: 1,
          alerts: [
            {
              agent: "Critic",
              rule: "needs_review",
              title: "Critic",
              body: "A Critic request needs your decision",
            },
          ],
        });
      }
      return Promise.reject(new Error(`unmocked Tauri command: ${cmd}`));
    },
    transformCallback: () => Math.random(),
  },
};

const { syncAgentHealth, fetchAgentHealthSummary, withTimeout } = await import(
  "@/features/agents/agentHealthHooks"
);
const { ingestAgentHealthFrame } = await import(
  "@/shared/api/tauriManagedAgents"
);

test("deliversDesktopNotificationForAlertsOnSyncAndIngest", async () => {
  sentNotifications.length = 0;
  recordedAlerts.length = 0;

  const syncResult = await syncAgentHealth("pm-pubkey");
  assert.equal(syncResult.alerts.length, 1);

  const ingestResult = await ingestAgentHealthFrame("critic-pubkey", {});
  assert.equal(ingestResult.alerts.length, 1);

  assert.equal(sentNotifications.length, 2);
  assert.equal(sentNotifications[0].title, "PM");
  assert.equal(
    sentNotifications[0].options.body,
    "PM has 3 saved messages waiting for 20 minutes",
  );
  assert.equal(sentNotifications[1].title, "Critic");
  assert.equal(
    sentNotifications[1].options.body,
    "A Critic request needs your decision",
  );

  // Both delivered alerts were recorded via delivery ack
  assert.equal(recordedAlerts.length, 2);
  assert.equal(recordedAlerts[0].rule, "parked_older_than_15_minutes");
  assert.equal(recordedAlerts[1].rule, "needs_review");
});

test("failedDesktopNotificationDoesNotAckDeliveredAlert", async () => {
  sentNotifications.length = 0;
  recordedAlerts.length = 0;

  globalThis.window.Notification.permission = "denied";
  try {
    const syncResult = await syncAgentHealth("pm-pubkey");
    assert.equal(syncResult.alerts.length, 1);
    // Notification was not sent
    assert.equal(sentNotifications.length, 0);
    // Delivery ack was not recorded
    assert.equal(recordedAlerts.length, 0);
  } finally {
    globalThis.window.Notification.permission = "granted";
  }
});

test("fetchAgentHealthSummaryDegradesOnSyncRejection", async () => {
  syncHealthShouldReject = true;
  try {
    const result = await fetchAgentHealthSummary();
    assert.equal(
      result.syncError,
      true,
      "syncError must be true when syncAgentHealth fails",
    );
    assert.deepEqual(result.summary24h, []);
    assert.deepEqual(result.summary7d, []);
  } finally {
    syncHealthShouldReject = false;
  }
});

// The exact gap T17 delta round 2 flagged: a RESOLVED sync_agent_health
// response can still carry per-agent errors the backend swallowed rather
// than aborting the whole sync for. A promise resolving is not the same as
// the sync having actually succeeded for every agent — this must still
// degrade the summary, not present stale data as fresh.
test("fetchAgentHealthSummaryDegradesOnResolvedResponseWithErrors", async () => {
  sentNotifications.length = 0;
  syncHealthResultErrors = [["agent-with-bad-ledger", "corrupt ledger"]];
  try {
    const result = await fetchAgentHealthSummary();
    assert.equal(
      result.syncError,
      true,
      "syncError must be true when a resolved response carries errors",
    );
    assert.deepEqual(
      result.syncErrorAgents,
      ["agent-with-bad-ledger"],
      "the affected agent id must be surfaced, not just a boolean",
    );
  } finally {
    syncHealthResultErrors = [];
  }
});

test("fetchAgentHealthSummaryStaysCleanOnResolvedResponseWithNoErrors", async () => {
  syncHealthResultErrors = [];
  const result = await fetchAgentHealthSummary();
  assert.equal(result.syncError, false);
  assert.deepEqual(result.syncErrorAgents, []);
});

// `withTimeout` is the exact mechanism `syncAgentHealth` now wraps
// notification delivery and the delivery-ack invoke in (T17 delta round 2):
// neither permission prompts, native delivery, nor the ack invoke can hang
// a sync indefinitely, because none of them are awaited directly anymore.
test("withTimeoutResolvesToTheRealValueWhenTheInnerPromiseSettlesInTime", async () => {
  const value = await withTimeout(
    Promise.resolve("real value"),
    1000,
    "fallback",
  );
  assert.equal(value, "real value");
});

test("withTimeoutFallsBackWhenTheInnerPromiseNeverSettles", async () => {
  const neverSettles = new Promise(() => {});
  const value = await withTimeout(neverSettles, 20, "fallback");
  assert.equal(
    value,
    "fallback",
    "a never-resolving promise must not hang the caller past the timeout",
  );
});

test("withTimeoutFallsBackWhenTheInnerPromiseRejects", async () => {
  const rejecting = Promise.reject(new Error("boom"));
  const value = await withTimeout(rejecting, 1000, "fallback");
  assert.equal(
    value,
    "fallback",
    "a rejection must resolve to the fallback, not propagate as a thrown error",
  );
});
