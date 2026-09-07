import assert from "node:assert/strict";
import test from "node:test";

let inFlightCount = 0;
let maxConcurrentInFlight = 0;
let callCount = 0;
let failFirstCall = false;
const receivedFrames = [];

globalThis.window = {
  Notification: class MockNotification {
    static permission = "granted";
    constructor() {}
    close() {}
  },
  __TAURI_INTERNALS__: {
    invoke: async (cmd, args) => {
      if (cmd === "ingest_agent_health_frame") {
        callCount++;
        inFlightCount++;
        maxConcurrentInFlight = Math.max(maxConcurrentInFlight, inFlightCount);

        // Simulate async execution
        await new Promise((resolve) => setTimeout(resolve, 5));

        inFlightCount--;

        if (failFirstCall && callCount === 1) {
          throw new Error("injected first-call failure");
        }

        receivedFrames.push(args?.frame);
        return { inserted: 1, alerts: [] };
      }
      return Promise.reject(new Error(`unmocked: ${cmd}`));
    },
    transformCallback: () => Math.random(),
  },
};

const { queueAgentHealthFrame, resetHealthQueuesForTest } = await import(
  "./observerRelayStore.ts"
);

test("emits 20 health frames and asserts serial execution and retry on failure", async () => {
  resetHealthQueuesForTest();
  inFlightCount = 0;
  maxConcurrentInFlight = 0;
  callCount = 0;
  failFirstCall = true;
  receivedFrames.length = 0;

  const agentPubkey = "agent_serial_test";

  // Emit 20 frames synchronously
  for (let i = 0; i < 20; i++) {
    queueAgentHealthFrame(agentPubkey, { index: i });
  }

  // Wait for queue to drain
  while (receivedFrames.length < 20) {
    await new Promise((resolve) => setTimeout(resolve, 10));
  }

  assert.equal(
    maxConcurrentInFlight,
    1,
    "at most 1 concurrent ingestAgentHealthFrame call must be in flight at a time",
  );
  assert.equal(
    callCount,
    21,
    "21 total calls expected: 1 failed call + 1 retry + 19 remaining calls",
  );
  assert.equal(receivedFrames.length, 20, "all 20 frames must be processed");
  assert.deepEqual(
    receivedFrames.map((f) => f.index),
    Array.from({ length: 20 }, (_, i) => i),
    "frames must be processed in order without losing the retried first frame",
  );
});
