import assert from "node:assert/strict";
import { after, before, test } from "node:test";

import { JSDOM } from "jsdom";

// Regression coverage for the `about` branch of `profileLookupsEqual`
// (identity.ts) through its one production caller. `about` is what makes
// the mention selector's role line pick up an about-only kind-0 update
// (mentionSuggestionMapping.ts): the stabilized reference this hook returns
// must be RELEASED (a new object identity) when only `about` changes, or
// MessageRow's `prev.profiles === next.profiles` memo keeps rendering the
// stale bio. See identity.test.mjs for the field-by-field coverage of
// `profileLookupsEqual` itself.

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});

before(() => {
  Object.assign(globalThis, {
    document: dom.window.document,
    HTMLElement: dom.window.HTMLElement,
    IS_REACT_ACT_ENVIRONMENT: true,
    window: dom.window,
  });
});

after(() => dom.window.close());

const PUBKEY = "a".repeat(64);

function profiles(about) {
  return {
    [PUBKEY]: {
      displayName: "Ada",
      name: null,
      avatarUrl: null,
      about,
      nip05Handle: null,
      ownerPubkey: null,
      isAgent: true,
    },
  };
}

async function renderMessageProfiles(initialProps) {
  const { renderHook } = await import("@testing-library/react");
  const { useMessageProfiles } = await import("./useMessageProfiles.ts");

  return renderHook((props) => useMessageProfiles(props), { initialProps });
}

const baseProps = {
  channelMembers: undefined,
  currentProfile: undefined,
  currentPubkey: undefined,
  managedAgents: [],
  relayAgents: [],
};

test("releases the stabilized reference when only `about` changes", async () => {
  const { result, rerender } = await renderMessageProfiles({
    ...baseProps,
    profiles: profiles("Researcher — deep dives"),
  });

  const first = result.current;
  assert.equal(first[PUBKEY].about, "Researcher — deep dives");

  rerender({ ...baseProps, profiles: profiles("Now doing something else") });

  const second = result.current;
  assert.notEqual(
    second,
    first,
    "an about-only change must release the stabilized reference",
  );
  assert.equal(second[PUBKEY].about, "Now doing something else");
});

test("keeps the stabilized reference when no profile value actually changed", async () => {
  const { result, rerender } = await renderMessageProfiles({
    ...baseProps,
    profiles: profiles("Researcher — deep dives"),
  });

  const first = result.current;

  // A fresh `profiles` object with value-identical content — the shape a
  // re-keyed `users-batch` query produces on typing churn.
  rerender({ ...baseProps, profiles: profiles("Researcher — deep dives") });

  assert.equal(
    result.current,
    first,
    "a value-identical re-key must keep the stabilized reference",
  );
});
