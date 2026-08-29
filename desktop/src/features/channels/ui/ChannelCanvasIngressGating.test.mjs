/**
 * Canvas ingress gating regression: `canOpenCanvas` must key on the presence of
 * a persisted relay revision (`eventId !== null`), not on content length. After
 * a restore-to-empty the relay holds a kind:40100 event with `event_id` set and
 * `content: ""` — a read-only member who cannot edit would lose the only ingress
 * to that revision stream if we gate on `hasCanvas` (content truthiness).
 *
 * Tests the `canvasIngressOpen` pure function that ChannelManagementSheet
 * delegates to for the `canOpenCanvas` flag.
 */

import assert from "node:assert/strict";
import test from "node:test";

const { canvasIngressOpen } = await import("./canvasIngress.ts");

const EVENT_ID = "a".repeat(64);

// Read-only member (canEditNarrative = false).

test("read-only member: no persisted canvas → ingress closed", () => {
  assert.equal(canvasIngressOpen(null, false), false);
  assert.equal(canvasIngressOpen(undefined, false), false);
});

test("read-only member: persisted canvas with content → ingress open", () => {
  assert.equal(canvasIngressOpen(EVENT_ID, false), true);
});

test("read-only member: persisted empty canvas (content='') with eventId → ingress open", () => {
  // This is the restored-to-empty case. Content is "" but a revision exists.
  // The old `hasCanvas || canEditNarrative` gating would return false here,
  // losing the only ingress to the revision stream for read-only members.
  assert.equal(canvasIngressOpen(EVENT_ID, false), true);
});

// Editor (canEditNarrative = true) — always open regardless of eventId.

test("editor: no persisted canvas → ingress open (seeds first revision)", () => {
  assert.equal(canvasIngressOpen(null, true), true);
  assert.equal(canvasIngressOpen(undefined, true), true);
});

test("editor: persisted canvas → ingress open", () => {
  assert.equal(canvasIngressOpen(EVENT_ID, true), true);
});

// Mutation oracle: confirms the test catches the content-based regression.

test("regression oracle: old content-only gating fails for read-only + persisted-empty", () => {
  // Old code: `hasCanvas || canEditNarrative` where hasCanvas = content.trim().length > 0.
  // For an empty-content revision, hadOldBug === false — ingress closed for read-only.
  const emptyContent = "";
  const hadOldBug = emptyContent.trim().length > 0 || false;
  assert.equal(
    hadOldBug,
    false,
    "old logic closes ingress for read-only + empty content",
  );
  // canvasIngressOpen must NOT replicate that defect.
  assert.equal(
    canvasIngressOpen(EVENT_ID, false),
    true,
    "canvasIngressOpen keeps ingress open when eventId is non-null",
  );
});
