import { useCallback, useMemo } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";
import { relayClient } from "@/shared/api/relayClient";
import { signRelayEvent } from "@/shared/api/tauri";
import type { RelayEvent } from "@/shared/api/types";

const FILE_FOLDER_KIND = 30078;
const FILE_FOLDER_TAG = "file-folder";
const FOLDER_QUERY_KEY_PREFIX = "channel-file-folders";

// Relay-sourced string/count caps applied at the DTO boundary (parseFolder)
// — a hostile or buggy folder event must not be able to hand the UI an
// unbounded name/d-tag to render, or an unbounded file list to enumerate.
const MAX_FOLDER_NAME_LENGTH = 200;
const MAX_FOLDER_DTAG_LENGTH = 300;
const MAX_FOLDER_FILES = 500;

function capString(value: string, maxLength: number): string {
  return value.length > maxLength ? value.slice(0, maxLength) : value;
}

export type FileFolder = {
  dTag: string;
  name: string;
  fileEventIds: string[];
  /** Parent folder d-tag, if nested. */
  parentDTag?: string;
  event: RelayEvent;
};

/** URL-safe slug for a folder name, used as the tail of its d-tag. */
export function folderSlug(name: string): string {
  return name
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-|-$/g, "");
}

/** The d-tag for a file folder: scoped to the channel, keyed by name slug. */
export function folderDTag(channelId: string, slug: string): string {
  return `files-${channelId}:${slug}`;
}

/**
 * Parse a kind:30078 file-folder event into a {@link FileFolder}, or `null`
 * if the event isn't a well-formed file-folder (wrong `t` tag, missing `d`).
 * Exported so the write-path helpers below, and their tests, share this
 * exact parsing — every mutation's round trip (build tags → parse them
 * back) is bound to the same production code the hook itself calls.
 */
export function parseFolder(event: RelayEvent): FileFolder | null {
  const rawDTag = event.tags.find((t) => t[0] === "d")?.[1];
  const typeTag = event.tags.find((t) => t[0] === "t");
  if (!rawDTag || typeTag?.[1] !== FILE_FOLDER_TAG) return null;
  const dTag = capString(rawDTag, MAX_FOLDER_DTAG_LENGTH);

  const rawName = event.tags.find((t) => t[0] === "name")?.[1] ?? "Untitled";
  const name = capString(rawName, MAX_FOLDER_NAME_LENGTH);
  const rawParentDTag = event.tags.find((t) => t[0] === "parent")?.[1];
  const parentDTag =
    rawParentDTag != null
      ? capString(rawParentDTag, MAX_FOLDER_DTAG_LENGTH)
      : undefined;
  const fileEventIds = event.tags
    .filter((t) => t[0] === "e")
    .map((t) => t[1])
    .filter(Boolean)
    .slice(0, MAX_FOLDER_FILES);

  return { dTag, name, fileEventIds, parentDTag, event };
}

function folderQueryKey(channelId: string) {
  return [FOLDER_QUERY_KEY_PREFIX, channelId] as const;
}

/** Group file event IDs by owning folder dTag for fast lookup. */
export function buildFileFolderMap(folders: FileFolder[]): Map<string, string> {
  const map = new Map<string, string>();
  for (const folder of folders) {
    for (const eventId of folder.fileEventIds) {
      map.set(eventId, folder.dTag);
    }
  }
  return map;
}

/** Tags for a new file-folder event. */
export function buildCreateFolderTags(
  channelId: string,
  name: string,
  parentDTag?: string,
): string[][] {
  const dTag = folderDTag(channelId, folderSlug(name));
  const tags: string[][] = [
    ["d", dTag],
    ["t", FILE_FOLDER_TAG],
    ["name", name],
  ];
  if (parentDTag) tags.push(["parent", parentDTag]);
  return tags;
}

/** Tags for adding one file to a folder — replaces any existing `e` tag for it, adds one otherwise. */
export function withFileAddedToFolder(
  folder: FileFolder,
  eventId: string,
): string[][] {
  return [
    ...folder.event.tags.filter((t) => t[0] !== "e" || t[1] !== eventId),
    ["e", eventId],
  ];
}

/**
 * Tags for adding multiple files to a folder in one event (avoids the race
 * of publishing N separate replaceable events). Returns `null` when every
 * id is already present — no-op, caller should skip the write.
 */
