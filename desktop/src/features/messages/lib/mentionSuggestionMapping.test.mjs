import assert from "node:assert/strict";
import test from "node:test";

import { mapMentionCandidateToSuggestion } from "./mentionSuggestionMapping.ts";

const OWNER = "a".repeat(64);
const AGENT_PUBKEY = "b".repeat(64);

function candidate(overrides = {}) {
  return {
    kind: "identity",
    pubkey: AGENT_PUBKEY,
    isAgent: true,
    isMember: true,
    ownerPubkey: OWNER,
    ...overrides,
  };
}

function agentCandidate(overrides = {}) {
  return candidate(overrides);
}

function suggestion(overrides = {}, agentProvenanceReady = true) {
  return mapMentionCandidateToSuggestion({
    agentProvenanceReady,
    candidate: candidate(overrides),
    currentPubkey: OWNER,
    label: "Carl",
  });
}

function profileSummary(about) {
  return {
    displayName: "Bumble",
    name: null,
    avatarUrl: null,
    about,
    nip05Handle: null,
    ownerPubkey: null,
    isAgent: true,
  };
}

test("labels Desktop-managed agent identities as managed here", () => {
  assert.equal(
    suggestion({ isManagedAgent: true }).agentProvenance,
    "managed-here",
  );
});

test("labels same-owner relay agent identities as managed elsewhere", () => {
  assert.equal(suggestion().agentProvenance, "managed-elsewhere");
});

test("fails closed while the managed-agent directory is unresolved", () => {
  assert.equal(
    suggestion({ isManagedAgent: true }, false).agentProvenance,
    undefined,
  );
  assert.equal(suggestion({}, false).agentProvenance, undefined);
});

test("does not attribute another owner's agent to a device", () => {
  assert.equal(
    suggestion({ ownerPubkey: "c".repeat(64) }).agentProvenance,
    undefined,
  );
});

test("does not attribute people or personas to a device", () => {
  assert.equal(suggestion({ isAgent: false }).agentProvenance, undefined);
  assert.equal(
    suggestion({ kind: "persona", pubkey: undefined }).agentProvenance,
    undefined,
  );
});

test("agent description comes from the candidate when resolved at build time", () => {
  const result = mapMentionCandidateToSuggestion({
    agentProvenanceReady: true,
    candidate: agentCandidate({ description: "Researcher — deep dives" }),
    label: "Bumble",
    profiles: { [AGENT_PUBKEY]: profileSummary("stale profile about") },
  });

  assert.equal(result.description, "Researcher — deep dives");
});

test("agent description falls back to the profile lookup's about", () => {
  const result = mapMentionCandidateToSuggestion({
    agentProvenanceReady: true,
    candidate: agentCandidate(),
    label: "Bumble",
    profiles: { [AGENT_PUBKEY]: profileSummary("Researcher — deep dives") },
  });

  assert.equal(result.description, "Researcher — deep dives");
});

test("agent description is null when about is missing everywhere", () => {
  const result = mapMentionCandidateToSuggestion({
    agentProvenanceReady: true,
    candidate: agentCandidate(),
    label: "Bumble",
    profiles: { [AGENT_PUBKEY]: profileSummary(null) },
  });

  assert.equal(result.description, null);
});

test("non-agent suggestions never carry a description", () => {
  const result = mapMentionCandidateToSuggestion({
    agentProvenanceReady: true,
    candidate: agentCandidate({ isAgent: false }),
    label: "Alice",
    profiles: { [AGENT_PUBKEY]: profileSummary("A human bio") },
  });

  assert.equal(result.description, null);
});

test("multi-line about collapses to a single trimmed line", () => {
  const result = mapMentionCandidateToSuggestion({
    agentProvenanceReady: true,
    candidate: agentCandidate({
      description: "  Writer bee.\nDrafts docs\n\tand posts.  ",
    }),
    label: "Honey",
  });

  assert.equal(result.description, "Writer bee. Drafts docs and posts.");
});

test("whitespace-only about degrades to null (name-only row)", () => {
  const result = mapMentionCandidateToSuggestion({
    agentProvenanceReady: true,
    candidate: agentCandidate({ description: "   \n  " }),
    label: "Fizz",
  });

  assert.equal(result.description, null);
});

test("an unbounded about is capped to 120 graphemes with an ellipsis", () => {
  // A plausible non-hostile case: an agent whose `about` is its full system
  // prompt. Also stands in for the relay's 256 KiB kind-0 content ceiling —
  // this suite doesn't build a string that large, it proves the cap applies
  // to any input longer than the bound, regardless of size.
  const hugeAbout = "S".repeat(200_000);

  const result = mapMentionCandidateToSuggestion({
    agentProvenanceReady: true,
    candidate: agentCandidate({ description: hugeAbout }),
    label: "Codex",
  });

  assert.equal(result.description?.length, 120);
  assert.ok(result.description?.endsWith("…"));
  assert.equal(result.description, `${"S".repeat(119)}…`);
});

test("an about exactly at the cap is left untouched", () => {
  const exact = "A".repeat(120);

  const result = mapMentionCandidateToSuggestion({
    agentProvenanceReady: true,
    candidate: agentCandidate({ description: exact }),
    label: "Codex",
  });

  assert.equal(result.description, exact);
});

test("description length cap counts graphemes, not UTF-16 code units", () => {
  // Each family emoji is one grapheme cluster spanning multiple UTF-16 code
  // units (ZWJ sequence) — a code-unit-based cap would truncate mid-cluster.
  const family = "\u{1F468}‍\u{1F469}‍\u{1F467}‍\u{1F466}";
  const about = family.repeat(130);

  const result = mapMentionCandidateToSuggestion({
    agentProvenanceReady: true,
    candidate: agentCandidate({ description: about }),
    label: "Codex",
  });

  assert.ok(result.description);
  const graphemeCount = Array.from(
    new Intl.Segmenter(undefined, { granularity: "grapheme" }).segment(
      result.description,
    ),
  ).length;
  assert.equal(graphemeCount, 120);
  assert.ok(result.description.endsWith("…"));
});
