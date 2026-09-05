# Calendar event model and view design (T12a)

Date: 2026-09-04. T12a `docs/calendar-view-design`, implemented by T12 `feat/google-calendar`, bound by the accepted T11 contract
(`docs/plans/2026-09-04-calendar-authorization.md`). Today `shared/ui/calendar.tsx` is a `react-day-picker` wrapper with no event layer whose only consumer is
a single-date picker (`desktop/src/features/user-status/ui/SetStatusDialog.tsx:18`). Authorization, caching and failure states stay T11's.

## Decisions

**1. Event model, caps and capability.** Decision: a `CalendarEvent` DTO — opaque id; `start`/`end` as `{ date }` or `{ dateTime, timeZone? }`, the timed
variant normalized natively to an instant plus optional source zone; `etag`, `status`, `recurringEventId`; and `summary`/`location`/`description` capped
natively at 256/256/4096 UTF-8 characters, each with a `truncated` flag. Editability is `canEdit` and `canDelete`, from T11 decision 3's `accessRole` narrowed
by event type and organizer, revalidated as T11 decision 4's tuple at call time. Reason: Google text is untrusted, the caps keep a row inside T11 decision 6's
256 KiB bound, and one boolean expresses neither narrowing (T11 decision 8 rejects `forbiddenForNonOrganizer`) nor edit-versus-delete asymmetry.

**2. All-day events.** Decision: the `{ date }` variant is a half-open day range in the calendar's own zone, never converted to an instant, with `end.date`
exclusive, so a one-day event renders on one day. Reason: converting an all-day date to UTC shifts it a day for any viewer east or west of the calendar zone,
and the shift is invisible until someone misses a meeting.

**3. Multi-day events.** Decision: one event split into per-day segments at render only, each carrying the event id plus `isStart`/`isEnd`; a month week packs
segments into at most three lanes, a lane stable for one event across that week, and the remainder collapses into a "+N more" indicator drawn in decision 6's
week layer, never inside a day button. Reason: a segment is a view artifact, so dedupe stays by event id, and the lane cap bounds nodes per week — the
quantity that costs — instead of bytes.

**4. Timezone.** Decision: one display zone per render, the OS zone from `Intl.DateTimeFormat().resolvedOptions().timeZone`, formatted through `Intl`.
Decision 2's day boundaries are plain calendar-date arithmetic on the `{ date }` strings with no conversion; `date-fns` (`desktop/package.json:66`) does only
instant arithmetic in the display zone, and no new time-zone dependency is added. An event whose source zone differs shows that zone in its popover. Reason:
`date-fns` v4 core computes in the system zone, so a calendar-zone boundary is not a conversion it can do — it is a date-string question.

**5. Recurrence expansion window.** Decision: expansion happens at Google — `events.list` with `singleEvents=true` and `orderBy=startTime`,
`timeMin`/`timeMax` exactly T11 decision 6's window (30 days back, 90 ahead) under its 10-page, 8 MiB and 30-second caps; a walk that ends against any cap is
truncated, not complete, and decision 13 governs it. No RRULE parser enters the fork; `recurringEventId` is display metadata. Reason: an in-app expander over
a yearly rule with exceptions is unbounded, and the server's window is the bound T11 decision 6 already sized the cache against.

**6. Month rendering.** Decision: keep `react-day-picker@^10.0.1` (`desktop/package.json:77`) for grid, month math and day chrome, and draw the event layer
from a new `CalendarMonth` wrapper overriding the `Week`/`MonthGrid` slots, so segments and the "+N more" indicator are siblings of the day buttons; today's
`DayButton` override (`desktop/src/shared/ui/calendar.tsx:154`, component at `:175`) stays untouched. Reason: that override renders a native `<button>`
(`desktop/src/shared/ui/button.tsx:46`) inside its own `<td>`, so a nested control is invalid HTML and a bar spanning days cannot be laid out from one day.

**7. Agenda rendering.** Decision: a flat list of day-header and event rows, date-ascending, through `VirtualizedList`
(`desktop/src/shared/ui/VirtualizedList.tsx:72`, `@tanstack/react-virtual` at `desktop/package.json:52`); default under a 640 px viewport, a toggle above. A
multi-day event appears under each covered day as decision 3's segment; within a day, all-day and continuing rows sort before timed rows, timed rows by start
instant, ties by summary then event id. Since `VirtualizedList` renders only the virtual window (`:133`) and emits no roles, the agenda wrapper supplies
`role="list"` and per-row `aria-posinset`/`aria-setsize`. Reason: T11 decision 6 permits 5,000 rows per partition, so an unvirtualized window is unbounded
nodes, and set position is what tells a screen reader about the rows that are not mounted.