export function withFilesAddedToFolder(
  folder: FileFolder,
  eventIds: string[],
): string[][] | null {
  // Read existing ids from the folder's raw tags, not `folder.fileEventIds`
  // — that field is capped at MAX_FOLDER_FILES for display, and merging
  // against the truncated view would drop real "e" tags past the cap when
  // the filter below runs.
  const existingIds = new Set(
    folder.event.tags.filter((t) => t[0] === "e").map((t) => t[1]),
  );
  const newIds = eventIds.filter((id) => !existingIds.has(id));
  if (newIds.length === 0) return null;
  return [
    ...folder.event.tags.filter((t) => t[0] !== "e" || existingIds.has(t[1])),
    ...newIds.map((id) => ["e", id] as [string, string]),
  ];
}

/** Tags for removing one file from a folder. */
export function withFileRemovedFromFolder(
  folder: FileFolder,
  eventId: string,
): string[][] {
  return folder.event.tags.filter((t) => !(t[0] === "e" && t[1] === eventId));
}

/** Tags for renaming a folder (new d-tag + name, file refs kept), plus whether the d-tag changed. */
export function buildRenameFolderTags(
  folder: FileFolder,
  channelId: string,
  newName: string,
): { tags: string[][]; newDTag: string; dTagChanged: boolean } {
  const newDTag = folderDTag(channelId, folderSlug(newName));
  const tags = folder.event.tags
    .filter((t) => t[0] !== "d" && t[0] !== "name")
    .concat([
      ["d", newDTag],
      ["name", newName],
    ]);
  return { tags, newDTag, dTagChanged: newDTag !== folder.dTag };
}

/** Tags for moving a folder under `parentDTag` (or to root when omitted). */
export function withFolderParent(
  folder: FileFolder,
  parentDTag?: string,
): string[][] {
  return folder.event.tags
    .filter((t) => t[0] !== "parent")
    .concat(parentDTag ? [["parent", parentDTag]] : []);
}

