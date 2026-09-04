import type {
  PromptSourceBinding,
  PromptSourceResult,
} from "@/shared/api/tauriPersonas";

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
 * The field seeds itself from the stored binding, so Clear is no longer offered
 * blind — but it is still not *gated* on that seed, because the seed can be out
 * of date: another window may have bound a file since this dialog opened, and
 * unbinding what is already unbound is a no-op the backend accepts.
 *
 * Clear is **not** the recovery path for an unreadable sidecar, which is what
 * this gate was once justified by. Clearing one entry has to read the file
 * first, so on a malformed sidecar it fails exactly where the seed did. The
 * reset ({@link canResetPromptSources}) is what recovers that state.
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
    ? ` The file was not remembered for next time: ${result.mappingError}${
        result.binding
          ? ` This agent is still set to ${result.binding.path}, which no longer matches these instructions.`
          : ""
      }`
    : "";
  const bookkeeping = result.bookkeepingError
    ? ` The local sync record did not update (${result.bookkeepingError}), so the catalog head will be sent again.`
    : "";
  const publish = result.publish ?? "";
  if (publish === "published") {
    return `Instructions reloaded from the file and published.${mapping}${bookkeeping}`;
  }
  if (publish === "queued") {
    return `Instructions reloaded from the file. The catalog update is queued.${mapping}${bookkeeping}`;
  }
  if (publish.startsWith("failed:")) {
    return `Instructions reloaded from the file, but the catalog update was not queued: ${publish.slice("failed:".length)}${mapping}${bookkeeping}`;
  }
  return `Instructions reloaded from the file.${mapping}${bookkeeping}`;
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

/**
 * Resting sentence under the field, before any action has been taken.
 *
 * Naming the bound path is what makes the stored binding visible: the value in
 * the input alone cannot say whether it is a live binding or something the
 * operator has just typed and not yet reloaded.
 *
 * The out-of-sync sentence is the one that matters. A binding is a claim that
 * the file's text is what the agent uses, and other paths write those
 * instructions — a hand-typed edit here, a definition replaced from another
 * device. Repeating the claim then would state something false about the agent
 * about to run, so the field says the file has drifted and names both ways out.
 */
export function promptSourceHint(binding: PromptSourceBinding | null): string {
  if (binding === null) {
    return "Read this agent's instructions from a file in your home folder.";
  }
  if (!binding.inSync) {
    return `These instructions no longer match ${binding.path}. Reload to use the file's text again, or Clear to forget the file.`;
  }
  return `These instructions are loaded from ${binding.path}.`;
}

/**
 * Whether to offer the sidecar reset.
 *
 * Only after a seed failed. The reset moves every agent's binding aside, so it
 * is not a general control — but a sidecar that cannot be parsed is refused by
 * the seed *and* by Clear (which must read the file before removing one entry),
 * and a field with an error and two buttons that cannot act on it is a dead
 * end. This is the way out of exactly that state.
 */
export function canResetPromptSources(
  seedFailed: boolean,
  isPending: boolean,
): boolean {
  return seedFailed && !isPending;
}

/** Warning shown beside the reset, because it is not scoped to this agent. */
export const PROMPT_SOURCE_RESET_WARNING =
  "This clears the instructions-file setting for every agent on this machine. The unreadable file is kept, renamed.";
