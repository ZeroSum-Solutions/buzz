# Calendar event model and view design (T12a)

Date: 2026-09-04. T12a `docs/calendar-view-design`, implemented by T12, bound by the accepted T11 contract (`docs/plans/2026-09-04-calendar-authorization.md`,
below as T11, cited by line). `shared/ui/calendar.tsx` is a `react-day-picker` wrapper with no event layer, used only by a single-date picker
(`desktop/src/features/user-status/ui/SetStatusDialog.tsx:18`). Authorization, caching and failure states stay T11's.

## Decisions

**1. Event model and capability.** Decision: a `CalendarEvent` DTO — `start`/`end` as `{ date }` or `{ dateTime, timeZone? }`, timed values normalized to an
instant plus source zone; `etag`, `status`, `recurringEventId`; `summary`/`location`/`description` capped at 256/256/4096 characters with a `truncated` flag.
Editability is `canEdit` and `canDelete` — T11 decision 3's `accessRole` narrowed by event type and organizer, revalidated as T11 decision 4's tuple at call
time. Reason: Google text is untrusted, the caps hold a row inside T11's 256 KiB bound, and one boolean expresses neither narrowing nor the edit/delete split.

**2. All-day events.** Decision: the `{ date }` variant is a half-open day range in the calendar's zone, never an instant, `end.date` exclusive, so a one-day
event renders on one day. Reason: converting it to UTC shifts the event a day for viewers east or west of that zone, unseen until someone misses a meeting.

**3. Multi-day events.** Decision: one event split into per-day segments at render, each carrying the event id and `isStart`/`isEnd`; a month week packs
segments into at most three lanes, a lane stable for one event across that week, the remainder collapsing into a "+N more" indicator in decision 6's overlay,
never inside a day button. Reason: a segment is a view artifact, so dedupe stays by event id, and the lane cap bounds nodes per week, not bytes.

**4. Timezone.** Decision: one display zone per render, the OS zone from `Intl.DateTimeFormat().resolvedOptions().timeZone`. Decision 2's day boundaries are
date-string arithmetic; `date-fns` (`desktop/package.json:66`) does only instant arithmetic in the display zone; no new time-zone dependency is added. A
differing source zone shows in the popover. Reason: `date-fns` v4 core computes in the system zone, which cannot express a calendar-zone boundary.

**5. Recurrence expansion window.** Decision: expansion happens at Google — `events.list` with `singleEvents=true`, `orderBy=startTime`, `timeMin`/`timeMax`
exactly T11 decision 6's window (30 days back, 90 ahead) under its 10-page, 8 MiB and 30-second caps; a walk ending against any cap is truncated, not complete,
and decision 13 governs it. No RRULE parser enters the fork; `recurringEventId` is display metadata. Reason: an in-app expander over a yearly rule with
exceptions is unbounded, and the server's window is the bound T11 sized the cache against.

**6. Month rendering.** Decision: keep `react-day-picker@^10.0.1` (`desktop/package.json:77`); a new `CalendarMonth` wrapper overrides one slot, `MonthGrid`,
forwarding `role="grid"` and the grid label to an inner `<table>`, leaving `Week` and `Day` to render the rows it measures, and painting decision 3's segments
and "+N more" in one absolutely positioned overlay beside that table. `Week`, `Day` and today's `DayButton` override (`desktop/src/shared/ui/calendar.tsx:154`,
component `:175`) stay untouched. Reason: a non-cell child of a `<table>` or `<tr>` breaks `role="grid"` row ownership, and a bar spanning days cannot be laid
out from inside one day's `<td>`, whose native `<button>` (`desktop/src/shared/ui/button.tsx:46`) nests no control.

**7. Agenda rendering.** Decision: a flat list of day-header and event rows, date-ascending, through `VirtualizedList`
(`desktop/src/shared/ui/VirtualizedList.tsx:72`, `@tanstack/react-virtual` at `desktop/package.json:52`); default under 640 px, a toggle above. A multi-day
event appears under each covered day as decision 3's segment; within a day, all-day and continuing rows sort first, then timed by start instant, ties by summary
then event id. `VirtualizedList` renders only the virtual window (`:133`) and emits no roles, so T12 adds role props: `role="list"` on the inner spacer (`:149`)
and `role="listitem"` with `aria-posinset`/`aria-setsize` on each row div (`:155`); day headers are rows and count in the set. Reason: T11 decision 6 permits
5,000 rows per partition, so an unvirtualized window is unbounded, and `aria-posinset` — the signal for unmounted rows — needs a set role a list owns directly.

