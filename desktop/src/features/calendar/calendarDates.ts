export type CalendarTime =
  | { kind: "all_day"; date: string }
  | { kind: "timed"; instant_ms: number; time_zone?: string | null };

export type DatedEvent = {
  id: string;
  status: string;
  start: CalendarTime;
  end: CalendarTime;
};

export function localDateKey(day: Date): string {
  return `${day.getFullYear()}-${String(day.getMonth() + 1).padStart(2, "0")}-${String(day.getDate()).padStart(2, "0")}`;
}

export function shiftDateKey(date: string, days: number): string {
  const [year, month, day] = date.split("-").map(Number);
  return localDateKey(new Date(year, month - 1, day + days));
}

export function dayBounds(day: Date) {
  // Calendar arithmetic preserves 23/25-hour days across DST transitions.
  return {
    start_ms: new Date(
      day.getFullYear(),
      day.getMonth(),
      day.getDate(),
    ).getTime(),
    end_ms: new Date(
      day.getFullYear(),
      day.getMonth(),
      day.getDate() + 1,
    ).getTime(),
  };
}

function overlapsDay(
  event: DatedEvent,
  key: string,
  bounds: ReturnType<typeof dayBounds>,
): boolean {
  if (event.status === "cancelled") return false;
  if (event.start.kind === "all_day" && event.end.kind === "all_day") {
    return event.start.date <= key && event.end.date > key;
  }
  if (event.start.kind !== "timed" || event.end.kind !== "timed") return false;
  return (
    event.start.instant_ms < bounds.end_ms &&
    (event.end.instant_ms > bounds.start_ms ||
      (event.end.instant_ms === event.start.instant_ms &&
        event.start.instant_ms >= bounds.start_ms))
  );
}

export function hasEventsForDay(events: DatedEvent[], day: Date): boolean {
  const key = localDateKey(day);
  const bounds = dayBounds(day);
  return events.some((event) => overlapsDay(event, key, bounds));
}

export function eventsForDay<T extends DatedEvent>(
  events: T[],
  day: Date,
): T[] {
  const key = localDateKey(day);
  const bounds = dayBounds(day);
  return events
    .filter((event) => overlapsDay(event, key, bounds))
    .sort((a, b) => {
      if (a.start.kind === "all_day")
        return b.start.kind === "all_day" ? a.id.localeCompare(b.id) : -1;
      if (b.start.kind === "all_day") return 1;
      return (
        a.start.instant_ms - b.start.instant_ms || a.id.localeCompare(b.id)
      );
    });
}

export function isDayCovered(
  day: Date,
  interval: { start_ms: number; end_ms: number } | null,
  stale: boolean,
): boolean {
  if (!interval || stale) return false;
  const bounds = dayBounds(day);
  return (
    interval.start_ms <= bounds.start_ms && interval.end_ms >= bounds.end_ms
  );
}
