import type { PromptSourceResult } from "@/shared/api/tauriPersonas";

/**
 * Reload needs something to read. A blank path (or one that is only
 * whitespace) has no file behind it, so the button stays disabled rather than
 * sending a request the backend would refuse.
 */
export function canReloadPromptSource(
  path: string,
  isPending: boolean,
): boolean {
  return !isPending && path.trim().length > 0;
}

/**
 * Clear removes a stored binding. It is enabled only once one exists — before
 * the first successful reload there is nothing on disk to remove, and offering
 * the action would suggest otherwise.
 */
export function canClearPromptSource(
  hasStoredSource: boolean,
  isPending: boolean,
): boolean {
  return !isPending && hasStoredSource;
}

/**
 * Sentence shown after a reload.
 *
 * The three publish outcomes are reported distinctly on purpose: `queued` means
 * a durable retry exists, while `failed:` means the local save landed and the
 * catalog head did not — collapsing them would tell the user a retry is coming
 * when none was recorded.
 */
export function promptSourceStatusMessage(
  result: PromptSourceResult,
): string | null {
  if (!result.localUpdated) {
    return "Prompt file unlinked. Instructions stay as they are.";
  }
  const publish = result.publish ?? "";
  if (publish === "published") {
    return "Instructions reloaded from the file and published.";
  }
  if (publish === "queued") {
    return "Instructions reloaded from the file. The catalog update is queued.";
  }
  if (publish.startsWith("failed:")) {
    return `Instructions reloaded from the file, but the catalog update was not queued: ${publish.slice("failed:".length)}`;
  }
  return "Instructions reloaded from the file.";
}
