import assert from "node:assert/strict";
import test from "node:test";

import { rankMentionCandidates } from "./mentionRanking.ts";
import {
  MENTION_SUGGESTION_LIMIT,
  rankVisibleMentionCandidates,
  selectAgentProfileFallbackPubkeys,
} from "./mentionFallbackWindow.ts";

function agentCandidate(n, overrides = {}) {
  return {
    displayName: `Agent${String(n).padStart(3, "0")}`,
    isAgent: true,
    isMember: false,
    kind: "identity",
    pubkey: `${n.toString(16).padStart(4, "0")}`.padEnd(64, "0"),
    ...overrides,
  };
}

test("bounds the fallback batch to MENTION_SUGGESTION_LIMIT even with 150 mentionable agents", () => {
  // Production seam: useMentions.ts's agentProfilePubkeys calls exactly this
  // function on exactly the output of rankVisibleMentionCandidates. A
  // community with 150 mentionable agents and no caller-scoped `profiles`
  // must never turn into a 150-author get_users_batch request — the relay
  // clamps a limit-less filter to 100 rows, and a truncated page gets
  // cached as confirmed-missing (see the docblock on
  // selectAgentProfileFallbackPubkeys).
  const candidates = Array.from({ length: 150 }, (_, i) => agentCandidate(i));
  const visible = rankVisibleMentionCandidates(candidates, "", new Set());
  assert.equal(visible.length, MENTION_SUGGESTION_LIMIT);

  const pubkeys = selectAgentProfileFallbackPubkeys(visible, undefined);
  assert.equal(pubkeys.length, MENTION_SUGGESTION_LIMIT);
  // Every returned pubkey actually belongs to a candidate in the input —
  // never a synthesized or out-of-window pubkey.
  const visiblePubkeys = new Set(visible.map((r) => r.candidate.pubkey));
  for (const pubkey of pubkeys) {
    assert.ok(visiblePubkeys.has(pubkey));
  }
});

test("an agent past the first page is excluded from the bare-query window but resolves once the query ranks it into view", () => {
  const candidates = Array.from({ length: 150 }, (_, i) => agentCandidate(i));
  const lastCandidate = candidates[149];

  const bareVisible = rankVisibleMentionCandidates(candidates, "", new Set());
  const barePubkeys = selectAgentProfileFallbackPubkeys(bareVisible, undefined);
  assert.ok(
    !barePubkeys.includes(lastCandidate.pubkey),
    "the 150th agent should be outside the default-ranked first page",
  );

  // Narrowing the query to its exact display name ranks it first (an
  // exact-match label always scores ahead of a mere prefix or substring
  // match — see mentionRanking.ts's scoreMentionCandidateLabel) and brings
  // it into the window, so it gets its own bounded request.
  const narrowedVisible = rankVisibleMentionCandidates(
    candidates,
    lastCandidate.displayName,
    new Set(),
  );
  const narrowedPubkeys = selectAgentProfileFallbackPubkeys(
    narrowedVisible,
    undefined,
  );
  assert.ok(narrowedPubkeys.includes(lastCandidate.pubkey));
});

test("excludes agents whose about is already known", () => {
  const known = agentCandidate(1, { description: "Already known" });
  const unknown = agentCandidate(2);
  const visible = rankVisibleMentionCandidates([known, unknown], "", new Set());
  const pubkeys = selectAgentProfileFallbackPubkeys(visible, undefined);
  assert.deepEqual(pubkeys, [unknown.pubkey]);
});

test("excludes agents already covered by the caller's profiles prop", () => {
  const covered = agentCandidate(1);
  const uncovered = agentCandidate(2);
  const visible = rankVisibleMentionCandidates(
    [covered, uncovered],
    "",
    new Set(),
  );
  const pubkeys = selectAgentProfileFallbackPubkeys(visible, {
    [covered.pubkey]: {
      displayName: null,
      name: null,
      avatarUrl: null,
      about: "Known via the caller",
      nip05Handle: null,
      ownerPubkey: null,
    },
  });
  assert.deepEqual(pubkeys, [uncovered.pubkey]);
});

test("includes an agent whose lookup entry exists but carries no about — the mergeAgentNamesIntoProfiles shape", () => {
  // Production seam: mergeAgentNamesIntoProfiles (useChannelActivityTyping.ts)
  // synthesizes a profile-lookup entry for every managed/relay agent with
  // displayName / avatarUrl / nip05Handle / ownerPubkey / isAgent and NO
  // `about` key at all — a real entry, not a hypothetical one. Treating
  // entry presence as "about known" would suppress the fallback for exactly
  // the agents it exists to cover; the fix must key off `about` being
  // present, not the entry.
  const candidate = agentCandidate(1);
  const visible = rankVisibleMentionCandidates([candidate], "", new Set());
  const pubkeys = selectAgentProfileFallbackPubkeys(visible, {
    [candidate.pubkey]: {
      displayName: "Bumble",
      avatarUrl: null,
      nip05Handle: null,
      ownerPubkey: null,
      isAgent: true,
    },
  });
  assert.deepEqual(pubkeys, [candidate.pubkey]);
});

test("matches the caller's profiles lookup by normalized pubkey, like the mapper does", () => {
  const candidate = agentCandidate(1, {
    pubkey: agentCandidate(1).pubkey.toUpperCase(),
  });
  const visible = rankVisibleMentionCandidates([candidate], "", new Set());
  const pubkeys = selectAgentProfileFallbackPubkeys(visible, {
    [candidate.pubkey.toLowerCase()]: {
      displayName: null,
      name: null,
      avatarUrl: null,
      about: "Known via the caller, keyed lowercase",
      nip05Handle: null,
      ownerPubkey: null,
    },
  });
  assert.deepEqual(pubkeys, []);
});

test("excludes non-agent candidates even when their description is unknown", () => {
  const person = agentCandidate(1, { isAgent: false });
  const agent = agentCandidate(2);
  const visible = rankVisibleMentionCandidates([person, agent], "", new Set());
  const pubkeys = selectAgentProfileFallbackPubkeys(visible, undefined);
  assert.deepEqual(pubkeys, [agent.pubkey]);
});

test("dedupes repeated pubkeys in the ranked window", () => {
  const agent = agentCandidate(1);
  const ranked = rankMentionCandidates([agent, { ...agent }], "", new Set());
  const pubkeys = selectAgentProfileFallbackPubkeys(ranked, undefined);
  assert.deepEqual(pubkeys, [agent.pubkey]);
});
