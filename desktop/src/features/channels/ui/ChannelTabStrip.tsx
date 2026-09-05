import * as React from "react";
import { cn } from "@/shared/lib/cn";

export const CHANNEL_TAB_CHAT = "chat";
export const CHANNEL_TAB_FILES = "files";

const TABS = [
  { value: CHANNEL_TAB_CHAT, label: "Chat" },
  { value: CHANNEL_TAB_FILES, label: "Files" },
] as const;

export function channelTabId(tab: string): string {
  return `channel-tab-${tab}`;
}

export function channelTabPanelId(tab: string): string {
  return `channel-tabpanel-${tab}`;
}

export type ChannelTabStripProps = {
  activeTab: string;
  onSelect: (tab: string) => void;
};

/**
 * The channel's Chat/Files tab strip, implementing the ARIA Tabs pattern:
 * roving `tabIndex` so the strip is one Tab stop, Arrow/Home/End to move
 * between tabs, and explicit `aria-controls`/`aria-labelledby` links to the
 * panel each tab owns.
 *
 * The strip rides *inside* the channel header rather than between the header
 * and the tab content: the header is an overlay whose measured title-row
 * height drives every downstream offset (timeline padding, sticky day
 * divider, shared blur band), and a sibling strip would add a second,
 * unmeasured offset all of those miss.
 */
export function ChannelTabStrip({ activeTab, onSelect }: ChannelTabStripProps) {
  const refs = React.useRef<Array<HTMLButtonElement | null>>([]);
  const onSelectRef = React.useRef(onSelect);
  onSelectRef.current = onSelect;

  const focusTab = React.useCallback((index: number) => {
    const wrapped = (index + TABS.length) % TABS.length;
    const target = refs.current[wrapped];
    onSelectRef.current(TABS[wrapped].value);
    target?.focus();
  }, []);

  const handleKeyDown = React.useCallback(
    (event: React.KeyboardEvent, index: number) => {
      switch (event.key) {
        case "ArrowRight":
          event.preventDefault();
          focusTab(index + 1);
          break;
        case "ArrowLeft":
          event.preventDefault();
          focusTab(index - 1);
          break;
        case "Home":
          event.preventDefault();
          focusTab(0);
          break;
        case "End":
          event.preventDefault();
          focusTab(TABS.length - 1);
          break;
        default:
          break;
      }
    },
    [focusTab],
  );

  return (
    // `-mx-5` bleeds the bottom rule past the header's inline padding;
    // `px-1` then re-aligns the first tab label with the channel title.
    <div className="-mx-5 border-b border-border bg-background px-1">
      <div className="flex gap-0" role="tablist">
        {TABS.map((tab, index) => {
          const isActive = activeTab === tab.value;
          return (
            <button
              aria-controls={channelTabPanelId(tab.value)}
              aria-selected={isActive}
              className={cn(
                "-mb-px border-b-2 px-4 py-2.5 text-sm font-medium transition-colors",
                isActive
                  ? "border-foreground text-foreground"
                  : "border-transparent text-muted-foreground hover:text-foreground",
              )}
              id={channelTabId(tab.value)}
              key={tab.value}
              onClick={() => onSelect(tab.value)}
              onKeyDown={(event) => handleKeyDown(event, index)}
              ref={(node) => {
                refs.current[index] = node;
              }}
              role="tab"
              tabIndex={isActive ? 0 : -1}
              type="button"
            >
              {tab.label}
            </button>
          );
        })}
      </div>
    </div>
  );
}
