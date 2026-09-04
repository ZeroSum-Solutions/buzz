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

test("Clear stays offered so a moved or deleted source can be unbound", () => {
  // The dialog has no getter for the stored binding, so on re-open it knows of
  // none. Gating Clear on what it has seen would strand a dead binding.
  assert.equal(canClearPromptSource(false), true);
  assert.equal(canClearPromptSource(true), false);
});

test("a queued head and a failed enqueue read differently", () => {
  const queued = promptSourceStatusMessage({
    localUpdated: true,
    publish: "queued",
    relayMessage: null,
    path: "/Users/me/agent-prompts/pm.md",
    mappingError: null,
    prompt: "Ship it.",
  });
  const failed = promptSourceStatusMessage({
    localUpdated: true,
    publish: "failed:retention db locked",
    relayMessage: null,
    path: "/Users/me/agent-prompts/pm.md",
    mappingError: null,
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
    mappingError: null,
    prompt: null,
  });
  assert.match(message, /unlinked/);
});

test("a mapping the backend could not store is reported, not hidden", () => {
  const message = promptSourceStatusMessage({
    localUpdated: true,
    publish: "published",
    relayMessage: null,
    path: null,
    mappingError:
      "failed to read prompt-sources.json: Is a directory (os error 21)",
    prompt: "Ship it.",
  });
  assert.match(message, /reloaded/);
  assert.match(message, /not remembered/);
  assert.match(message, /prompt-sources\.json/);
});
