/**
 * The bulk drag-and-drop setting for the Files tab.
 *
 * Dragging one file into a folder is one move the user can see. Dragging a
 * selection is a batch of relay writes started by a gesture that is easy to
 * make by accident, so it is off unless the user turns it on, and a batch is
 * capped: an over-cap drop moves nothing and says so, rather than silently
 * moving the first twenty.
 */

import * as React from "react";

export const BULK_DRAG_DROP_STORAGE_KEY = "buzz.channelFiles.bulkDragDrop";
export const DEFAULT_BULK_DRAG_DROP_ENABLED = false;
/** Files one drop may move. The cap is on files, which is what each move writes. */
export const MAX_BULK_DROP_FILES = 20;

const listeners = new Set<() => void>();

/** Parse the stored value; anything unrecognised means the default (off). */
export function parseBulkDragDropEnabled(
  value: string | null | undefined,
): boolean {
  if (value === "true") return true;
  if (value === "false") return false;
  return DEFAULT_BULK_DRAG_DROP_ENABLED;
}

function readStoredPreference(): boolean {
  try {
    return parseBulkDragDropEnabled(
      globalThis.localStorage?.getItem(BULK_DRAG_DROP_STORAGE_KEY),
    );
  } catch {
    return DEFAULT_BULK_DRAG_DROP_ENABLED;
  }
}

let bulkDragDropEnabled = readStoredPreference();

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** Current setting. */
export function getBulkDragDropEnabled(): boolean {
  return bulkDragDropEnabled;
}

/** Change the setting and persist it for this machine. */
export function setBulkDragDropEnabled(value: boolean): void {
  if (value === bulkDragDropEnabled) return;
  bulkDragDropEnabled = value;
  try {
    globalThis.localStorage?.setItem(BULK_DRAG_DROP_STORAGE_KEY, String(value));
  } catch {
    // Persistence is best-effort; the live setting still applies this session.
  }
  for (const listener of listeners) listener();
}

/** Subscribe a component to the setting. */
export function useBulkDragDropEnabled(): boolean {
  return React.useSyncExternalStore(
    subscribe,
    getBulkDragDropEnabled,
    () => DEFAULT_BULK_DRAG_DROP_ENABLED,
  );
}

/** Either the files a drop moves, or the reason it moves nothing. */
export type BulkDropPlan =
  | { keys: string[]; refusedReason?: undefined }
  | { keys?: undefined; refusedReason: string };

/**
 * Decide what a drop onto a folder moves.
 *
 * With the setting off, or when the dragged file is not part of the selection,
 * a drop moves exactly the dragged file — the behaviour the Files tab shipped
 * with. With the setting on it moves the whole selection, up to
 * {@link MAX_BULK_DROP_FILES}; a larger selection is refused whole.
 */
export function resolveBulkDropKeys({
  draggedKey,
  selectedKeys,
  enabled,
}: {
  draggedKey: string;
  selectedKeys: readonly string[];
  enabled: boolean;
}): BulkDropPlan {
  if (!enabled || !selectedKeys.includes(draggedKey)) {
    return { keys: [draggedKey] };
  }
  if (selectedKeys.length > MAX_BULK_DROP_FILES) {
    return {
      refusedReason: `Select at most ${MAX_BULK_DROP_FILES} files to drag into a folder — this selection has ${selectedKeys.length}.`,
    };
  }
  return { keys: [...selectedKeys] };
}
