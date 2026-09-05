import { useCallback, useMemo, useRef } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";
import { relayClient } from "@/shared/api/relayClient";
import {
  nip44DecryptFromSelf,
  nip44EncryptToSelf,
  signRelayEvent,
} from "@/shared/api/tauri";
import type { RelayEvent } from "@/shared/api/types";
import {
  type FolderSnapshot,
  type TransformResult,
  buildFileFolderMap,
  channelFolderDTag,
  emptySnapshot,
  newFolderId,
  parseFolderPayload,
  serializeSnapshot,
  withFilesAssigned,
  withFolderCreated,
  withFolderDeleted,
  withFolderMoved,
  withFolderRenamed,
} from "./folderStore";

const FILE_FOLDER_KIND = 30078;
const FILE_FOLDER_TAG = "file-folders";
const FOLDER_QUERY_KEY_PREFIX = "channel-file-folders";

export type { FolderNode, FolderSnapshot } from "./folderStore";

export type FolderQueryData = {
  snapshot: FolderSnapshot;
  /** Non-null when the stored payload could not be trusted as complete. */
  invalidReason: string | null;
  /** The replaceable event the snapshot came from, `null` when none exists. */
  head: RelayEvent | null;
};

export function folderQueryKey(channelId: string) {
  return [FOLDER_QUERY_KEY_PREFIX, channelId] as const;
}

/**
 * Whether a successful OK actually stored the event.
 *
 * The relay answers a replaceable write that lost a same-second race with a
 * *successful* OK whose message reads `duplicate:…`, so "the promise resolved"
 * is not "the update landed". An empty message is the ordinary accepted case
 * on relays that send no detail; anything else that is not `Inserted` is
 * treated as a conflict and retried against a freshly read head.
 */
export function isSupersededOk(message: string): boolean {
  const trimmed = message.trim();
  if (trimmed.length === 0) return false;
  return !/^inserted$/i.test(trimmed);
}

async function readFolderEvent(
  channelId: string,
  pubkey: string,
): Promise<FolderQueryData> {
  const events = await relayClient.fetchEvents({
    kinds: [FILE_FOLDER_KIND],
    authors: [pubkey],
    "#d": [channelFolderDTag(channelId)],
    limit: 1,
  });
  const head = events.find((event) => event.pubkey === pubkey) ?? null;
  if (!head) {
    return { snapshot: emptySnapshot(), invalidReason: null, head: null };
  }

  let plaintext: string;
  try {
    plaintext = await nip44DecryptFromSelf(head.content);
  } catch {
    return { snapshot: emptySnapshot(), invalidReason: "decrypt-failed", head };
  }

  const parsed = parseFolderPayload(plaintext);
  if (!parsed.ok) {
    return { snapshot: emptySnapshot(), invalidReason: parsed.reason, head };
  }
  return { snapshot: parsed.snapshot, invalidReason: null, head };
}

/**
 * Folders and file assignments for one channel, held as a single encrypted
 * kind:30078 aggregate per (user, channel).
 *
 * Every mutation is one publish, applied through a per-channel serial queue
 * against the head this client last read, so two rapid actions cannot each
 * build a write from the same stale state and drop one another's change.
 */
