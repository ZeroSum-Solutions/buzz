# Calendar event model and view design (T12a)

Date: 2026-09-04. T12a `docs/calendar-view-design`, implemented by T12 `feat/google-calendar`, bound by the accepted T11 contract
(`docs/plans/2026-09-04-calendar-authorization.md`). Today `shared/ui/calendar.tsx` is a `react-day-picker` wrapper with no event layer, and its only
consumer is a single-date picker (`desktop/src/features/user-status/ui/SetStatusDialog.tsx:18`). Every decision below is a view decision; authorization,
caching and failure states stay T11's.

## Decisions

**1. Event model and string caps.** Decision: the native command returns a `CalendarEvent` DTO — opaque event id, `start`/`end` as either `{ date }` or
`{ dateTime, timeZone }`, `etag`, `status`, `recurringEventId`, and `summary`/`location`/`description` truncated in Rust to 256, 256 and 4096 UTF-8
characters before serialization; editability is a boolean derived natively from T11 decision 3's `accessRole`, never from a field the webview supplies.
Reason: Google text is untrusted at the webview boundary, capping belongs at the DTO, and the cap keeps a cached row inside T11 decision 6's 256 KiB bound.

**2. All-day events.** Decision: the `{ date }` variant is a half-open day range in the calendar's own zone and is never converted to an instant;
`end.date` is treated as exclusive, so a one-day event renders on one day. Reason: converting an all-day date to UTC shifts it a day for any viewer east or
west of the calendar zone, and the shift is invisible until someone misses a meeting.

**3. Multi-day events.** Decision: one event, split into per-day segments at render only, each segment carrying the event id plus `isStart`/`isEnd`; a
month week lays segments into at most three lanes, and the remainder collapses into a "+N more" control that opens that day's agenda. Reason: a segment is a
view artifact, so dedupe stays by event id, and the lane cap bounds nodes per week — the quantity that costs — instead of bytes.

**4. Timezone.** Decision: one display zone per render, the OS zone from `Intl.DateTimeFormat().resolvedOptions().timeZone`; formatting through `Intl`,
arithmetic through the `date-fns` already in the tree (`desktop/package.json:66`), and no new time-zone dependency. An event whose `start.timeZone` differs
from the display zone shows that zone in its detail popover. Reason: `Intl` is the platform's tz database, so a second date stack adds drift, not accuracy.

**5. Recurrence expansion window.** Decision: expansion happens at Google, not in Buzz — `events.list` with `singleEvents=true` and `orderBy=startTime`,
`timeMin`/`timeMax` set to exactly T11 decision 6's window (30 days back, 90 ahead) under its 10-page, 8 MiB and 30-second caps. No RRULE parser enters the
fork; `recurringEventId` is display metadata only. Reason: an in-app expander over a yearly rule with exceptions is unbounded, and the server's window is
the same bound T11 decision 6 already sized the render cache against.

**6. Month rendering.** Decision: keep `react-day-picker@^10.0.1` (`desktop/package.json:77`) for the grid, month math and day chrome, and hang the event
layer off the existing `DayButton` override seam (`desktop/src/shared/ui/calendar.tsx:154`, component at `:175`) from a new `CalendarMonth` wrapper rather
than by editing today's shared `Calendar`. Reason: the grid and its focus behaviour are the solved part, and a new wrapper adds a caller instead of
changing the one the status dialog already depends on.

**7. Agenda rendering.** Decision: a flat list of day-header and event rows, date-ascending, rendered through the existing `VirtualizedList`
(`desktop/src/shared/ui/VirtualizedList.tsx:72`, `@tanstack/react-virtual` at `desktop/package.json:52`); agenda is the default under a 640 px viewport and
a toggle everywhere else. Reason: T11 decision 6 permits 5,000 rows per partition, so an unvirtualized list of the window is a node count nobody bounded.

**8. Paging.** Decision: the month view pages one whole month at a time and stops at the edge of the fetched window, where it shows an explicit "Load
earlier"/"Load later" control that issues a fresh `events.list` with a shifted `timeMin`/`timeMax` under the same T11 decision 6 caps; agenda scrolls the
window and never auto-extends it. Reason: paging silently past the fetched window renders an empty month that is indistinguishable from a genuinely empty
calendar, which is a swallowed boundary, not a UI nicety.

**9. Keyboard operation.** Decision: the month grid is one composite widget with a single tab stop and roving focus — arrows move by day, PageUp/PageDown
by month, Home/End to the week's ends, Enter opens the focused day's agenda, `n` starts a create on the focused day, Escape returns focus to the element
that opened the popover. Agenda is a plain list: Tab reaches each event row, Enter opens it. Reason: 42 tab stops per month is exactly the failure the
composite-widget pattern exists to prevent.

**10. Screen-reader semantics.** Decision: keep the grid semantics `DayPicker` emits and label each day cell "«full date», N events"; the in-cell event
layer is `aria-hidden` decoration, the agenda list (`role="list"`) is the authoritative reading order, and month, view and load-more changes announce once
through a single polite live region, following `desktop/src/features/home/ui/InboxListPane.tsx:460`. Reason: an interactive list inside a grid cell breaks
grid semantics, so the agenda is the accessible equivalent view, not a degraded fallback.

**11. Create and edit conflict handling.** Decision: mutations carry the DTO's `etag` as `If-Match` on `events.patch` and `events.delete`; a 412 refetches
that one event and re-presents the form with the server's values beside the user's edits, discarding neither, and a 404/410 closes the form to an "event no
longer exists" state. The command revalidates T11 decision 4's whole tuple at call time and T11 decision 8's mutation default rejects that call alone.
Reason: last-write-wins silently destroys a co-organizer's edit, and a disabled button is a hint, never the gate.

**12. Component and library chosen.** Decision: no calendar-widget dependency is added — `react-day-picker` for the month grid, `VirtualizedList` over
`@tanstack/react-virtual` for agenda, `date-fns` plus `Intl` for time, and the existing dialog primitive for create and edit. Reason: those pieces are
already in the tree and already gated by the repository's checks, and a calendar widget would bring its own DOM, styling and date stack for a view that
ships no week or day time-grid.

## Open verifications

- The exact ARIA roles `react-day-picker@^10.0.1` emits for its month grid and day cells, read off the rendered DOM in T12's test, before decision 10's
  labels are written against them.
- That a `DayButton` override can host the decision 3 segment layer without the grid losing those roles.
- The three-lane and "+N more" threshold, and `VirtualizedList`'s `estimateSize` for an event row, measured against T12's 250-event fixture rather than
  assumed.
- That Google honours `If-Match` with a 412 on `events.patch` for a stale etag — asserted first against T12's mock server, then once in the live checklist.

## Risks accepted

- No week or day time-grid in v1: several timed events in one day render as an ordered list, so overlap is readable but not visible as geometry.
- Decision 8's shifted-window fetch is a second `events.list` per user action; T11 decision 6's per-fetch caps bound each one, and the partition's row and
  byte bounds — not the fetch count — are what stop the cache from growing.
- A 4096-character `description` cap means long event bodies render elided, with the full text only in Google's own UI.
- `Intl` reads the OS tz database, so a machine with stale tzdata mis-renders events across a zone-rule change until the OS is updated.
- To a screen reader the month grid carries event counts, not events; a user who never opens the agenda sees how many, not which.
