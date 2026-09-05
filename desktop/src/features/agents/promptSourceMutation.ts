import { useMutation, useQueryClient } from "@tanstack/react-query";

import {
  PERSONA_CATALOG_QUERY_KEY_ROOT,
  TEAM_CATALOG_QUERY_KEY_ROOT,
} from "@/features/agents/lib/catalogQueryKeys";
import {
  type PromptSourceResult,
  setPromptSourceAndReload,
} from "@/shared/api/tauriPersonas";
import { managedAgentsQueryKey, personasQueryKey } from "./hooks";

/** What one Reload or Clear submits. */
export type SetPromptSourceInput = {
  definitionId: string;
  /** `null` clears the binding and leaves the stored instructions alone. */
  path: string | null;
};

/**
 * Reload (or unbind) an agent's prompt file, and refresh the caches the reload
 * just invalidated.
 *
 * A reload writes the definition's `system_prompt` on disk through the ordinary
 * persona update path, so it invalidates what a typed edit invalidates — and
 * the backend command emits no `agents-data-changed`, so nothing else will.
 * Without this the cached persona keeps the pre-reload prompt for `staleTime`,
 * the next `openEdit` seeds the dialog from that stale copy, and saving any
 * unrelated field resubmits the old instructions over the new ones.
 *
 * The catalog caches are invalidated too, because a reload is a
 * save-**and-publish**: it awaits the relay through `publish_prepared_persona`
 * and the backend refreshes the affected team catalog heads, exactly like
 * `useUpdatePersonaAndPublishMutation`. The live 30175 subscription usually
 * covers this, but its `subscribeLive` failure path only logs, so a device
 * whose subscription never established would show the pre-reload prompt in the
 * community catalog until the 20-minute persona poll.
 *
 * Invalidated by key root rather than by `(root, communityId)`: this mutation
 * runs inside the agent dialog, which is not given a community id, and only the
 * active community's catalog queries are mounted, so the prefix reaches exactly
 * the same caches.
 *
 * Invalidating on settle rather than on success is deliberate: a reload that
 * reports `mappingError`, a failed publish, or even a rejected path may still
 * have written the prompt, and the caches must not be left behind on any of
 * those branches.
 */
export function useSetPromptSourceMutation() {
  const queryClient = useQueryClient();

  return useMutation<PromptSourceResult, Error, SetPromptSourceInput>({
    mutationFn: ({ definitionId, path }) =>
      setPromptSourceAndReload(definitionId, path),
    onSettled: async () => {
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: personasQueryKey }),
        queryClient.invalidateQueries({ queryKey: managedAgentsQueryKey }),
        queryClient.invalidateQueries({
          queryKey: [PERSONA_CATALOG_QUERY_KEY_ROOT],
        }),
        queryClient.invalidateQueries({
          queryKey: [TEAM_CATALOG_QUERY_KEY_ROOT],
        }),
      ]);
    },
  });
}
