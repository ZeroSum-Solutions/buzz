import assert from "node:assert/strict";
import test from "node:test";

import { HEALTH_FRAME_KINDS, parseHealthFrame } from "./agentHealthFrames.ts";

test("parsesOnlyHealthKinds", () => {
  const expectedKinds = [
    "turn_failed",
    "batch_parked",
    "batch_replayed",
    "batch_needs_review",
    "agent_paused",
    "agent_resumed",
    "breaker_opened",
    "breaker_closed",
    "relay_reconnected",
  ];

  assert.equal(HEALTH_FRAME_KINDS.size, 9);
  for (const kind of expectedKinds) {
    assert.ok(
      HEALTH_FRAME_KINDS.has(kind),
      `HEALTH_FRAME_KINDS must include ${kind}`,
    );
  }

  const at = "2026-09-06T16:00:00Z";

  // All nine kinds parse when valid
  for (const kind of expectedKinds) {
    const parsed = parseHealthFrame({
      at,
      kind,
      batchId: "b-1",
      channelId: "c-1",
      class: "error-class",
      payload: { extra: true },
    });
    assert.ok(parsed, `expected parseHealthFrame to succeed for kind ${kind}`);
    assert.equal(parsed.at, at);
    assert.equal(parsed.kind, kind);
    assert.equal(parsed.batchId, "b-1");
    assert.equal(parsed.channelId, "c-1");
    assert.equal(parsed.class, "error-class");
    assert.deepEqual(parsed.payload, { extra: true });
  }

  // Parses when fields are embedded in ObserverEvent payload
  const observerEvent = {
    seq: 42,
    timestamp: "2026-09-06T16:00:00.000Z",
    kind: "batch_parked",
    channelId: "c-observer",
    payload: {
      at,
      batch_id: "batch-xyz",
      channel_id: "c-observer",
      reason: "retries_exhausted",
      started: true,
      events: 3,
    },
  };
  const parsedObserver = parseHealthFrame(observerEvent);
  assert.ok(parsedObserver);
  assert.equal(parsedObserver.at, at);
  assert.equal(parsedObserver.kind, "batch_parked");
  assert.equal(parsedObserver.batchId, "batch-xyz");
  assert.equal(parsedObserver.channelId, "c-observer");
  assert.deepEqual(parsedObserver.payload, observerEvent.payload);

  // Non-health kinds must return null
  const nonHealthKinds = [
    "turn_started",
    "turn_activity",
    "turn_finished",
    "session_config_captured",
    "control_result",
    "managed_agent_runtime_lifecycle",
    "agent_management_request",
    "message",
    "random_unrelated_event",
  ];

  for (const kind of nonHealthKinds) {
    assert.equal(
      parseHealthFrame({ at, kind }),
      null,
      `expected non-health kind ${kind} to return null`,
    );
  }
});

test("rejectsFrameWithoutAt", () => {
  const kind = "turn_failed";

  // Missing at
  assert.equal(parseHealthFrame({ kind }), null);
  assert.equal(parseHealthFrame({ kind, payload: {} }), null);

  // Non-string at
  assert.equal(parseHealthFrame({ kind, at: 123456789 }), null);
  assert.equal(parseHealthFrame({ kind, at: null }), null);
  assert.equal(parseHealthFrame({ kind, at: undefined }), null);
  assert.equal(parseHealthFrame({ kind, at: true }), null);
  assert.equal(parseHealthFrame({ kind, at: {} }), null);

  // Empty string at
  assert.equal(parseHealthFrame({ kind, at: "" }), null);
  assert.equal(parseHealthFrame({ kind, payload: { at: "" } }), null);

  // Non-string at in payload
  assert.equal(parseHealthFrame({ kind, payload: { at: 12345 } }), null);
  assert.equal(parseHealthFrame({ kind, payload: { at: null } }), null);

  // Null/undefined/non-object event
  assert.equal(parseHealthFrame(null), null);
  assert.equal(parseHealthFrame(undefined), null);
  assert.equal(parseHealthFrame("invalid"), null);
  assert.equal(parseHealthFrame(123), null);
});
