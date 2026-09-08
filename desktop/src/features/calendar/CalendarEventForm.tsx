import * as React from "react";
import type {
  CalendarEvent,
  CalendarEventFields,
} from "@/shared/api/tauriCalendar";
import { Button } from "@/shared/ui/button";
import { Input } from "@/shared/ui/input";
import { Textarea } from "@/shared/ui/textarea";
import { localDateKey, shiftDateKey } from "./calendarDates";

function localInput(instant: number) {
  const day = new Date(instant);
  return `${localDateKey(day)}T${String(day.getHours()).padStart(2, "0")}:${String(day.getMinutes()).padStart(2, "0")}`;
}

export function CalendarEventForm({
  event,
  day,
  busy,
  onSave,
  onCancel,
}: {
  event?: CalendarEvent;
  day: Date;
  busy: boolean;
  onSave: (fields: Partial<CalendarEventFields>) => void;
  onCancel: () => void;
}) {
  const [summary, setSummary] = React.useState(event?.summary.value ?? "");
  const [location, setLocation] = React.useState(event?.location.value ?? "");
  const [description, setDescription] = React.useState(
    event?.description.value ?? "",
  );
  const [allDay, setAllDay] = React.useState(event?.start.kind === "all_day");
  const [start, setStart] = React.useState(
    event
      ? event.start.kind === "all_day"
        ? event.start.date
        : localInput(event.start.instant_ms)
      : `${localDateKey(day)}T09:00`,
  );
  const [end, setEnd] = React.useState(
    event
      ? event.end.kind === "all_day"
        ? shiftDateKey(event.end.date, -1)
        : localInput(event.end.instant_ms)
      : `${localDateKey(day)}T10:00`,
  );
  const [error, setError] = React.useState<string | null>(null);
  function submit(e: React.FormEvent) {
    e.preventDefault();
    const originalStart = event
      ? event.start.kind === "all_day"
        ? event.start.date
        : localInput(event.start.instant_ms)
      : null;
    const originalEnd = event
      ? event.end.kind === "all_day"
        ? shiftDateKey(event.end.date, -1)
        : localInput(event.end.instant_ms)
      : null;
    const startChanged =
      !event ||
      start !== originalStart ||
      allDay !== (event.start.kind === "all_day");
    const endChanged =
      !event ||
      end !== originalEnd ||
      allDay !== (event.end.kind === "all_day");
    // Use untouched provider instants before validation: display precision and
    // repeated DST wall times cannot reconstruct their original ordering.
    const startTime =
      event && !startChanged
        ? event.start
        : allDay
          ? { kind: "all_day" as const, date: start }
          : { kind: "timed" as const, instant_ms: new Date(start).getTime() };
    const endTime =
      event && !endChanged
        ? event.end
        : allDay
          ? { kind: "all_day" as const, date: shiftDateKey(end, 1) }
          : { kind: "timed" as const, instant_ms: new Date(end).getTime() };
    for (const [changed, time, input] of [
      [startChanged, startTime, start],
      [endChanged, endTime, end],
    ] as const) {
      if (
        changed &&
        time.kind === "timed" &&
        Number.isFinite(time.instant_ms) &&
        localInput(time.instant_ms) !== input.slice(0, 16)
      ) {
        setError(
          "This local time does not exist because the clocks change. Choose another time.",
        );
        return;
      }
    }
    if (
      !summary.trim() ||
      !start ||
      !end ||
      (startTime.kind === "all_day" && endTime.kind === "all_day"
        ? endTime.date <= startTime.date
        : startTime.kind !== "timed" ||
          endTime.kind !== "timed" ||
          !Number.isFinite(startTime.instant_ms) ||
          !Number.isFinite(endTime.instant_ms) ||
          endTime.instant_ms <= startTime.instant_ms)
    ) {
      setError("Add a title and an end after the start.");
      return;
    }
    const fields: Partial<CalendarEventFields> = {};
    if (!event?.summary.truncated && summary !== event?.summary.value)
      fields.summary = summary;
    if (!event?.location.truncated && location !== event?.location.value)
      fields.location = location;
    if (
      !event?.description.truncated &&
      description !== event?.description.value
    )
      fields.description = description;
    if (startChanged) fields.start = startTime;
    if (endChanged) fields.end = endTime;
    if (event && (fields.start || fields.end)) {
      fields.start ??= event.start;
      fields.end ??= event.end;
    }
    setError(null);
    onSave(fields);
  }
  return (
    <form onSubmit={submit} className="space-y-4">
      <div className="space-y-2">
        <label className="text-sm font-medium" htmlFor="calendar-title">
          Title
        </label>
        <Input
          id="calendar-title"
          value={summary}
          onChange={(e) => setSummary(e.target.value)}
          maxLength={256}
          disabled={busy || event?.summary.truncated}
          required
        />
      </div>
      <label className="flex items-center gap-2 text-sm">
        <input
          type="checkbox"
          checked={allDay}
          disabled={busy}
          onChange={(e) => {
            setAllDay(e.target.checked);
            setStart(e.target.checked ? start.slice(0, 10) : `${start}T09:00`);
            setEnd(e.target.checked ? end.slice(0, 10) : `${end}T10:00`);
          }}
        />
        All day
      </label>
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <div className="space-y-2">
          <label className="text-sm font-medium" htmlFor="calendar-start">
            Start
          </label>
          <Input
            id="calendar-start"
            type={allDay ? "date" : "datetime-local"}
            value={start}
            onChange={(e) => setStart(e.target.value)}
            disabled={busy}
            required
          />
        </div>
        <div className="space-y-2">
          <label className="text-sm font-medium" htmlFor="calendar-end">
            {allDay ? "Last day" : "End"}
          </label>
          <Input
            id="calendar-end"
            type={allDay ? "date" : "datetime-local"}
            value={end}
            onChange={(e) => setEnd(e.target.value)}
            disabled={busy}
            required
          />
        </div>
      </div>
      <div className="space-y-2">
        <label className="text-sm font-medium" htmlFor="calendar-location">
          Location
        </label>
        <Input
          id="calendar-location"
          value={location}
          onChange={(e) => setLocation(e.target.value)}
          maxLength={256}
          disabled={busy || event?.location.truncated}
        />
      </div>
      <div className="space-y-2">
        <label className="text-sm font-medium" htmlFor="calendar-description">
          Notes
        </label>
        <Textarea
          id="calendar-description"
          value={description}
          onChange={(e) => setDescription(e.target.value)}
          maxLength={4096}
          disabled={busy || event?.description.truncated}
        />
      </div>
      {event &&
      [event.summary, event.location, event.description].some(
        (field) => field.truncated,
      ) ? (
        <p className="text-sm text-muted-foreground">
          Long fields are shown in part and cannot be edited here.
        </p>
      ) : null}
      {error ? (
        <p role="alert" className="text-sm text-destructive">
          {error}
        </p>
      ) : null}
      <div className="flex justify-end gap-2">
        <Button
          type="button"
          variant="outline"
          disabled={busy}
          onClick={onCancel}
        >
          Cancel
        </Button>
        <Button type="submit" disabled={busy}>
          {busy ? "Saving…" : "Save event"}
        </Button>
      </div>
    </form>
  );
}
