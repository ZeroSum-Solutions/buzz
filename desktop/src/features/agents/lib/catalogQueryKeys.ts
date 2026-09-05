/**
 * Cache-key roots for the two community catalog queries.
 *
 * They live in a leaf module of their own, imported by nothing, because the
 * hooks that own those queries (`usePersonaCatalogRelay`, `useTeamCatalogRelay`)
 * pull in the relay client and its subscriptions. A writer that only needs to
 * say "this catalog is stale" must not drag that graph in behind it.
 *
 * The full key is `[root, communityId]`. A writer that has no community id —
 * the agent dialog is not given one — invalidates by root alone, which reaches
 * the same caches because only the active community's catalog queries are ever
 * mounted.
 */

/** Root segment of every community's persona-catalog cache key. */
export const PERSONA_CATALOG_QUERY_KEY_ROOT = "persona-catalog";

/** Root segment of every community's team-catalog cache key. */
export const TEAM_CATALOG_QUERY_KEY_ROOT = "team-catalog";
