// The Files index against a real `RelayClient`.
//
// History: the controller took its live subscription through a call that
// resolves for every outcome. A relay that answered the REQ with
// `CLOSED error: too many subscriptions` — the likely answer once this branch
// adds a per-channel subscription on top of the ones a channel view already
// opens — still produced an unsubscribe closure, so the tab recorded a refusal
// as an open subscription: no banner, no Retry, and no further event for the
// life of the channel view. The same blindness covered a terminal CLOSED that
// arrives after readiness, which deletes the subscription inside the client.
//
// A stub that throws cannot reproduce either, so every test here drives the
// production wiring (`subscribeChannelFilesLive`) against a `RelayClient` and
// feeds it real relay frames through `handleWsMessage`.
import assert from "node:assert/strict";
import test from "node:test";

const pendingTimers = new Map();
let nextTimerId = 1;
const sentFrames = [];

globalThis.window = {
  setTimeout: (fn, ms) => {
    const id = nextTimerId++;
    pendingTimers.set(id, { fn, delay: ms });
    return id;
  },
  clearTimeout: (id) => pendingTimers.delete(id),
  __TAURI_INTERNALS__: {
    invoke: async (command, args) => {
      if (command === "plugin:websocket|send") {
        sentFrames.push(JSON.parse(args.message.data));
      }
    },
  },
};

const { RelayClient } = await import("@/shared/api/relayClientSession");
const { MAX_ERROR_DETAIL_LENGTH, createChannelFilesIndexController } =
  await import("./channelFilesBackfill.ts");
const { subscribeChannelFilesLive } = await import(
  "./channelFilesLiveSubscription.ts"
);

const CHANNEL = "channel-1";
const SHA = "d".repeat(64);

function hexId(seed) {
  return seed.toString(16).padStart(64, "0");
}

/** A well-formed attachment-bearing channel event. */
function fileEvent(index, overrides = {}) {
  return {
    id: hexId(index),
    pubkey: "a".repeat(64),
    created_at: 1_000 + index,
    kind: 40002,
    content: `attachment ${index}`,
    sig: "sig",
    tags: [
      ["h", CHANNEL],
      [
        "imeta",
        `url https://relay.example/media/file-${index}.png`,
        "m image/png",
        `x ${SHA}`,
      ],
    ],
    ...overrides,
  };
}

function reset() {
  pendingTimers.clear();
  nextTimerId = 1;
  sentFrames.length = 0;
}

/** A client that believes it is connected, so no socket setup is needed. */
function connectedClient() {
  const client = new RelayClient();
  client.wsId = 7;
  return client;
}

/** Feed one raw relay frame through the real inbound dispatch. */
function deliver(client, frame) {
  return client.handleWsMessage(
    { type: "Text", data: JSON.stringify(frame) },
    client.connectionGeneration,
  );
}

/** Fire every timer the client has scheduled (the event-batch flush). */
function runTimers() {
  for (const [id, { fn }] of Array.from(pendingTimers.entries())) {
    pendingTimers.delete(id);
    fn();
  }
}

/** Wait for the REQ the controller sends, and return its subscription id. */
async function reqSubId() {
  for (let attempt = 0; attempt < 50; attempt += 1) {
    const req = sentFrames.find((frame) => frame[0] === "REQ");
    if (req) return req[1];
    await Promise.resolve();
  }
  assert.fail("the controller never sent a REQ");
}

/** The controller, wired to `client` exactly the way the hook wires it. */
function filesController(client, onChange) {
  return createChannelFilesIndexController({
    channelId: CHANNEL,
    subscribeLive: (id, onEvent, handlers) =>
      subscribeChannelFilesLive(client, id, onEvent, handlers),
    fetchPage: async () => [],
    onChange,
  });
}

test("a REQ the relay refuses is not recorded as a live subscription", async () => {
  reset();
  const client = connectedClient();
  const controller = filesController(client);

  const started = controller.start();
  const subId = await reqSubId();
  await deliver(client, ["CLOSED", subId, "error: too many subscriptions"]);
  await started;

  const snapshot = controller.snapshot();
  assert.equal(
    snapshot.liveConnected,
    false,
    "the relay refused the REQ; nothing is following the channel",
  );
  assert.match(snapshot.error ?? "", /not receiving live updates/i);
  assert.match(snapshot.error ?? "", /too many subscriptions/);
  assert.equal(
    snapshot.liveTerminal,
    false,
    "one refusal leaves attempts, so Retry is still offered",
  );
});

test("the relay's CLOSED message is capped before it reaches the banner", async () => {
  reset();
  const client = connectedClient();
  const controller = filesController(client);

  const started = controller.start();
  const subId = await reqSubId();
  await deliver(client, ["CLOSED", subId, `restricted: ${"n".repeat(20_000)}`]);
  await started;

  const error = controller.snapshot().error ?? "";
  assert.ok(
    error.length < MAX_ERROR_DETAIL_LENGTH + 120,
    `the relay controls this string; it must be capped, got ${error.length}`,
  );
});

test("a terminal CLOSED after readiness takes the live subscription down", async () => {
  reset();
  const client = connectedClient();
  const controller = filesController(client);

  const started = controller.start();
  const subId = await reqSubId();
  await deliver(client, ["EOSE", subId]);
  await started;

  assert.equal(
    controller.snapshot().liveConnected,
    true,
    "EOSE means the subscription is open",
  );
  assert.equal(controller.snapshot().error, null);

  await deliver(client, ["CLOSED", subId, "restricted: not a member"]);

  const snapshot = controller.snapshot();
  assert.equal(
    snapshot.liveConnected,
    false,
    "the client deleted the subscription; the tab must stop claiming it is live",
  );
  assert.match(snapshot.error ?? "", /restricted: not a member/);
});

test("a retryable CLOSED leaves the client's own recovery alone", async () => {
  reset();
  const client = connectedClient();
  const controller = filesController(client);

  const started = controller.start();
  const subId = await reqSubId();
  await deliver(client, ["EOSE", subId]);
  await started;

  await deliver(client, ["CLOSED", subId, "auth-required: re-authenticate"]);

  assert.equal(
    controller.snapshot().liveConnected,
    true,
    "a retryable CLOSED is re-sent by the relay client; only a terminal one ends it",
  );
});

test("a malformed live event does not discard the rest of the batch", async () => {
  reset();
  const client = connectedClient();
  const controller = filesController(client);

  const started = controller.start();
  const subId = await reqSubId();
  await deliver(client, ["EOSE", subId]);
  await started;

  // `pubkey` is a number. The relay's EVENT payload is `JSON.parse`d and cast,
  // so this is a shape the client will hand straight to the index.
  await deliver(client, ["EVENT", subId, fileEvent(1, { pubkey: 42 })]);
  await deliver(client, ["EVENT", subId, fileEvent(2)]);
  runTimers();

  const snapshot = controller.snapshot();
  assert.ok(
    snapshot.index.sources.has(hexId(2)),
    "the event after the malformed one must still reach the index — the " +
      "dispatcher flushes one buffer for every subscription on the socket",
  );
  assert.equal(
    snapshot.index.sources.get(hexId(1))?.pubkey,
    "",
    "the malformed field is dropped, not read as a string",
  );
});

test("a live event delivered after dispose is ignored", async () => {
  reset();
  const client = connectedClient();
  const controller = filesController(client);

  const started = controller.start();
  const subId = await reqSubId();
  await deliver(client, ["EOSE", subId]);
  await started;
  await controller.dispose();

  await deliver(client, ["EVENT", subId, fileEvent(3)]);
  runTimers();

  assert.equal(controller.snapshot().index.sources.size, 0);
});
