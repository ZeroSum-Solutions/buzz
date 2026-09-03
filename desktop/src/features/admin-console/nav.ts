/**
 * Pure visibility logic for the Settings → Relay admin nav entry.
 *
 * Kept free of React and IO so the gate decision is unit-testable in
 * isolation; `hooks.ts` resolves the origin source that feeds it.
 */

/** Where the admin origin came from for the active identity. */
export type AdminOriginSource = "saved" | "advertised" | "none";

export type RelayAdminNavResolution = {
  originSource: AdminOriginSource;
};

/**
 * Decide whether the Relay admin nav entry is visible.
 *
 * - No origin (neither saved-manual nor advertised) → hidden. Ordinary members
 *   never see a dead entry.
 * - A saved manual origin always shows the entry: the Advanced affordance that
 *   edits/clears the origin lives inside the surface, so hiding it would lock a
 *   user out of fixing a bad saved URL.
 * - An advertised origin shows the entry so the operator can open Relay admin
 *   and confirm the pre-filled origin. The advertised value is NOT probed to
 *   decide visibility — probing an untrusted relay-advertised origin would send
 *   a signed NIP-98 credential to an attacker-chosen destination. Nothing
 *   contacts the advertised origin until the operator explicitly saves it.
 */
export function shouldShowRelayAdminNav(res: RelayAdminNavResolution): boolean {
  return res.originSource !== "none";
}