**8. Paging.** Decision: month and agenda both stay inside T11 decision 6's fixed 120-day horizon; the month pages one month at a time and at the edge renders
an explicit end-of-window state — a named boundary row, not an empty month — with no further fetch and no auto-extension. Widening it needs T11 amended and is
out of v1. Reason: paging past the fetched window renders an empty month indistinguishable from an empty calendar, a swallowed boundary.

**9. Keyboard operation.** Decision: the month grid is one composite widget, one tab stop, roving focus — arrows by day, PageUp/PageDown by month, Home/End in
the week, Enter opening the day's agenda (also the path to "+N more"), `n` create, Escape out. The agenda is a second composite widget, one tab stop; its arrows
set the active row index, `scrollToIndex` through `onVirtualizer` (`VirtualizedList.tsx:69`), wait for the row to mount, then focus its named control; Enter
opens the row. Reason: Tab cannot reach an unmounted row; `scrollToIndex` scrolls without focusing, so the two must be sequenced.

**10. Screen-reader semantics.** Decision: keep the grid semantics `DayPicker` emits and name each day "«full date», N events" through its `labelDayButton`
formatter, since an interactive picker labels the button, not the `<td>`; decision 6's overlay is `aria-hidden` decoration and decision 7's agenda is the
authoritative reading order. Month and boundary changes announce through `DayPicker`'s own `role="status"` region, which the wrapper feeds rather than adding a
second announcer. Reason: an interactive list in a grid cell breaks grid semantics, and two live regions announce every change twice.

**11. Create and edit conflict handling.** Decision: `events.patch` sends only changed fields, and a field with decision 1's `truncated` flag set is read-only,
so a prefix never overwrites Google's copy. `events.insert` carries a client-generated event id — a UUID's 32 hex digits, lowercase and unhyphenated, inside
Google's base32hex 5-1024 alphabet — so a lost response is retryable with the same id; nothing retries a create automatically. `events.patch` and
`events.delete` carry the `etag` as `If-Match`, and a 412 refetches the event and shows both values. A 404/410 closes the form to "event no longer exists" only
if the mapped calendar probes clean; a probe reading T11 decision 8's terminal class purges, while a transient or ambiguous one (T11:66-68) rejects that command
alone and leaves a read-only view on the stale snapshot until `stale_after`. Reason: last-write-wins destroys a co-organizer's edit, an insert takes no
`If-Match`, Google reuses 404 for both, and a transient probe must not destroy the cache T11 sized for outages.

**12. Component and library chosen.** Decision: no calendar-widget dependency — decisions 4, 6 and 7's pieces plus the existing dialog primitive. Reason: they
are already in the tree and gated by the repository's checks, and a widget would add its own DOM, styling and date stack for a view with no time-grid.

**13. Truncated coverage.** Decision: a batch carries the interval it reached plus `complete` or `truncated(reason)`, and is never the authoritative snapshot
for dates it did not reach: those render unknown, not empty — the month marks the range partial and offers one retry, the agenda ends in decision 8's boundary
row — and its events are read-only. The marker lives outside the evictable rows, and T11's eviction (T11:50) downgrades each interval it touches to
`truncated(evicted)` in the transaction dropping the rows, so a snapshot reads `complete` only while every row substantiating it is retained; T11 owns that
write, T12a requires it. Reason: neither cap exhaustion (200s) nor eviction (no HTTP at all) has a branch in T11 decision 8's status-keyed matrices, so either
paints a partial window as a finished calendar — the defect `AGENTS.md` Review-Proven Rule 1 names.

## Open verifications

- The ARIA roles `react-day-picker@^10.0.1` emits for grid and day cells, and that a `MonthGrid` override keeps them, read off T12's rendered DOM.
- That `labelDayButton` names an interactive picker's days, and that feeding `DayPicker`'s own status region announces each change exactly once.
- Lane and "+N more" thresholds and `VirtualizedList`'s `estimateSize` on T12's 250-event fixture; agenda focus traversal at T11's 5,000-row bound.
- That Google honours `If-Match` (412 on a stale etag) and a client-supplied `events.insert` id on replay — T12's mock server first, the live checklist once.

## Risks accepted

- No week or day time-grid in v1: several timed events in one day render as an ordered list, so overlap is readable but not visible as geometry.
- Decision 8 gives v1 no way to see past T11's 120-day horizon; a date outside it is reachable only in Google's own UI until T11 is amended.
- Decision 13's `truncated(evicted)` downgrade is a T11-side write; until it lands, an evicted interval can still paint empty inside `stale_after`.
- The 4096-character `description` cap means long bodies render elided and, per decision 11, are not editable in Buzz at all.
- `Intl` reads the OS tz database, so a machine with stale tzdata mis-renders events across a zone-rule change until the OS is updated.
- To a screen reader the month grid carries counts, not events, and "+N more" has no tab stop; decision 9's Enter opens the same day agenda for both.
