import * as React from "react";
import type { CalendarEvent } from "@/shared/api/tauriCalendar";
import {
  VirtualizedList,
  type ListVirtualizer,
} from "@/shared/ui/VirtualizedList";

export function CalendarEventList({
  events,
  onSelect,
}: {
  events: CalendarEvent[];
  onSelect: (event: CalendarEvent) => void;
}) {
  const virtualizer = React.useRef<ListVirtualizer | null>(null);
  const container = React.useRef<HTMLDivElement>(null);
  function moveFocus(event: React.KeyboardEvent, index: number) {
    const destination =
      event.key === "ArrowDown"
        ? index + 1
        : event.key === "ArrowUp"
          ? index - 1
          : event.key === "Home"
            ? 0
            : event.key === "End"
              ? events.length - 1
              : null;
    if (destination === null) return;
    event.preventDefault();
    const target = Math.max(0, Math.min(events.length - 1, destination));
    virtualizer.current?.scrollToIndex(target, { align: "auto" });
    let attempts = 0;
    const focus = () => {
      const button = container.current?.querySelector<HTMLButtonElement>(
        `[data-calendar-index="${target}"]`,
      );
      if (button) button.focus({ preventScroll: true });
      else if (++attempts < 5) requestAnimationFrame(focus);
    };
    requestAnimationFrame(focus);
  }
  return (
    <div ref={container} className="flex min-h-0 flex-1 flex-col">
      <VirtualizedList
        items={events}
        getItemKey={(event) => event.id}
        className="min-h-0 flex-1"
        estimateSize={86}
        listLabel="Calendar events"
        onVirtualizer={(instance) => {
          virtualizer.current = instance;
        }}
        renderItem={(event, index) => (
          <button
            type="button"
            data-calendar-index={index}
            className="flex w-full flex-col gap-1 border-b px-4 py-4 text-left hover:bg-muted focus-visible:outline focus-visible:outline-primary"
            onKeyDown={(key) => moveFocus(key, index)}
            onClick={() => onSelect(event)}
          >
            <span className="text-sm font-medium">
              {event.summary.value || "Untitled event"}
            </span>
            <span className="text-xs text-muted-foreground">
              {event.start.kind === "all_day"
                ? `${event.start.date} · All day`
                : new Date(event.start.instant_ms).toLocaleString(undefined, {
                    month: "short",
                    day: "numeric",
                    hour: "numeric",
                    minute: "2-digit",
                  })}
            </span>
            {event.location.value ? (
              <span className="truncate text-xs text-muted-foreground">
                {event.location.value}
              </span>
            ) : null}
          </button>
        )}
      />
    </div>
  );
}
