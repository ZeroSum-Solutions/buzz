import assert from "node:assert/strict";
import test from "node:test";

import {
  canClearPromptSource,
  canReloadPromptSource,
  canResetPromptSources,
  PROMPT_SOURCE_RESET_WARNING,
  promptSourceHint,
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
  // The field seeds itself from the stored binding, but Clear is not gated on
  // that seed: a stale seed would otherwise strand a live binding. It is NOT
  // the recovery path for an unreadable sidecar — see the reset test below.
  assert.equal(canClearPromptSource(false), true);
  assert.equal(canClearPromptSource(true), false);
});

test("the resting hint names the bound file, or invites one", () => {
  const inSync = { path: "/Users/me/agent-prompts/pm.md", inSync: true };
  assert.match(promptSourceHint(inSync), /\/Users\/me\/agent-prompts\/pm\.md/);
  assert.match(promptSourceHint(null), /file in your home folder/i);
  assert.notEqual(
    promptSourceHint(inSync),
    promptSourceHint(null),
    "a bound agent must not read the same as an unbound one",
  );
});

test("a binding the definition has drifted from reads as out of sync", () => {
  // Another path wrote the instructions — a hand-typed edit, a definition
  // replaced from another device. Repeating "these instructions are loaded
  // from X" would state something false about the agent that is about to run.
  const drifted = { path: "/Users/me/agent-prompts/pm.md", inSync: false };
  assert.match(promptSourceHint(drifted), /no longer match/i);
  assert.match(
    promptSourceHint(drifted),
    /Reload[\s\S]*Clear/,
    "the out-of-sync sentence must name both ways back to a true state",
  );
});

test("the reset is offered only after a seed failed, and never while busy", () => {
  // Clear cannot recover an unreadable sidecar: removing one entry reads the
  // whole file first, so it fails exactly where the seed did.
  assert.equal(canResetPromptSources(false, false), false);
  assert.equal(canResetPromptSources(true, false), true);
  assert.equal(canResetPromptSources(true, true), false);
  assert.match(
    PROMPT_SOURCE_RESET_WARNING,
    /every agent/i,
    "the action is machine-wide and must say so before it is taken",
  );
});

test("a queued head and a failed enqueue read differently", () => {
  const queued = promptSourceStatusMessage({
    localUpdated: true,
    publish: "queued",
    relayMessage: null,
    binding: { path: "/Users/me/agent-prompts/pm.md", inSync: true },
    mappingError: null,
    prompt: "Ship it.",
  });
  const failed = promptSourceStatusMessage({
    localUpdated: true,
    publish: "failed:retention db locked",
    relayMessage: null,
    binding: { path: "/Users/me/agent-prompts/pm.md", inSync: true },
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
    binding: null,
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
    binding: null,
    mappingError:
      "failed to read prompt-sources.json: Is a directory (os error 21)",
    prompt: "Ship it.",
  });
  assert.match(message, /reloaded/);
  assert.match(message, /not remembered/);
  assert.match(message, /prompt-sources\.json/);
});

test("a surviving binding is described by its own inSync, not by the mapping failure", () => {
  // An unchanged reload of an already-bound file: the command saves the same
  // prompt the stored digest was made from, so the binding that survives the
  // failed sidecar write still matches these instructions. Saying otherwise
  // sends the operator to Reload a file that is already loaded.
  const matching = promptSourceStatusMessage({
    localUpdated: true,
    publish: "published",
    relayMessage: null,
    binding: { path: "/Users/me/agent-prompts/pm.md", inSync: true },
    mappingError:
      "failed to write prompt-sources.json: No space left on device",
    prompt: "Ship it.",
  });
  assert.match(matching, /not remembered/);
  assert.match(matching, /which matches these instructions/);
  assert.doesNotMatch(matching, /no longer matches/);

  // A binding to some other file, left standing by the same failure: that one
  // genuinely does not feed the agent any more, and still says so.
  const stale = promptSourceStatusMessage({
    localUpdated: true,
    publish: "published",
    relayMessage: null,
    binding: { path: "/Users/me/agent-prompts/old.md", inSync: false },
    mappingError:
      "failed to write prompt-sources.json: No space left on device",
    prompt: "Ship it.",
  });
  assert.match(stale, /no longer matches these instructions/);
});
