import { useCallback, useMemo, useRef, useState } from "react";
import { VList } from "virtua";
import {
  Search,
  ArrowUpDown,
  FolderPlus,
  Folder,
  ChevronRight,
  ChevronDown,
  Trash2,
  X,
  Undo2,
  FolderInput,
  AlertTriangle,
} from "lucide-react";
import { toast } from "sonner";
import { FileRow, FileRowSkeleton } from "./FileCard";
import {
  type FlatFolder,
  type FolderNode,
  type FolderSnapshot,
  flattenFolders,
  hasSiblingNamed,
} from "./folderStore";
import {
  categorizeFile,
  sortFiles,
  type ChannelFile,
  type FileCategory,
  type FileSort,
} from "./useChannelFiles";
import {
  MAX_BULK_DROP_FILES,
  resolveBulkDropKeys,
  setBulkDragDropEnabled,
  useBulkDragDropEnabled,
} from "./bulkDropPreference";
import { Button } from "@/shared/ui/button";

const CATEGORY_TABS: { value: FileCategory; label: string }[] = [
  { value: "all", label: "All" },
  { value: "image", label: "Images" },
  { value: "video", label: "Videos" },
  { value: "document", label: "Documents" },
  { value: "other", label: "Other" },
];

const SORT_OPTIONS: { value: FileSort; label: string }[] = [
  { value: "newest", label: "Newest" },
  { value: "oldest", label: "Oldest" },
  { value: "name", label: "Name" },
  { value: "size", label: "Size" },
];

export type ChannelFilesTabProps = {
  files: ChannelFile[];
  /** True when the file projection hit its row cap and is not the whole set. */
  truncated?: boolean;
  isLoading: boolean;
  /** True when the file list could not be loaded at all. */
  isError?: boolean;
  /**
   * Set when the list is showing files but is known to be incomplete — a
   * history page failed, or live updates stopped. The list still renders; the
   * banner says what is missing and offers the same retry.
   */
  filesError?: string | null;
  onRetryFiles?: () => void;
  /** True when the index stopped short of the channel's oldest attachment. */
  canLoadOlder?: boolean;
  /** Continue the history walk from where it stopped. */
  onLoadOlder?: () => void;
  senderNames?: Map<string, string>;
  senderAvatarUrls?: Map<string, string | null>;
  onJumpToMessage?: (eventId: string) => void;
  snapshot?: FolderSnapshot;
  foldersLoading?: boolean;
  foldersError?: boolean;
  /** Non-null when the stored folder payload could not be trusted. */
  foldersInvalidReason?: string | null;
  onRetryFolders?: () => void;
  /** False while the folder state is unknown, failed, or invalid. */
  canMutateFolders?: boolean;
  /** fileKey → owning folder id. */
  fileFolderMap?: Map<string, string>;
  onCreateFolder?: (name: string, parent: string | null) => Promise<unknown>;
  onDeleteFolder?: (id: string) => Promise<unknown>;
  onMoveFolder?: (id: string, parent: string | null) => Promise<unknown>;
  onAssignFiles?: (
    fileKeys: string[],
    folderId: string | null,
  ) => Promise<unknown>;
};

type Row =
  | { kind: "folder"; key: string; folder: FolderNode; depth: number }
  | { kind: "folder-empty"; key: string; folderId: string }
  | { kind: "file"; key: string; file: ChannelFile; folderId: string | null };

const EMPTY_SNAPSHOT: FolderSnapshot = { folders: [], files: {} };

/** Row count at which the list switches to a virtualized viewport. */
const VIRTUALIZE_ROW_THRESHOLD = 60;

