import assert from "node:assert/strict";
import test from "node:test";

/**
 * Commands the publication mapper is asked about answer from here, so the
 * mapper runs over the exact shape the backend serializes.
 */
let invoked = null;
// `@tauri-apps/api` reaches the bridge through `window`, so the stub needs a
// window to live on. No DOM is involved — these are pure mapper tests.
globalThis.window = globalThis;
globalThis.__TAURI_INTERNALS__ = {
  invoke: (cmd, payload) => {
    invoked = { cmd, payload };
    return Promise.resolve(publicationResponse);
  },
  transformCallback: () => 1,
};
let publicationResponse = null;

const { fromRawPersona, setPersonaShared, updatePersonaAndPublish } =
  await import("./tauriPersonas.ts");

function rawPersona(overrides = {}) {
  return {
    id: "persona-1",
    display_name: "Team Analyst",
    avatar_url: null,
    system_prompt: "You are Team Analyst.",
    runtime: null,
    model: null,
    provider: null,
    name_pool: [],
    is_builtin: false,
    is_active: true,
    source_team: null,
    env_vars: {},
    created_at: "2026-01-01T00:00:00.000Z",
    updated_at: "2026-01-01T00:00:00.000Z",
    ...overrides,
  };
}

test("fromRawPersona maps source_team to sourceTeam", () => {
  const persona = fromRawPersona(rawPersona({ source_team: "team-research" }));

  assert.equal(persona.sourceTeam, "team-research");
});

test("fromRawPersona maps authored description and defaults absence to null", () => {
  assert.equal(
    fromRawPersona(rawPersona({ description: "A careful analyst." }))
      .description,
    "A careful analyst.",
  );
  assert.equal(fromRawPersona(rawPersona()).description, null);
});

/**
 * The relay accepted the head and only the local "this head is synced"
 * bookkeeping failed. The backend deliberately reports that as success rather
 * than `Err` — the change is live locally and on the relay — so the mapper is
 * the only thing that can carry the reason to the UI. Dropping the field here
 * turns a reported failure into an invisible one, and the user sees the flush
 * loop republish a head that already went out with nothing to explain it.
 */
test("test_publication_result_carries_the_bookkeeping_failure", async () => {
  publicationResponse = {
    persona: rawPersona(),
    publicationStatus: "published",
    bookkeepingError: "retention db is locked",
  };

  const shared = await setPersonaShared("persona-1", true);
  assert.equal(invoked.cmd, "set_persona_shared");
  assert.equal(shared.bookkeepingError, "retention db is locked");
  assert.equal(shared.relayMessage, null);

  const saved = await updatePersonaAndPublish({
    id: "persona-1",
    displayName: "Team Analyst",
    systemPrompt: "You are Team Analyst.",
  });
  assert.equal(invoked.cmd, "update_persona_and_publish");
  assert.equal(saved.bookkeepingError, "retention db is locked");
});

test("test_publication_result_reports_no_bookkeeping_failure_as_null", async () => {
  publicationResponse = {
    persona: rawPersona(),
    publicationStatus: "queued",
    relayMessage: "relay unreachable",
  };

  const result = await setPersonaShared("persona-1", true);
  assert.equal(
    result.bookkeepingError,
    null,
    "absent must map to null, never undefined — the UI branches on it",
  );
  assert.equal(result.relayMessage, "relay unreachable");
});
