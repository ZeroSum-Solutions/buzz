import assert from "node:assert/strict";
import test from "node:test";

let inFlightCount = 0;
let maxConcurrentInFlight = 0;
let callCount = 0;
let failFirstCall = false;
let alwaysFailIngest = false;
let syncAgentHealthCalls = 0;
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

        if (alwaysFailIngest) {
          throw new Error("injected persistent failure");
        }
        if (failFirstCall && callCount === 1) {
          throw new Error("injected first-call failure");
        }

        receivedFrames.push(args?.frame);
        return { inserted: 1, alerts: [] };
      }
      if (cmd === "sync_agent_health") {
        syncAgentHealthCalls++;
        return { inserted: 0, alerts: [] };
      }
      return Promise.reject(new Error(`unmocked: ${cmd}`));
    },
    transformCallback: () => Math.random(),
  },
};

const {
  queueAgentHealthFrame,
  resetHealthQueuesForTest,
  resetAgentObserverStore,
  getHealthQueueState,
  MAX_HEALTH_QUEUE_PER_AGENT,
} = await import("./observerRelayStore.ts");

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

test("overflow beyond the per-agent cap is tracked, not silently unaccounted for", async () => {
  resetHealthQueuesForTest();
  failFirstCall = false;
  alwaysFailIngest = false;
  callCount = 0;
  receivedFrames.length = 0;

  const agentPubkey = "agent_overflow_test";
  const overBy = 5;
  // The very first frame is picked up synchronously by `processHealthQueue`
  // before the loop's next iteration (it never sits in `items`), so the
  // queue array itself only fills from the second frame onward: pushing
  // `MAX + overBy + 1` total frames overflows the array by exactly `overBy`.
  for (let i = 0; i < MAX_HEALTH_QUEUE_PER_AGENT + overBy + 1; i++) {
    queueAgentHealthFrame(agentPubkey, { index: i });
  }

  // The queue is capped: only MAX_HEALTH_QUEUE_PER_AGENT items ever sit in
  // it at once, but the drop count must still be observable.
  const state = getHealthQueueState(agentPubkey);
  assert.ok(state, "queue state must exist while items remain");
  assert.equal(
    state.overflowDropped,
    overBy,
    "every frame dropped for exceeding the cap must be counted",
  );

  const expectedDelivered = MAX_HEALTH_QUEUE_PER_AGENT + 1; // survivors: 1 dispatched immediately + MAX still queued
  while (receivedFrames.length < expectedDelivered) {
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  assert.equal(receivedFrames.length, expectedDelivered);
});

test("persistent ingest failure falls back to a full resync instead of silently dropping the frame", async () => {
  resetHealthQueuesForTest();
  callCount = 0;
  syncAgentHealthCalls = 0;
  alwaysFailIngest = true;
  receivedFrames.length = 0;

  const agentPubkey = "agent_persistent_failure_test";
  queueAgentHealthFrame(agentPubkey, { index: 0 });

  // Wait for the initial attempt + 1 retry (both fail), then the fallback.
  const deadline = Date.now() + 2000;
  while (syncAgentHealthCalls < 1 && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 10));
  }

  assert.equal(callCount, 2, "the frame must be attempted, then retried once");
  assert.equal(
    syncAgentHealthCalls,
    1,
    "exhausting retries must trigger exactly one full-resync fallback",
  );
  assert.equal(
    getHealthQueueState(agentPubkey),
    undefined,
    "the queue must not be left stuck in-flight after the terminal failure",
  );

  alwaysFailIngest = false;
});

test("a community reset mid-delivery stops the stale run instead of recursing into it", async () => {
  resetHealthQueuesForTest();
  callCount = 0;
  alwaysFailIngest = false;
  receivedFrames.length = 0;

  const agentPubkey = "agent_reset_mid_delivery_test";
  // Queue two frames: the first is in flight (5ms simulated latency) when
  // the reset below fires; the second must never be delivered afterward.
  queueAgentHealthFrame(agentPubkey, { index: 0 });
  queueAgentHealthFrame(agentPubkey, { index: 1 });

  // Fire the reset while the first item's delivery is still in flight.
  resetAgentObserverStore();

  // Give the in-flight delivery time to resolve and reach its `finally`.
  await new Promise((resolve) => setTimeout(resolve, 50));

  assert.equal(
    getHealthQueueState(agentPubkey),
    undefined,
    "the reset community's queue must be gone, not repopulated by a stale run",
  );
  assert.ok(
    receivedFrames.length <= 1,
    "the second, still-queued frame must never be delivered after the reset",
  );
});