export function ChannelFilesTab({
  files,
  truncated = false,
  isLoading,
  isError = false,
  filesError = null,
  onRetryFiles,
  canLoadOlder = false,
  onLoadOlder,
  senderNames,
  senderAvatarUrls,
  onJumpToMessage,
  snapshot = EMPTY_SNAPSHOT,
  foldersLoading = false,
  foldersError = false,
  foldersInvalidReason = null,
  onRetryFolders,
  canMutateFolders = false,
  fileFolderMap,
  onCreateFolder,
  onDeleteFolder,
  onMoveFolder,
  onAssignFiles,
}: ChannelFilesTabProps) {
  const [category, setCategory] = useState<FileCategory>("all");
  const [searchQuery, setSearchQuery] = useState("");
  const [sort, setSort] = useState<FileSort>("newest");
  const [expandedFolders, setExpandedFolders] = useState<Set<string>>(
    () => new Set(),
  );
  const [isCreatingFolder, setIsCreatingFolder] = useState(false);
  const [newFolderName, setNewFolderName] = useState("");
  const [dragOverFolder, setDragOverFolder] = useState<string | null>(null);
  const bulkDragDrop = useBulkDragDropEnabled();
  const [isSelecting, setIsSelecting] = useState(false);
  const [selectedKeys, setSelectedKeys] = useState<Set<string>>(new Set());
  const [pending, setPending] = useState<Set<string>>(new Set());
  const lastClickedRef = useRef<string | null>(null);

  const folders = snapshot.folders;

  /**
   * One place every mutation goes through: it marks its control pending (so a
   * second click while the write is in flight is a no-op), awaits the result,
   * and swallows nothing — the hook has already surfaced the error to the
   * user, and the boolean it returns tells the caller whether to clear local
   * state such as the selection.
   */
  const runMutation = useCallback(
    async (controlId: string, action: () => Promise<unknown>) => {
      if (pending.has(controlId)) return false;
      setPending((prev) => new Set(prev).add(controlId));
      try {
        await action();
        return true;
      } catch {
        // The folders hook toasts the failure; nothing further to report.
        return false;
      } finally {
        setPending((prev) => {
          const next = new Set(prev);
          next.delete(controlId);
          return next;
        });
      }
    },
    [pending],
  );

  const filtered = useMemo(() => {
    let result = files;
    if (category !== "all") {
      result = result.filter((f) => categorizeFile(f.mimeType) === category);
    }
    if (searchQuery.trim()) {
      const q = searchQuery.toLowerCase().trim();
      result = result.filter(
        (f) =>
          (f.filename ?? "").toLowerCase().includes(q) ||
          (f.caption ?? "").toLowerCase().includes(q),
      );
    }
    return sortFiles(result, sort);
  }, [files, category, searchQuery, sort]);

  const counts = useMemo(() => {
    const c: Record<FileCategory, number> = {
      all: files.length,
      image: 0,
      video: 0,
      document: 0,
      other: 0,
    };
    for (const f of files) c[categorizeFile(f.mimeType)]++;
    return c;
  }, [files]);

  const filesByFolder = useMemo(() => {
    const map = new Map<string, ChannelFile[]>();
    if (!fileFolderMap) return map;
    for (const file of filtered) {
      const folderId = fileFolderMap.get(file.key);
      if (!folderId) continue;
      const list = map.get(folderId) ?? [];
      list.push(file);
      map.set(folderId, list);
    }
    return map;
  }, [filtered, fileFolderMap]);

  const unfiledFiles = useMemo(
    () =>
      fileFolderMap
        ? filtered.filter((f) => !fileFolderMap.has(f.key))
        : filtered,
    [filtered, fileFolderMap],
  );

  const flatFolders: FlatFolder[] = useMemo(
    () => flattenFolders(snapshot, expandedFolders),
    [snapshot, expandedFolders],
  );

  const rows = useMemo(() => {
    const result: Row[] = [];
    for (const { folder, depth } of flatFolders) {
      result.push({
        kind: "folder",
        key: `folder:${folder.id}`,
        folder,
        depth,
      });
      if (!expandedFolders.has(folder.id)) continue;
      const folderFiles = filesByFolder.get(folder.id) ?? [];
      if (folderFiles.length === 0) {
        result.push({
          kind: "folder-empty",
          key: `empty:${folder.id}`,
          folderId: folder.id,
        });
        continue;
      }
      for (const file of folderFiles) {
        result.push({
          kind: "file",
          key: `${folder.id}:${file.key}`,
          file,
          folderId: folder.id,
        });
      }
    }
    for (const file of unfiledFiles) {
      result.push({ kind: "file", key: file.key, file, folderId: null });
    }
    return result;
  }, [flatFolders, expandedFolders, filesByFolder, unfiledFiles]);

  /** Selection order follows render order, so a Shift range means what it looks like. */
  const visibleFileKeys = useMemo(
    () => rows.flatMap((row) => (row.kind === "file" ? [row.file.key] : [])),
    [rows],
  );

  const selectedFolderId = useMemo(() => {
    if (!fileFolderMap || selectedKeys.size === 0) return null;
    let common: string | null = null;
    for (const key of selectedKeys) {
      const folderId = fileFolderMap.get(key);
      if (!folderId) return null;
      if (common === null) common = folderId;
      else if (common !== folderId) return null;
    }
    return common;
  }, [fileFolderMap, selectedKeys]);

  function toggleFolder(id: string) {
    setExpandedFolders((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }

  const handleToggleSelect = useCallback(
    (fileKey: string, shiftKey: boolean) => {
      setSelectedKeys((prev) => {
        const next = new Set(prev);
        if (shiftKey && lastClickedRef.current) {
          const lastIdx = visibleFileKeys.indexOf(lastClickedRef.current);
          const thisIdx = visibleFileKeys.indexOf(fileKey);
          if (lastIdx !== -1 && thisIdx !== -1) {
            const [start, end] =
              lastIdx < thisIdx ? [lastIdx, thisIdx] : [thisIdx, lastIdx];
            for (let i = start; i <= end; i++) next.add(visibleFileKeys[i]);
            lastClickedRef.current = fileKey;
            return next;
          }
        }
        if (next.has(fileKey)) next.delete(fileKey);
        else next.add(fileKey);
        lastClickedRef.current = fileKey;
        return next;
      });
    },
    [visibleFileKeys],
  );

  async function handleCreateFolder() {
    const name = newFolderName.trim();
    if (!name || !onCreateFolder || !canMutateFolders) return;
    if (hasSiblingNamed(snapshot, null, name)) {
      toast.error(`A folder named "${name}" already exists here.`);
      return;
    }
    const ok = await runMutation("create-folder", () =>
      onCreateFolder(name, null),
    );
    if (!ok) return;
    setNewFolderName("");
    setIsCreatingFolder(false);
  }

  const handleDragStart = useCallback((e: React.DragEvent, fileKey: string) => {
    e.dataTransfer.setData("text/plain", fileKey);
    e.dataTransfer.effectAllowed = "move";
  }, []);

  const handleFolderDragOver = useCallback((e: React.DragEvent, id: string) => {
    e.preventDefault();
    e.dataTransfer.dropEffect = "move";
    setDragOverFolder(id);
  }, []);

  const handleFolderDrop = useCallback(
    async (e: React.DragEvent, folder: FolderNode) => {
      e.preventDefault();
      setDragOverFolder(null);
      if (!canMutateFolders) return;
      const draggedFolderId = e.dataTransfer.getData("application/x-folder");
      if (draggedFolderId) {
        if (!onMoveFolder) return;
        // The transform refuses a cycle too; refusing here as well keeps the
        // pointless write off the relay.
        if (draggedFolderId === folder.id) return;
        await runMutation(`folder:${draggedFolderId}`, () =>
          onMoveFolder(draggedFolderId, folder.id),
        );
        return;
      }
      const fileKey = e.dataTransfer.getData("text/plain");
      if (!fileKey || !onAssignFiles) return;
      const plan = resolveBulkDropKeys({
        draggedKey: fileKey,
        selectedKeys: Array.from(selectedKeys),
        enabled: bulkDragDrop,
      });
      if (!plan.keys) {
        // Refuse the whole batch out loud: a silently truncated drop leaves
        // the user believing files moved that did not.
        toast.error(plan.refusedReason);
        return;
      }
      const keys = plan.keys.filter(
        (key) => fileFolderMap?.get(key) !== folder.id,
      );
      if (keys.length === 0) return;
      await runMutation(keys.length === 1 ? `file:${keys[0]}` : "bulk", () =>
        onAssignFiles(keys, folder.id),
      );
    },
    [
      bulkDragDrop,
      canMutateFolders,
      fileFolderMap,
      onAssignFiles,
      onMoveFolder,
      runMutation,
      selectedKeys,
    ],
  );

  async function handleAssignSelection(folderId: string | null) {
    if (!onAssignFiles || !canMutateFolders) return;
    const keys = Array.from(selectedKeys);
    if (keys.length === 0) return;
    const ok = await runMutation("bulk", () => onAssignFiles(keys, folderId));
    if (!ok) return;
    const target =
      folderId === null
        ? "unfiled"
        : (folders.find((f) => f.id === folderId)?.name ?? "the folder");
    toast.success(
      `Moved ${keys.length} file${keys.length !== 1 ? "s" : ""} to ${target}`,
    );
    setSelectedKeys(new Set());
  }

  function renderFileRow(file: ChannelFile) {
    return (
      <FileRow
        file={file}
        onDragStart={canMutateFolders ? handleDragStart : undefined}
        onJumpToMessage={onJumpToMessage}
        onToggleSelect={handleToggleSelect}
        selected={selectedKeys.has(file.key)}
        selecting={isSelecting}
        senderAvatarUrl={senderAvatarUrls?.get(file.pubkey) ?? null}
        senderName={senderNames?.get(file.pubkey)}
      />
    );
  }

  function renderFolderRow(folder: FolderNode, depth: number) {
    const folderFiles = filesByFolder.get(folder.id) ?? [];
    const isExpanded = expandedFolders.has(folder.id);
    const isPending = pending.has(`folder:${folder.id}`);
    return (
      // biome-ignore lint/a11y/noStaticElementInteractions: drag-and-drop is the pointer convenience; the "Move folder to" select below is the keyboard-reachable equivalent for every nest/un-nest this accepts
      <div
        className={`flex items-center gap-2 px-3 py-2 transition-colors ${
          dragOverFolder === folder.id
            ? "bg-primary/10 ring-2 ring-primary/30"
            : "hover:bg-muted/50"
        } ${isPending ? "opacity-60" : ""}`}
        draggable={canMutateFolders}
        onDragLeave={() => setDragOverFolder(null)}
        onDragOver={(e) => handleFolderDragOver(e, folder.id)}
        onDragStart={(e) => {
          e.dataTransfer.setData("application/x-folder", folder.id);
          e.dataTransfer.effectAllowed = "move";
        }}
        onDrop={(e) => void handleFolderDrop(e, folder)}
        style={{ paddingLeft: `${12 + depth * 20}px` }}
      >
        <button
          aria-expanded={isExpanded}
          className="flex flex-1 items-center gap-2 text-left text-sm font-medium"
          onClick={() => toggleFolder(folder.id)}
          type="button"
        >
          {isExpanded ? (
            <ChevronDown className="h-4 w-4 text-muted-foreground" />
          ) : (
            <ChevronRight className="h-4 w-4 text-muted-foreground" />
          )}
          <Folder className="h-4 w-4 text-muted-foreground" />
          {folder.name}
          <span className="text-xs text-muted-foreground">
            ({folderFiles.length})
          </span>
        </button>
        {onMoveFolder ? (
          <select
            aria-label={`Move folder ${folder.name} to`}
            className="h-7 rounded border border-border bg-background px-1 text-xs focus:outline-none focus:ring-1 focus:ring-ring"
            disabled={!canMutateFolders || isPending}
            onChange={(e) => {
              const value = e.target.value;
              if (!value) return;
              void runMutation(`folder:${folder.id}`, () =>
                onMoveFolder(folder.id, value === "root" ? null : value),
              );
            }}
            value=""
          >
            <option disabled value="">
              Move to…
            </option>
            <option value="root">Root</option>
            {folders
              .filter((candidate) => candidate.id !== folder.id)
              .map((candidate) => (
                <option key={candidate.id} value={candidate.id}>
                  {candidate.name}
                </option>
              ))}
          </select>
        ) : null}
        {onDeleteFolder ? (
          <Button
            aria-label={`Delete folder ${folder.name}`}
            className="h-7 w-7 opacity-60 hover:opacity-100"
            disabled={!canMutateFolders || isPending}
            onClick={() =>
              void runMutation(`folder:${folder.id}`, () =>
                onDeleteFolder(folder.id),
              )
            }
            size="icon-xs"
            variant="ghost"
          >
            <Trash2 className="h-3.5 w-3.5" />
          </Button>
        ) : null}
      </div>
    );
  }

  function renderRow(row: Row) {
    if (row.kind === "folder") return renderFolderRow(row.folder, row.depth);
    if (row.kind === "folder-empty") {
      return (
        <p className="ml-6 border-l-2 border-l-muted px-3 py-4 text-xs text-muted-foreground">
          Empty folder — drag files here, or select files and use “Move to
          folder”.
        </p>
      );
    }
    if (row.folderId === null) return renderFileRow(row.file);
    return (
      <div className="group ml-6 flex items-center border-l-2 border-l-muted">
        <div className="min-w-0 flex-1">{renderFileRow(row.file)}</div>
        {onAssignFiles ? (
          <Button
            aria-label={`Remove ${row.file.filename ?? "file"} from folder`}
            className="mr-2 h-7 w-7 shrink-0 opacity-0 transition-opacity focus-visible:opacity-100 group-focus-within:opacity-100 group-hover:opacity-100"
            disabled={!canMutateFolders || pending.has(`file:${row.file.key}`)}
            onClick={() =>
              void runMutation(`file:${row.file.key}`, () =>
                onAssignFiles([row.file.key], null),
              )
            }
            size="icon-xs"
            variant="ghost"
          >
            <Undo2 className="h-3.5 w-3.5" />
          </Button>
        ) : null}
      </div>
    );
  }

  if (isLoading) {
    return (
      <div className="flex-1 overflow-y-auto p-4">
        <div className="divide-y divide-border">
          {Array.from({ length: 8 }).map((_, i) => (
            // biome-ignore lint/suspicious/noArrayIndexKey: static skeleton rows
            <FileRowSkeleton key={i} />
          ))}
        </div>
      </div>
    );
  }

  const selectedCount = selectedKeys.size;
  const folderStateBroken = foldersError || foldersInvalidReason !== null;

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="shrink-0 space-y-2 border-b border-border px-4 pb-3 pt-3">
        <div className="flex items-center gap-1 overflow-x-auto [scrollbar-width:none]">
          {CATEGORY_TABS.map((tab) => (
            <Button
              className="h-7 shrink-0 rounded-full px-3 text-xs"
              data-active={category === tab.value}
              key={tab.value}
              onClick={() => setCategory(tab.value)}
              size="sm"
              variant={category === tab.value ? "secondary" : "ghost"}
            >
              {tab.label}
              {counts[tab.value] > 0 ? (
                <span className="ml-1 text-muted-foreground">
                  {counts[tab.value]}
                </span>
              ) : null}
            </Button>
          ))}
        </div>

        <div className="flex items-center gap-2">
          <div className="relative flex-1">
            <Search className="absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
            <input
              className="h-8 w-full rounded-md border border-border bg-background pl-8 pr-3 text-xs placeholder:text-muted-foreground focus:outline-none focus:ring-1 focus:ring-ring"
              onChange={(e) => setSearchQuery(e.target.value)}
              placeholder="Search files..."
              type="text"
              value={searchQuery}
            />
            {searchQuery ? (
              <button
                aria-label="Clear search"
                className="absolute right-2 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground"
                onClick={() => setSearchQuery("")}
                type="button"
              >
                ×
              </button>
            ) : null}
          </div>

          <div className="relative">
            <select
              aria-label="Sort files"
              className="h-8 appearance-none rounded-md border border-border bg-background px-7 py-0 pr-6 text-xs focus:outline-none focus:ring-1 focus:ring-ring"
              onChange={(e) => setSort(e.target.value as FileSort)}
              value={sort}
            >
              {SORT_OPTIONS.map((opt) => (
                <option key={opt.value} value={opt.value}>
                  {opt.label}
                </option>
              ))}
            </select>
            <ArrowUpDown className="pointer-events-none absolute left-2 top-1/2 h-3 w-3 -translate-y-1/2 text-muted-foreground" />
          </div>

          <Button
            aria-pressed={isSelecting}
            className="h-8 shrink-0 px-2 text-xs"
            onClick={() => {
              setIsSelecting((prev) => {
                if (prev) setSelectedKeys(new Set());
                return !prev;
              });
            }}
            size="sm"
            variant={isSelecting ? "secondary" : "outline"}
          >
            {isSelecting ? "Done" : "Select"}
          </Button>

          {isSelecting ? (
            <Button
              aria-pressed={bulkDragDrop}
              className="h-8 shrink-0 px-2 text-xs"
              onClick={() => setBulkDragDropEnabled(!bulkDragDrop)}
              size="sm"
              title={`Dragging one selected file moves the whole selection, up to ${MAX_BULK_DROP_FILES} files`}
              variant={bulkDragDrop ? "secondary" : "outline"}
            >
              Drag selection
            </Button>
          ) : null}

          {onCreateFolder ? (
            <Button
              className="h-8 shrink-0 gap-1 px-2 text-xs"
              disabled={!canMutateFolders}
              onClick={() => setIsCreatingFolder(true)}
              size="sm"
              variant="outline"
            >
              <FolderPlus className="h-3.5 w-3.5" />
              New
            </Button>
          ) : null}
        </div>

        {isCreatingFolder ? (
          <div className="flex items-center gap-2">
            <input
              className="h-8 flex-1 rounded-md border border-border bg-background px-3 text-xs focus:outline-none focus:ring-1 focus:ring-ring"
              onChange={(e) => setNewFolderName(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") void handleCreateFolder();
                if (e.key === "Escape") setIsCreatingFolder(false);
              }}
              placeholder="Folder name..."
              type="text"
              value={newFolderName}
            />
            <Button
              className="h-8 px-3 text-xs"
              disabled={
                !newFolderName.trim() ||
                !canMutateFolders ||
                pending.has("create-folder")
              }
              onClick={() => void handleCreateFolder()}
              size="sm"
            >
              Create
            </Button>
            <Button
              className="h-8 px-2 text-xs"
              onClick={() => setIsCreatingFolder(false)}
              size="sm"
              variant="ghost"
            >
              Cancel
            </Button>
          </div>
        ) : null}
      </div>

      {filesError !== null && !isError ? (
        <div
          className="flex shrink-0 items-center gap-2 border-b border-destructive/30 bg-destructive/5 px-4 py-2 text-xs"
          role="alert"
        >
          <AlertTriangle className="h-3.5 w-3.5 text-destructive" />
          <span className="flex-1">{filesError}</span>
          {onRetryFiles ? (
            <Button
              className="h-7 px-2 text-xs"
              onClick={onRetryFiles}
              size="sm"
              variant="outline"
            >
              Retry
            </Button>
          ) : null}
        </div>
      ) : null}

      {folderStateBroken ? (
        <div
          className="flex shrink-0 items-center gap-2 border-b border-destructive/30 bg-destructive/5 px-4 py-2 text-xs"
          role="alert"
        >
          <AlertTriangle className="h-3.5 w-3.5 text-destructive" />
          <span className="flex-1">
            {foldersError
              ? "Folders could not be loaded, so the list below shows every file as unfiled."
              : "This channel's folder data could not be read, so folders are disabled."}
          </span>
          {onRetryFolders ? (
            <Button
              className="h-7 px-2 text-xs"
              onClick={onRetryFolders}
              size="sm"
              variant="outline"
            >
              Retry
            </Button>
          ) : null}
        </div>
      ) : null}

      {foldersLoading ? (
        <p className="shrink-0 border-b border-border px-4 py-1.5 text-xs text-muted-foreground">
          Loading folders…
        </p>
      ) : null}

      {truncated ? (
        <p className="shrink-0 border-b border-border px-4 py-1.5 text-xs text-muted-foreground">
          Showing the most recent attachments only — this channel has more than
          the Files tab loads at once.
        </p>
      ) : null}

      {selectedCount > 0 ? (
        <div className="flex shrink-0 items-center gap-2 border-b border-primary/20 bg-primary/5 px-4 py-2">
          <span className="text-xs font-medium">{selectedCount} selected</span>
          <Button
            className="h-7 px-2 text-xs"
            onClick={() => setSelectedKeys(new Set(visibleFileKeys))}
            size="sm"
            variant="ghost"
          >
            Select all
          </Button>
          <div className="flex-1" />
          {selectedFolderId ? (
            <Button
              className="h-7 gap-1 px-2 text-xs"
              disabled={!canMutateFolders || pending.has("bulk")}
              onClick={() => void handleAssignSelection(null)}
              size="sm"
              variant="outline"
            >
              <Undo2 className="h-3.5 w-3.5" />
              Remove from folder
            </Button>
          ) : (
            <div className="relative">
              <select
                aria-label="Move selected files to folder"
                className="h-7 appearance-none rounded border border-border bg-background pl-2 pr-6 text-xs focus:outline-none focus:ring-1 focus:ring-ring"
                disabled={!canMutateFolders || pending.has("bulk")}
                onChange={(e) => {
                  if (e.target.value)
                    void handleAssignSelection(e.target.value);
                }}
                value=""
              >
                <option disabled value="">
                  Move to folder…
                </option>
                {folders.map((f) => (
                  <option key={f.id} value={f.id}>
                    {f.name}
                  </option>
                ))}
              </select>
              <FolderInput className="pointer-events-none absolute right-2 top-1/2 h-3 w-3 -translate-y-1/2 text-muted-foreground" />
            </div>
          )}
          <Button
            className="h-7 px-2 text-xs"
            onClick={() => setSelectedKeys(new Set())}
            size="sm"
            variant="ghost"
          >
            <X className="mr-1 h-3.5 w-3.5" />
            Clear
          </Button>
        </div>
      ) : null}

      {isError ? (
        <div className="flex flex-1 items-center justify-center p-12">
          <div className="flex max-w-xs flex-col items-center gap-2 text-center">
            <AlertTriangle className="h-5 w-5 text-destructive" />
            <p className="text-sm font-medium">Files could not be loaded</p>
            <p className="text-xs text-muted-foreground">
              This is not an empty channel — the request failed.
            </p>
            {onRetryFiles ? (
              <Button onClick={onRetryFiles} size="sm" variant="outline">
                Retry
              </Button>
            ) : null}
          </div>
        </div>
      ) : rows.length === 0 ? (
        <div className="flex flex-1 items-center justify-center p-12">
          <div className="flex max-w-xs flex-col items-center gap-2 text-center">
            <p className="text-sm font-medium">No files yet</p>
            <p className="text-xs text-muted-foreground">
              Files shared in this channel will appear here.
            </p>
          </div>
        </div>
      ) : (
        (() => {
          const children = rows.map((row) => (
            <div className="border-b border-border" key={row.key}>
              {renderRow(row)}
            </div>
          ));
          // Above the threshold the list is virtualized, so the mounted DOM
          // stays proportional to the viewport however many attachments the
          // channel holds. Below it the whole list is already smaller than a
          // virtualizer's own window, and rendering it plainly keeps the rows
          // reachable to find-in-page and to assistive technology.
          return rows.length >= VIRTUALIZE_ROW_THRESHOLD ? (
            <VList className="flex-1" data-testid="channel-files-list">
              {children}
            </VList>
          ) : (
            <div
              className="flex-1 overflow-y-auto"
              data-testid="channel-files-list"
            >
              {children}
            </div>
          );
        })()
      )}

      {canLoadOlder && onLoadOlder ? (
        <div className="shrink-0 border-t border-border p-2 text-center">
          <Button
            className="h-7 px-3 text-xs"
            onClick={onLoadOlder}
            size="sm"
            variant="outline"
          >
            Load older files
          </Button>
        </div>
      ) : null}
    </div>
  );
}
