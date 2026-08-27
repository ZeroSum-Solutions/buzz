import {
  shouldPrioritizeIdleAuxiliary,
  shouldUseFocusIdleDrawer,
} from "./ChannelPane.helpers";

type ChannelPaneAuxiliaryLayoutOptions = {
  channelManagementOpen: boolean;
  hasAgentSession: boolean;
  hasIdleAuxiliaryPanel: boolean;
  hasIdlePanelCloseHandler: boolean;
  hasProfilePanel: boolean;
  hasThreadSurface: boolean;
  idleAuxiliaryOverridesThread: boolean;
  isOverlay: boolean;
  isSinglePanelView: boolean;
  markdownDocName?: string | null;
  markdownDocUrl?: string | null;
  threadViewMode: string;
};

export function createChannelPaneAuxiliaryLayout({
  channelManagementOpen,
  hasAgentSession,
  hasIdleAuxiliaryPanel,
  hasIdlePanelCloseHandler,
  hasProfilePanel,
  hasThreadSurface,
  idleAuxiliaryOverridesThread,
  isOverlay,
  isSinglePanelView,
  markdownDocName,
  markdownDocUrl,
  threadViewMode,
}: ChannelPaneAuxiliaryLayoutOptions) {
  const useSplitAuxiliaryPane = !isSinglePanelView && !isOverlay;
  const useFocusThreadDrawer =
    threadViewMode === "focus" && useSplitAuxiliaryPane && hasThreadSurface;
  const hasIdleAuxiliary = hasIdleAuxiliaryPanel && hasIdlePanelCloseHandler;
  const priorityIdleAuxiliary = shouldPrioritizeIdleAuxiliary(
    idleAuxiliaryOverridesThread,
    hasIdleAuxiliary,
  );
  const overlayIdleAuxiliaryOverThread =
    priorityIdleAuxiliary && hasThreadSurface && !isOverlay;
  const replaceThreadWithIdleAuxiliary =
    priorityIdleAuxiliary && hasThreadSurface && isOverlay;
  const useFocusIdleDrawer = shouldUseFocusIdleDrawer({
    channelManagementOpen,
    hasAgentSession,
    hasIdleAuxiliaryPanel,
    hasIdlePanelCloseHandler,
    hasProfilePanel,
    hasThreadSurface,
    overrideThread: overlayIdleAuxiliaryOverThread,
    useSplitAuxiliaryPane,
  });

  const openMarkdownDoc =
    markdownDocUrl && markdownDocName
      ? { filename: markdownDocName, url: markdownDocUrl }
      : null;
  const hasSplitAuxiliaryPane =
    useSplitAuxiliaryPane &&
    (channelManagementOpen ||
      hasThreadSurface ||
      hasAgentSession ||
      hasProfilePanel);

  return {
    hasSplitAuxiliaryPane,
    openMarkdownDoc,
    priorityIdleAuxiliary,
    replaceThreadWithIdleAuxiliary,
    showIdleAuxiliaryOverThread:
      overlayIdleAuxiliaryOverThread && useFocusIdleDrawer,
    useFocusIdleDrawer,
    useFocusThreadDrawer,
    useSplitAuxiliaryPane,
  };
}
