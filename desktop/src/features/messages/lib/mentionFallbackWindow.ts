import type { UserProfileLookup } from "@/features/profile/lib/identity";
import {
  type MentionCandidateForRanking,
  rankMentionCandidates,
  type RankedMentionCandidate,
} from "./mentionRanking";

/**
 * Cap on the mention picker's rendered suggestion list, and — via
 * {@link rankVisibleMentionCandidates} — on the agent profile fallback batch
 * in useMentions.ts's `agentProfilePubkeys`. The two must share one bound:
 * on a surface with no scoped `profiles` prop the mentionable set can be a
 * community's whole agent directory. `get_users_batch` sends a `kind: 0`
 * filter with no `limit` (desktop/src-tauri/src/commands/profile.rs), and
 * the relay clamps a limit-less filter to `DEFAULT_MAX_PAGE_LIMIT` — 1,000
 * rows (crates/buzz-db/src/store/event.rs), also the advertised NIP-11
 * `max_limit` (crates/buzz-relay/src/nip11.rs). This bound is not sized
 * against that 1,000-row ceiling; it is sized to match what the picker
 * renders, so the fallback never fetches an about for an agent the user
 * can't see. Requesting every unknown agent in the directory instead risks
 * a truncated page (past 1,000 mentionable agents) being cached as
 * confirmed-missing. Keeping the fallback to this same 50-row window keeps
 * every request an order of magnitude under that clamp.
 */
export const MENTION_SUGGESTION_LIMIT = 50;

/** Ranks `candidates` against `query` and cuts to {@link MENTION_SUGGESTION_LIMIT}. */
export function rankVisibleMentionCandidates<
  T extends MentionCandidateForRanking,
>(
  candidates: readonly T[],
  query: string,
  activePersonaIds: ReadonlySet<string>,
): RankedMentionCandidate<T>[] {
  return rankMentionCandidates(candidates, query, activePersonaIds).slice(
    0,
    MENTION_SUGGESTION_LIMIT,
  );
}

export type AgentProfileFallbackCandidate = MentionCandidateForRanking & {
  description?: string | null;
};

/**
 * Agent pubkeys whose kind-0 `about` is not already known — neither
 * resolved at candidate-build time (search results) nor present in the
 * caller's profile lookup — drawn ONLY from the already-ranked, already-
 * bounded visible window (`rankedVisibleCandidates`), never the full
 * mentionable-agent set. Batch-resolving just this window lets the selector
 * show a role line even for agents who haven't authored anything in the
 * loaded timeline, without the unbounded-batch failure mode
 * {@link MENTION_SUGGESTION_LIMIT} documents.
 */
export function selectAgentProfileFallbackPubkeys<
  T extends AgentProfileFallbackCandidate,
>(
  rankedVisibleCandidates: readonly RankedMentionCandidate<T>[],
  profiles: UserProfileLookup | undefined,
): string[] {
  return [
    ...new Set(
      rankedVisibleCandidates
        .filter(
          ({ candidate }) =>
            candidate.isAgent &&
            candidate.pubkey &&
            candidate.description === undefined &&
            !profiles?.[candidate.pubkey],
        )
        .map(({ candidate }) => candidate.pubkey as string),
    ),
  ];
}
