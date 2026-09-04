import type { PersonaSharePublicationResult } from "@/shared/api/tauriPersonas";

/**
 * The confirmation shown after a persona edit is saved.
 *
 * `publicationStatus` is null when the edit did not promise publication, so
 * the copy stays silent about the catalog. When it did, the copy must
 * distinguish a relay-accepted publish from a queued one — a "published"
 * message for an edit still sitting in the outbox is the promise the
 * "Save and publish" button was making falsely.
 *
 * `bookkeepingError` is appended because the backend no longer raises it: the
 * publish landed, so failing the call would claim nothing was saved. Saying
 * nothing instead would leave the user watching the flush loop republish a head
 * that already went out, with nothing to explain it.
 */
export function personaSaveNotice(
  displayName: string,
  publicationStatus: PersonaSharePublicationResult["publicationStatus"] | null,
  bookkeepingError: string | null = null,
): string {
  const bookkeeping = catalogBookkeepingSentence(bookkeepingError);
  switch (publicationStatus) {
    case "published":
      return `Updated ${displayName} and published it to the community catalog.${bookkeeping}`;
    case "queued":
      return `Updated ${displayName}. Publishing to the community catalog is queued and will appear after the relay accepts the update.${bookkeeping}`;
    default:
      return `Updated ${displayName}.${bookkeeping}`;
  }
}

/**
 * The sentence every catalog-publishing notice appends when the relay accepted
 * the head but the local sync record did not update.
 *
 * One definition, shared by the save-and-publish notice, the share toggle and
 * the prompt reload, because all three call the same backend publish path and a
 * user who sees the same outcome must read the same words.
 */
export function catalogBookkeepingSentence(
  bookkeepingError: string | null | undefined,
): string {
  return bookkeepingError
    ? ` The local sync record did not update (${bookkeepingError}), so the catalog head will be sent again.`
    : "";
}
