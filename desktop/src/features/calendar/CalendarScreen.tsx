import * as React from "react";
import { Link } from "@tanstack/react-router";
import { CalendarDays, Plus, RefreshCw, Settings } from "lucide-react";
import {
  createCalendarEvent,
  deleteCalendarEvent,
  updateCalendarEvent,
  type CalendarEvent,
  type CalendarEventFields,
} from "@/shared/api/tauriCalendar";
import { useIdentityQuery } from "@/shared/api/hooks";
import { Button } from "@/shared/ui/button";
import { Calendar } from "@/shared/ui/calendar";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/shared/ui/dialog";
import { CalendarEventList } from "./CalendarEventList";
import { CalendarConnectionPanel } from "./CalendarConnectionPanel";
import { CalendarEventForm } from "./CalendarEventForm";
import {
  eventsForDay,
  hasEventsForDay,
  isDayCovered,
  shiftDateKey,
} from "./calendarDates";
import { useCalendar } from "./useCalendar";

export function CalendarScreen() {
  const identity = useIdentityQuery();
  const calendar = useCalendar();
  if (!identity.data || identity.data.locked)
    return (
      <p role="status" className="p-6">
        Unlock Buzz to view your calendar.
      </p>
    );
  return (
    <CalendarView
      key={`${identity.data.pubkey}:${calendar.status.data?.generation ?? "none"}`}
      calendar={calendar}
    />
  );
}

