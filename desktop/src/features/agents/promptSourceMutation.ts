import { useMutation, useQueryClient } from "@tanstack/react-query";

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
 * persona update path, so it invalidates exactly what a typed edit invalidates
 * — and the backend command emits no `agents-data-changed`, so nothing else
 * will. Without this the cached persona keeps the pre-reload prompt for
 * `staleTime`, the next `openEdit` seeds the dialog from that stale copy, and
 * saving any unrelated field resubmits the old instructions over the new ones.
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
      ]);
    },
  });
}
