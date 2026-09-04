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
 * Clear removes a stored binding, and is offered whenever the field is usable.
 *
 * The dialog cannot read the sidecar back — the feature has one backend
 * command and no getter — so on every re-open it knows of no binding even when
 * one exists. Gating Clear on what the dialog has seen this session would make
 * a binding whose file has been moved or deleted unclearable, which is exactly
 * when a user needs it. Unbinding what is already unbound is a no-op the
 * backend accepts, so offering the action always is the safe direction.
 */
export function canClearPromptSource(isPending: boolean): boolean {
  return !isPending;
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
  const mapping = result.mappingError
    ? ` The file was not remembered for next time: ${result.mappingError}`
    : "";
  const publish = result.publish ?? "";
  if (publish === "published") {
    return `Instructions reloaded from the file and published.${mapping}`;
  }
  if (publish === "queued") {
    return `Instructions reloaded from the file. The catalog update is queued.${mapping}`;
  }
  if (publish.startsWith("failed:")) {
    return `Instructions reloaded from the file, but the catalog update was not queued: ${publish.slice("failed:".length)}${mapping}`;
  }
  return `Instructions reloaded from the file.${mapping}`;
}

/**
 * Attribute marking a subtree of the agent dialog whose controls are
 * machine-local.
 *
 * The dialog treats any change inside its form as a user edit, and that flag
 * both warns about unsaved work and arms a catalog publish on save. The
 * instructions-file path is neither submitted with the definition nor
 * published: it is consumed immediately by Reload and Clear. Typing one must
 * therefore leave the dialog exactly as dirty as it already was.
 */
export const DIRTY_EXEMPT_ATTRIBUTE = "data-dirty-exempt";

/** Selector for {@link DIRTY_EXEMPT_ATTRIBUTE}, for ancestor lookups. */
export const DIRTY_EXEMPT_SELECTOR = `[${DIRTY_EXEMPT_ATTRIBUTE}]`;