function CalendarView({
  calendar,
}: {
  calendar: ReturnType<typeof useCalendar>;
}) {
  const [day, setDay] = React.useState(() => new Date());
  const [month, setMonth] = React.useState(day);
  const [mode, setMode] = React.useState<"month" | "agenda">("month");
  const [selected, setSelected] = React.useState<CalendarEvent | null>(null);
  const [editing, setEditing] = React.useState(false);
  const [creating, setCreating] = React.useState(false);
  const [eventId, setEventId] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  const snapshot = calendar.events.data;
  const generation = calendar.status.data?.generation;
  const stale =
    !snapshot ||
    snapshot.stale ||
    calendar.status.isError ||
    calendar.events.isError ||
    snapshot.generation !== generation;
  const events =
    snapshot && snapshot.generation === generation ? snapshot.events : [];
  const visible =
    mode === "month"
      ? eventsForDay(events, day)
      : events.filter((event) => event.status !== "cancelled");
  const covered = isDayCovered(day, snapshot?.interval ?? null, stale);
  const today = new Date();
  const firstDay = new Date(
    today.getFullYear(),
    today.getMonth(),
    today.getDate() - 30,
  );
  const lastDay = new Date(
    today.getFullYear(),
    today.getMonth(),
    today.getDate() + 89,
  );
  async function save(fields: Partial<CalendarEventFields>) {
    if (generation === null || generation === undefined || stale) return;
    setBusy(true);
    setError(null);
    try {
      if (creating)
        await createCalendarEvent({
          expectedIdentity: calendar.expectedIdentity,
          expectedGeneration: generation,
          eventId,
          fields: fields as CalendarEventFields,
        });
      else if (selected?.etag)
        await updateCalendarEvent({
          expectedIdentity: calendar.expectedIdentity,
          expectedGeneration: generation,
          eventId: selected.id,
          etag: selected.etag,
          fields,
        });
      setCreating(false);
      setEditing(false);
      setSelected(null);
      await calendar.refresh();
    } catch (cause) {
      setError(String(cause));
    } finally {
      setBusy(false);
    }
  }
  async function remove() {
    if (
      !selected?.etag ||
      generation === null ||
      generation === undefined ||
      stale
    )
      return;
    setBusy(true);
    setError(null);
    try {
      await deleteCalendarEvent({
        expectedIdentity: calendar.expectedIdentity,
        expectedGeneration: generation,
        eventId: selected.id,
        etag: selected.etag,
      });
      setSelected(null);
      await calendar.refresh();
    } catch (cause) {
      setError(String(cause));
    } finally {
      setBusy(false);
    }
  }
  return (
    <main className="flex h-full min-h-0 flex-col" aria-label="Calendar">
      <header className="flex flex-wrap items-center justify-between gap-3 border-b px-6 py-4">
        <div>
          <h1 className="flex items-center gap-2 text-lg font-semibold">
            <CalendarDays className="h-5 w-5" />
            Calendar
          </h1>
          <p className="text-sm text-muted-foreground">
            {calendar.status.data?.email ?? "Your personal schedule"}
          </p>
        </div>
        <div className="flex items-center gap-2">
          {calendar.status.data?.connected ? (
            <>
              <Button
                variant="outline"
                disabled={calendar.events.isFetching}
                onClick={() => void calendar.refresh()}
                aria-label="Refresh calendar"
              >
                <RefreshCw className="h-4 w-4" />
              </Button>
              <Button
                disabled={stale}
                onClick={() => {
                  setEventId(crypto.randomUUID().replaceAll("-", ""));
                  setCreating(true);
                  setError(null);
                }}
              >
                <Plus className="mr-1 h-4 w-4" />
                New event
              </Button>
            </>
          ) : null}
          <Button variant="ghost" asChild>
            <Link
              to="/settings"
              search={{ section: "calendar" }}
              aria-label="Calendar settings"
            >
              <Settings className="h-4 w-4" />
            </Link>
          </Button>
        </div>
      </header>
      {!calendar.status.data?.connected ? (
        <CalendarConnectionPanel />
      ) : (
        <>
          <div className="flex flex-wrap items-center gap-2 px-6 py-3">
            <Button
              variant={mode === "month" ? "secondary" : "ghost"}
              aria-pressed={mode === "month"}
              onClick={() => setMode("month")}
            >
              Month
            </Button>
            <Button
              variant={mode === "agenda" ? "secondary" : "ghost"}
              aria-pressed={mode === "agenda"}
              onClick={() => setMode("agenda")}
            >
              Agenda
            </Button>
            <Button
              variant="ghost"
              onClick={() => {
                const now = new Date();
                setDay(now);
                setMonth(now);
              }}
            >
              Today
            </Button>
            <span className="ml-auto text-xs text-muted-foreground">
              {Intl.DateTimeFormat().resolvedOptions().timeZone}
            </span>
          </div>
          {calendar.events.isPending ? (
            <p className="px-6 py-3 text-sm" role="status">
              Loading your calendar…
            </p>
          ) : null}
          {calendar.events.error ? (
            <p className="px-6 py-3 text-sm text-destructive" role="alert">
              Calendar refresh failed: {String(calendar.events.error)}
            </p>
          ) : null}
          {snapshot &&
          (stale || snapshot.interval?.coverage.coverage === "truncated") ? (
            <p
              role="status"
              className="px-6 py-3 text-sm text-muted-foreground"
            >
              {stale
                ? "These events may be out of date. Refresh before making changes."
                : "Only part of this calendar was loaded. Additional events may be missing."}
            </p>
          ) : null}
          <div className="flex min-h-0 flex-1 flex-col gap-4 px-6 pb-6 md:flex-row">
            {mode === "month" ? (
              <div className="shrink-0">
                <Calendar
                  mode="single"
                  required
                  selected={day}
                  month={month}
                  onMonthChange={setMonth}
                  onSelect={setDay}
                  startMonth={firstDay}
                  endMonth={lastDay}
                  disabled={{ before: firstDay, after: lastDay }}
                  modifiers={{
                    hasEvents: (date) => hasEventsForDay(events, date),
                  }}
                  modifiersClassNames={{
                    hasEvents:
                      "font-semibold underline decoration-primary underline-offset-4",
                  }}
                />
              </div>
            ) : null}
            <section
              className="flex min-h-0 min-w-0 flex-1 flex-col rounded-lg border"
              aria-label={
                mode === "month" ? "Events on selected day" : "Calendar agenda"
              }
            >
              <h2 className="border-b px-4 py-3 text-sm font-medium">
                {mode === "month"
                  ? day.toLocaleDateString(undefined, {
                      weekday: "long",
                      month: "long",
                      day: "numeric",
                    })
                  : "Past 30 days and next 90 days"}
              </h2>
              {visible.length === 0 ? (
                <p className="p-6 text-sm text-muted-foreground">
                  {covered && mode === "month"
                    ? "No events on this day."
                    : !stale &&
                        snapshot?.interval?.coverage.coverage === "complete" &&
                        mode === "agenda"
                      ? "No events in this period."
                      : "No events loaded for this view yet."}
                </p>
              ) : (
                <CalendarEventList
                  events={visible}
                  onSelect={(event) => {
                    setSelected(event);
                    setEditing(false);
                    setError(null);
                  }}
                />
              )}
            </section>
          </div>
        </>
      )}
      <Dialog
        open={creating || !!selected}
        onOpenChange={(open) => {
          if (!open && !busy) {
            setCreating(false);
            setSelected(null);
            setEditing(false);
            setError(null);
          }
        }}
      >
        <DialogContent className="max-h-[85vh] overflow-y-auto">
          <DialogHeader>
            <DialogTitle>
              {creating
                ? "New event"
                : editing
                  ? "Edit event"
                  : selected?.summary.value || "Event"}
            </DialogTitle>
            <DialogDescription>
              {creating || editing
                ? "Times use your local time zone."
                : selected?.recurring_event_id
                  ? "This occurrence of a recurring event"
                  : "Event details"}
            </DialogDescription>
          </DialogHeader>
          {error ? (
            <p role="alert" className="text-sm text-destructive">
              {error}
            </p>
          ) : null}
          {creating || editing ? (
            <CalendarEventForm
              key={creating ? eventId : selected?.id}
              event={creating ? undefined : (selected ?? undefined)}
              day={day}
              busy={busy || stale}
              onSave={(fields) => void save(fields)}
              onCancel={() => {
                setCreating(false);
                setEditing(false);
              }}
            />
          ) : selected ? (
            <div className="space-y-4">
              <p className="text-sm">
                {selected.start.kind === "all_day"
                  ? `${selected.start.date} through ${selected.end.kind === "all_day" ? shiftDateKey(selected.end.date, -1) : ""}`
                  : new Date(selected.start.instant_ms).toLocaleString()}
              </p>
              <p className="whitespace-pre-wrap break-words text-sm">
                {selected.location.value}
              </p>
              <p className="whitespace-pre-wrap break-words text-sm">
                {selected.description.value}
              </p>
              <div className="flex justify-end gap-2">
                <Button
                  variant="outline"
                  disabled={
                    busy || stale || !selected.can_delete || !selected.etag
                  }
                  onClick={() => void remove()}
                >
                  Delete event
                </Button>
                <Button
                  disabled={
                    busy || stale || !selected.can_edit || !selected.etag
                  }
                  onClick={() => setEditing(true)}
                >
                  Edit event
                </Button>
              </div>
            </div>
          ) : null}
        </DialogContent>
      </Dialog>
    </main>
  );
}
