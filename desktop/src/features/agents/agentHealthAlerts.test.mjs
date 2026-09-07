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

globalThis.window = {
  Notification: class MockNotification {
    static permission = "granted";
    constructor(title, options) {
      sentNotifications.push({ title, options });
    }
    close() {}
  },
  __TAURI_INTERNALS__: {
    invoke: (cmd) => {
      if (cmd === "sync_agent_health") {
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

const { syncAgentHealth } = await import("@/features/agents/agentHealthHooks");
const { ingestAgentHealthFrame } = await import(
  "@/shared/api/tauriManagedAgents"
);

test("deliversDesktopNotificationForAlertsOnSyncAndIngest", async () => {
  sentNotifications.length = 0;

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
});