export function useFileFolders(
  channelId: string | null,
  currentPubkey?: string,
) {
  const queryClient = useQueryClient();
  const queueRef = useRef<Promise<unknown>>(Promise.resolve());

  const queryKey = folderQueryKey(channelId ?? "");
  const queryFn = useCallback(async (): Promise<FolderQueryData> => {
    if (!channelId || !currentPubkey) {
      return { snapshot: emptySnapshot(), invalidReason: null, head: null };
    }
    return readFolderEvent(channelId, currentPubkey);
  }, [channelId, currentPubkey]);

  const query = useQuery({
    queryKey,
    queryFn,
    enabled: !!channelId && !!currentPubkey,
    staleTime: 30_000,
  });

  const data = query.data;
  const snapshot = data?.snapshot ?? emptySnapshot();
  const folders = snapshot.folders;
  const invalidReason = data?.invalidReason ?? null;

  const fileFolderMap = useMemo(() => buildFileFolderMap(snapshot), [snapshot]);

  const publishSnapshot = useCallback(
    async (
      next: FolderSnapshot,
      head: RelayEvent | null,
      failureMessage: string,
    ): Promise<{ event: RelayEvent; superseded: boolean }> => {
      const ciphertext = await nip44EncryptToSelf(serializeSnapshot(next));
      const createdAt = Math.max(
        Math.floor(Date.now() / 1_000),
        (head?.created_at ?? 0) + 1,
      );
      const event = await signRelayEvent({
        kind: FILE_FOLDER_KIND,
        content: ciphertext,
        createdAt,
        tags: [
          ["d", channelFolderDTag(channelId ?? "")],
          ["t", FILE_FOLDER_TAG],
        ],
      });
      let okMessage = "";
      await relayClient.publishEvent(
        event,
        `Timed out ${failureMessage}`,
        `Failed ${failureMessage}`,
        (message) => {
          okMessage = message;
        },
      );
      return { event, superseded: isSupersededOk(okMessage) };
    },
    [channelId],
  );

  /**
   * Serialise one transform onto the channel's mutation queue: read the head
   * this client last saw, apply the transform to it, publish, and only then
   * update the cache. A superseded acknowledgement re-reads the head: when the
   * head *is* the event we just signed the write did land (the relay answers a
   * re-sent, already-stored event `accepted: true` with `duplicate:`,
   * `crates/buzz-relay/src/handlers/ingest.rs:3206`), so it is reported as
   * saved; otherwise the transform is replayed once before surfacing an error.
   * The transform must be a pure `(snapshot) => snapshot` function, because
   * that replay calls it a second time.
   */
  const runMutation = useCallback(
    async (
      failureMessage: string,
      transform: (snapshot: FolderSnapshot) => TransformResult,
    ): Promise<void> => {
      if (!channelId || !currentPubkey) {
        throw new Error("No channel is open.");
      }

      const run = async () => {
        const readHead = async (force: boolean): Promise<FolderQueryData> => {
          const cached = force
            ? undefined
            : queryClient.getQueryData<FolderQueryData>(queryKey);
          if (cached) return cached;
          return queryClient.fetchQuery({
            queryKey,
            queryFn,
            staleTime: 0,
          });
        };

        let current = await readHead(false);
        for (let attempt = 0; attempt < 2; attempt += 1) {
          if (current.invalidReason) {
            throw new Error(
              "This channel's folder data could not be read, so it cannot be changed. Reload the Files tab and try again.",
            );
          }
          const result = transform(current.snapshot);
          if (!result.ok) throw new Error(result.error);

          const { event, superseded } = await publishSnapshot(
            result.snapshot,
            current.head,
            failureMessage,
          );
          if (!superseded) {
            queryClient.setQueryData<FolderQueryData>(queryKey, {
              snapshot: result.snapshot,
              invalidReason: null,
              head: event,
            });
            void queryClient.invalidateQueries({ queryKey });
            return;
          }
          current = await readHead(true);
          // The acknowledgement was an echo of our own committed write — a
          // socket-level resend of the same signed event returns `duplicate:`
          // after the first send stored it. Replaying here would publish the
          // change twice; telling the user it failed would be a lie.
          if (current.head?.id === event.id) return;
        }

        throw new Error(
          "Another device changed these folders at the same time. Try again.",
        );
      };

      const queued = queueRef.current.then(run, run);
      // Keep the chain alive for the next mutation without letting this
      // rejection escape as an unhandled one; the awaited copy below is what
      // the caller (and its toast) sees.
      queueRef.current = queued.catch(() => undefined);
      try {
        await queued;
      } catch (error) {
        toast.error(
          error instanceof Error ? error.message : `Failed ${failureMessage}`,
        );
        throw error;
      }
    },
    [channelId, currentPubkey, publishSnapshot, queryClient, queryFn, queryKey],
  );

  const createFolder = useCallback(
    (name: string, parent: string | null = null) => {
      // Minted once, outside the transform: `runMutation` may replay the
      // transform against a re-read head, and a fresh id per attempt would
      // publish the same folder twice under two ids.
      const id = newFolderId();
      return runMutation("creating the folder.", (current) =>
        withFolderCreated(current, { id, name, parent }),
      );
    },
    [runMutation],
  );

  const renameFolder = useCallback(
    (id: string, name: string) =>
      runMutation("renaming the folder.", (current) =>
        withFolderRenamed(current, id, name),
      ),
    [runMutation],
  );

  const deleteFolder = useCallback(
    (id: string) =>
      runMutation("deleting the folder.", (current) =>
        withFolderDeleted(current, id),
      ),
    [runMutation],
  );

  const moveFolder = useCallback(
    (id: string, parent: string | null) =>
      runMutation("moving the folder.", (current) =>
        withFolderMoved(current, id, parent),
      ),
    [runMutation],
  );

  const assignFiles = useCallback(
    (fileKeys: string[], folderId: string | null) =>
      runMutation(
        folderId === null
          ? "removing the files from the folder."
          : "moving the files to the folder.",
        (current) => withFilesAssigned(current, fileKeys, folderId),
      ),
    [runMutation],
  );

  return {
    snapshot,
    folders,
    fileFolderMap,
    isLoading: query.isPending,
    isError: query.isError,
    error: query.error,
    invalidReason,
    /** Mutations are only safe against a complete, valid, loaded snapshot. */
    canMutate:
      !!channelId &&
      !!currentPubkey &&
      query.isSuccess &&
      invalidReason === null,
    refetch: query.refetch,
    createFolder,
    renameFolder,
    deleteFolder,
    moveFolder,
    assignFiles,
  };
}
