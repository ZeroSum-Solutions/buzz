import assert from "node:assert/strict";
import test from "node:test";

import {
  canClearPromptSource,
  canReloadPromptSource,
  promptSourceStatusMessage,
} from "./promptSourceActions.ts";

test("Reload is disabled when no path is set", () => {
  assert.equal(canReloadPromptSource("", false), false);
  assert.equal(canReloadPromptSource("   ", false), false);
  assert.equal(
    canReloadPromptSource("/Users/me/agent-prompts/pm.md", false),
    true,
  );
});

test("Reload is disabled while a reload is in flight", () => {
  assert.equal(
    canReloadPromptSource("/Users/me/agent-prompts/pm.md", true),
    false,
  );
});

test("Clear is disabled until a source is stored", () => {
  assert.equal(canClearPromptSource(false, false), false);
  assert.equal(canClearPromptSource(true, false), true);
  assert.equal(canClearPromptSource(true, true), false);
});

test("a queued head and a failed enqueue read differently", () => {
  const queued = promptSourceStatusMessage({
    localUpdated: true,
    publish: "queued",
    relayMessage: null,
    path: "/Users/me/agent-prompts/pm.md",
    prompt: "Ship it.",
  });
  const failed = promptSourceStatusMessage({
    localUpdated: true,
    publish: "failed:retention db locked",
    relayMessage: null,
    path: "/Users/me/agent-prompts/pm.md",
    prompt: "Ship it.",
  });

  assert.match(queued, /queued/);
  assert.match(failed, /not queued/);
  assert.match(failed, /retention db locked/);
  assert.notEqual(queued, failed);
});

test("clearing reports that the instructions were left alone", () => {
  const message = promptSourceStatusMessage({
    localUpdated: false,
    publish: null,
    relayMessage: null,
    path: null,
    prompt: null,
  });
  assert.match(message, /unlinked/);
});