export function useFileFolders(
  channelId: string | null,
  currentPubkey?: string,
) {
  const queryClient = useQueryClient();

  const query = useQuery({
    queryKey: folderQueryKey(channelId ?? ""),
    queryFn: async () => {
      if (!channelId || !currentPubkey) return [];
      const events = await relayClient.fetchEvents({
        kinds: [FILE_FOLDER_KIND],
        authors: [currentPubkey],
        limit: 50,
      });
      return events.map(parseFolder).filter(
        (f): f is FileFolder =>
          // biome-ignore lint/complexity/useOptionalChain: explicit null check is required for the `f is FileFolder` type predicate
          f !== null && f.dTag.startsWith(`files-${channelId}:`),
      );
    },
    enabled: !!channelId && !!currentPubkey,
    staleTime: 30_000,
  });

  const folders = query.data ?? [];

  const fileFolderMap = useMemo(() => buildFileFolderMap(folders), [folders]);

  const createFolder = useCallback(
    async (name: string, parentDTag?: string): Promise<FileFolder | null> => {
      if (!channelId || !currentPubkey) return null;
      try {
        const event = await signRelayEvent({
          kind: FILE_FOLDER_KIND,
          content: "",
          tags: buildCreateFolderTags(channelId, name, parentDTag),
        });
        await relayClient.publishEvent(
          event,
          "Timed out creating folder.",
          "Failed to create folder.",
        );
        await queryClient.invalidateQueries({
          queryKey: folderQueryKey(channelId),
        });
        return parseFolder(event);
      } catch (error) {
        toast.error(
          error instanceof Error ? error.message : "Failed to create folder.",
        );
        throw error;
      }
    },
    [channelId, currentPubkey, queryClient],
  );

  const addFileToFolder = useCallback(
    async (folder: FileFolder, eventId: string) => {
      if (!channelId || !currentPubkey) return;
      if (folder.fileEventIds.includes(eventId)) return;
      try {
        const event = await signRelayEvent({
          kind: FILE_FOLDER_KIND,
          content: "",
          tags: withFileAddedToFolder(folder, eventId),
          createdAt: Math.floor(Date.now() / 1000),
        });
        await relayClient.publishEvent(
          event,
          "Timed out adding the file to the folder.",
          "Failed to add the file to the folder.",
        );
        await queryClient.invalidateQueries({
          queryKey: folderQueryKey(channelId),
        });
      } catch (error) {
        toast.error(
          error instanceof Error
            ? error.message
            : "Failed to add the file to the folder.",
        );
        throw error;
      }
    },
    [channelId, currentPubkey, queryClient],
  );

  /** Add multiple files to a folder in a single event — avoids race conditions. */
  const addFilesToFolder = useCallback(
    async (folder: FileFolder, eventIds: string[]) => {
      if (!channelId || !currentPubkey || eventIds.length === 0) return;
      const newTags = withFilesAddedToFolder(folder, eventIds);
      if (newTags === null) return;
      try {
        const event = await signRelayEvent({
          kind: FILE_FOLDER_KIND,
          content: "",
          tags: newTags,
          createdAt: Math.floor(Date.now() / 1000),
        });
        await relayClient.publishEvent(
          event,
          "Timed out adding files to the folder.",
          "Failed to add files to the folder.",
        );
        await queryClient.invalidateQueries({
          queryKey: folderQueryKey(channelId),
        });
      } catch (error) {
        toast.error(
          error instanceof Error
            ? error.message
            : "Failed to add files to the folder.",
        );
        throw error;
      }
    },
    [channelId, currentPubkey, queryClient],
  );

  const removeFileFromFolder = useCallback(
    async (folder: FileFolder, eventId: string) => {
      if (!channelId || !currentPubkey) return;
      try {
        const event = await signRelayEvent({
          kind: FILE_FOLDER_KIND,
          content: "",
          tags: withFileRemovedFromFolder(folder, eventId),
          createdAt: Math.floor(Date.now() / 1000),
        });
        await relayClient.publishEvent(
          event,
          "Timed out removing the file from the folder.",
          "Failed to remove the file from the folder.",
        );
        await queryClient.invalidateQueries({
          queryKey: folderQueryKey(channelId),
        });
      } catch (error) {
        toast.error(
          error instanceof Error
            ? error.message
            : "Failed to remove the file from the folder.",
        );
        throw error;
      }
    },
    [channelId, currentPubkey, queryClient],
  );

  const deleteFolder = useCallback(
    async (folder: FileFolder) => {
      if (!channelId || !currentPubkey) return;
      try {
        // NIP-09 deletion: publish kind:5 referencing the folder event
        const event = await signRelayEvent({
          kind: 5,
          content: "deleting file folder",
          tags: [
            ["e", folder.event.id],
            ["k", String(FILE_FOLDER_KIND)],
          ],
        });
        await relayClient.publishEvent(
          event,
          "Timed out deleting the folder.",
          "Failed to delete the folder.",
        );
        await queryClient.invalidateQueries({
          queryKey: folderQueryKey(channelId),
        });
      } catch (error) {
        toast.error(
          error instanceof Error
            ? error.message
            : "Failed to delete the folder.",
        );
        throw error;
      }
    },
    [channelId, currentPubkey, queryClient],
  );

  const renameFolder = useCallback(
    async (folder: FileFolder, newName: string) => {
      if (!channelId || !currentPubkey) return;
      const { tags, dTagChanged } = buildRenameFolderTags(
        folder,
        channelId,
        newName,
      );

      try {
        const event = await signRelayEvent({
          kind: FILE_FOLDER_KIND,
          content: "",
          tags,
          createdAt: Math.floor(Date.now() / 1000),
        });
        await relayClient.publishEvent(
          event,
          "Timed out renaming the folder.",
          "Failed to rename the folder.",
        );

        // If the d-tag changed, also delete the old event. The rename above
        // has already published, so this old-event cleanup is best-effort:
        // report failure without rolling back the (successful) rename.
        if (dTagChanged) {
          const deleteEvent = await signRelayEvent({
            kind: 5,
            content: "",
            tags: [
              ["e", folder.event.id],
              ["k", String(FILE_FOLDER_KIND)],
            ],
          });
          await relayClient.publishEvent(
            deleteEvent,
            "Timed out cleaning up the renamed folder's old entry.",
            "Failed to clean up the renamed folder's old entry.",
          );
        }

        await queryClient.invalidateQueries({
          queryKey: folderQueryKey(channelId),
        });
      } catch (error) {
        toast.error(
          error instanceof Error
            ? error.message
            : "Failed to rename the folder.",
        );
        throw error;
      }
    },
    [channelId, currentPubkey, queryClient],
  );

  /** Move a folder under another folder (or to root when parentDTag=undefined). */
  const setFolderParent = useCallback(
    async (folder: FileFolder, parentDTag?: string) => {
      if (!channelId || !currentPubkey) return;
      try {
        const event = await signRelayEvent({
          kind: FILE_FOLDER_KIND,
          content: "",
          tags: withFolderParent(folder, parentDTag),
          createdAt: Math.floor(Date.now() / 1000),
        });
        await relayClient.publishEvent(
          event,
          "Timed out moving the folder.",
          "Failed to move the folder.",
        );
        await queryClient.invalidateQueries({
          queryKey: folderQueryKey(channelId),
        });
      } catch (error) {
        toast.error(
          error instanceof Error ? error.message : "Failed to move the folder.",
        );
        throw error;
      }
    },
    [channelId, currentPubkey, queryClient],
  );

  return {
    folders,
    fileFolderMap,
    isLoading: query.isPending,
    createFolder,
    addFileToFolder,
    addFilesToFolder,
    removeFileFromFolder,
    deleteFolder,
    renameFolder,
    setFolderParent,
  };
}