**8. Paging.** Decision: month and agenda both stay inside T11 decision 6's fixed 120-day horizon; the month pages one month at a time and at the horizon edge
renders an explicit end-of-window state — a named boundary row, not an empty month — with no further fetch and no auto-extension. Widening the horizon needs
T11 decision 6 amended and is out of v1. Reason: paging silently past the fetched window renders an empty month indistinguishable from a genuinely empty
calendar, a swallowed boundary; a shifted `timeMin`/`timeMax` would read across the horizon T11 fixed.

**9. Keyboard operation.** Decision: the month grid is one composite widget with a single tab stop and roving focus — arrows by day, PageUp/PageDown by month,
Home/End to the week's ends, Enter opens the focused day's agenda (also the keyboard path to decision 3's "+N more"), `n` starts a create, Escape returns
focus to the opener. The agenda is a second composite widget, one tab stop, arrows moving by row through `VirtualizedList`'s `onVirtualizer` `scrollToIndex`
(`desktop/src/shared/ui/VirtualizedList.tsx:69`), Enter opening the row. Reason: Tab cannot reach a row virtualization has not mounted.

**10. Screen-reader semantics.** Decision: keep the grid semantics `DayPicker` emits and name each day "«full date», N events" through its `labelDayButton`
formatter, since an interactive picker labels the button, not the `<td>`; the in-cell segment layer is `aria-hidden` decoration and decision 7's agenda is the
authoritative reading order. Month and boundary changes announce through `DayPicker`'s own `role="status"` region, which the wrapper feeds rather than adding
a second announcer. Reason: an interactive list in a grid cell breaks grid semantics, and two live regions announce every month change twice.

**11. Create and edit conflict handling.** Decision: `events.patch` sends only changed fields, and a field with decision 1's `truncated` flag set is
read-only, so a prefix never overwrites Google's copy. `events.insert` carries a client-generated event id, so a lost response is retryable with the same id;
nothing retries a create automatically. `events.patch` and `events.delete` carry the `etag` as `If-Match`, and a 412 refetches that event and shows both
values. A 404/410 closes the form to "event no longer exists" only if the mapped calendar probes clean; otherwise, or when the probe is ambiguous, it is T11
decision 8's terminal class and purges. Reason: last-write-wins destroys a co-organizer's edit, an insert takes no `If-Match`, and Google reuses 404 for both.

**12. Component and library chosen.** Decision: no calendar-widget dependency is added — `react-day-picker` for the month grid, `VirtualizedList` over
`@tanstack/react-virtual` for agenda, `date-fns` plus `Intl` for time, the existing dialog primitive for create and edit. Reason: those pieces are already in
the tree and gated by the repository's checks, and a calendar widget would bring its own DOM, styling and date stack for a view that ships no time-grid.

**13. Truncated coverage.** Decision: a batch carries the interval it reached plus `complete` or `truncated(reason)` from decision 5's caps, and a truncated
batch is never the authoritative snapshot for dates it did not reach: they render as unknown, not empty — the month marks the range partial and offers one
explicit retry, the agenda ends in decision 8's boundary row — and its events are read-only. Reason: cap exhaustion returns 200s, so T11 decision 8's
status-keyed matrices have no branch for it, and painting a truncated window as a finished calendar is the defect `AGENTS.md` Review-Proven Rule 1 names.

## Open verifications

- The ARIA roles `react-day-picker@^10.0.1` emits for its grid and day cells, and that `Week`/`MonthGrid` overrides keep them, read off T12's rendered DOM.
- That `labelDayButton` is the naming seam for an interactive picker, and that feeding `DayPicker`'s own status region announces each change exactly once.
- The three-lane and "+N more" threshold, and `VirtualizedList`'s `estimateSize` for an event row, measured against T12's 250-event fixture, not assumed.
- That Google honours `If-Match` (412 on a stale etag) and a client-supplied `events.insert` id on replay — T12's mock server first, the live checklist once.

## Risks accepted

- No week or day time-grid in v1: several timed events in one day render as an ordered list, so overlap is readable but not visible as geometry.
- Decision 8 gives v1 no way to see past T11's 120-day horizon; a date outside it is reachable only in Google's own UI until T11 decision 6 is amended.
- The 4096-character `description` cap means long bodies render elided and, per decision 11, are not editable in Buzz at all.
- `Intl` reads the OS tz database, so a machine with stale tzdata mis-renders events across a zone-rule change until the OS is updated.
- To a screen reader the month grid carries counts, not events, and "+N more" has no tab stop; decision 9's Enter opens the same day agenda for both.
