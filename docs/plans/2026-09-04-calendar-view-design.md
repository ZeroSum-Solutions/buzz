# Calendar event model and view design (T12a)

Date: 2026-09-04. T12a `docs/calendar-view-design`, implemented by T12, bound by the accepted T11 contract (`docs/plans/2026-09-04-calendar-authorization.md`, cited below by line).
`shared/ui/calendar.tsx` is a `react-day-picker` wrapper with no event layer, used only by a single-date picker (`desktop/src/features/user-status/ui/SetStatusDialog.tsx:18`). Authorization, caching
and failure states stay T11's.

## Decisions

**1. Event model and capability.** Decision: a `CalendarEvent` DTO — Google's opaque `id`, which later decisions address it by; `start`/`end` as `{ date }` or `{ dateTime, timeZone? }`, timed values
normalized to an instant plus source zone; `etag`, `status`, `recurringEventId`; `summary`, `location` and `description` each `{ value, truncated }`, capped at 256/256/4096 characters — one flag per
field, none per DTO. Editability is `canEdit` and `canDelete`: T11 decision 3's `accessRole` narrowed by event type and organizer, revalidated as T11 decision 4's tuple at call time. Reason: Google
text is untrusted, the caps hold a row inside T11's 256 KiB bound, and one boolean hides both the narrowing and the edit/delete split.

**2. All-day events.** Decision: the `{ date }` variant is a half-open day range in the calendar's zone, never an instant, `end.date` exclusive, so a one-day event renders on one day. Reason:
converting it to UTC shifts the event a day either side of that zone, unseen until someone misses a meeting.

**3. Multi-day events.** Decision: one event split into per-day segments at render, each carrying the event id and `isStart`/`isEnd`; a month week packs segments into at most three lanes, a lane
stable for one event across that week, the rest collapsing into a "+N more" indicator in decision 6's overlay, never inside a day button. The split is a month-grid artifact only. Reason: a segment is
a view artifact, so dedupe stays by event id, and the lane cap bounds nodes per week.

**4. Timezone.** Decision: one display zone per render, the OS zone from `Intl.DateTimeFormat().resolvedOptions().timeZone`; decision 2's day boundaries stay date-string arithmetic, `date-fns`
(`desktop/package.json:66`) does instant arithmetic only, no new dependency is added, and a differing source zone shows in the popover. Reason: `date-fns` v4 core computes in the system zone, which
cannot express a calendar-zone boundary.

**5. Recurrence expansion window.** Decision: expansion happens at Google — `events.list` with `singleEvents=true`, `orderBy=startTime`, `timeMin`/`timeMax` exactly T11 decision 6's window (30 days
back, 90 ahead) under its 10-page, 8 MiB and 30-second caps (T11:51); a walk ending against a cap is truncated, and decision 13 governs it. No RRULE parser enters the fork; `recurringEventId` is
display metadata. Reason: an in-app expander over a yearly rule is unbounded, and the server's window is what T11 sized the cache against.

**6. Month rendering.** Decision: keep `react-day-picker@^10.0.1` (`desktop/package.json:77`); a new `CalendarMonth` wrapper overrides one slot, `MonthGrid`, spreading every prop it receives —
`role`, `aria-multiselectable`, `aria-label`, `className`, `style` — onto an inner `<table>`, keeping only `children` and the overlay outside it and leaving `Week` and `Day` to render the rows the
library measures; decision 3's segments and "+N more" paint in one absolutely positioned overlay beside that table. Today's `DayButton` override (`desktop/src/shared/ui/calendar.tsx:154`, component
`:175`) stays untouched. Reason: a non-cell child of a `<table>` or `<tr>` breaks `role="grid"` row ownership, a day `<td>`'s `<button>` (`desktop/src/shared/ui/button.tsx:46`) nests no control, and a
named subset drops the other props.

**7. Agenda rendering.** Decision: a flat list of day-header and event rows, date-ascending, through `VirtualizedList` (`desktop/src/shared/ui/VirtualizedList.tsx:72`, `@tanstack/react-virtual` at
`desktop/package.json:52`); default under 640 px, a toggle above. A multi-day event takes one row, on the first day it covers inside the window, labelled with its span; within a day, all-day and
continuing rows sort first, then timed by start instant, ties by summary then event id. The row maximum derives from the bound the cache is held to, T11 decision 6's 5,000 rows per (identity,
calendar) partition (T11:49-50): 5,000 event rows, 120 day headers and decision 8's boundary row, 5,121, the count T12 verifies traversal and `estimateSize` against. `VirtualizedList` renders only the
virtual window (`:133`) and emits no roles, so T12 adds `role="list"` on the inner spacer (`:149`) and `role="listitem"` with `aria-posinset`/`aria-setsize` on each row div (`:155`); day headers are
rows and count in the set. Reason: one row per cached event ties the maximum to T11's bound, and `aria-posinset` needs a set role a list owns.

**8. Paging.** Decision: month and agenda stay inside T11 decision 6's fixed 120-day horizon; the month pages one month at a time and at the edge renders an end-of-window state — a named boundary
row, not an empty month — with no further fetch. Widening the horizon needs T11 amended. Reason: paging past the window renders an empty month indistinguishable from an empty calendar, a swallowed
boundary.

**9. Keyboard operation.** Decision: the month grid is one composite widget, one tab stop, roving focus — arrows by day, PageUp/PageDown by month, Home/End in the week, Enter opening the day's
agenda (also the path to "+N more"), `n` create, Escape out. The agenda is a second composite widget, one tab stop; its arrows set the active row index, `scrollToIndex` through `onVirtualizer`
(`VirtualizedList.tsx:69`), wait for the row to mount, then focus that row's named focusable, which every navigable row has: an event row its control, a day header a `tabindex="-1"` heading named by
the date, decision 8's boundary row one named "end of the fetched window". Enter opens the row. Reason: Tab cannot reach an unmounted row, and a row with nothing focusable strands focus where the list
scrolled from.

**10. Screen-reader semantics.** Decision: keep the grid semantics `DayPicker` emits and name each day through `labels.labelDayButton`, composed rather than replaced — call the library default for
the date and its `today` and `selected` state, then append ", N events" — since an interactive picker labels the button, not the `<td>`. Decision 6's overlay is `aria-hidden` decoration, decision
7's agenda is the authoritative reading order, and month and boundary changes announce through `DayPicker`'s own `role="status"` region, fed by the wrapper, not a second announcer. Reason: an
exact-string override deletes the picker's only "today" and "selected" announcement, and two live regions announce every change twice.

**11. Create and edit conflict handling.** Decision: `events.patch` sends only changed fields, and a field whose decision 1 `truncated` flag is set is read-only, so a prefix never overwrites Google's
copy. `events.insert` carries a client-generated event id — a UUID's 32 hex digits, lowercase and unhyphenated, inside Google's base32hex 5-1024 alphabet — so a lost response is retryable with the
same id; nothing retries a create automatically. `events.patch` and `events.delete` carry the `etag` as `If-Match`, and a 412 refetches the event and shows both values. A 404/410 closes the form to
"event no longer exists" only if the mapped calendar probes clean: a probe in T11 decision 8's terminal class purges, a transient or ambiguous one (T11:66-69) rejects that command alone, leaving a
read-only view on the stale snapshot until `stale_after`. Reason: last-write-wins destroys a co-organizer's edit, Google reuses 404 for both cases, and a transient probe must not destroy the cache.

**12. Component and library chosen.** Decision: no calendar-widget dependency — decisions 4, 6 and 7's pieces plus the existing dialog primitive. Reason: they are already in the tree and gated, and
a widget adds its own DOM, styling and date stack for a view with no time-grid.

**13. Truncated coverage and the interval a batch proves.** Decision: a batch carries the interval it reached plus `complete` or `truncated(reason)`, and that interval is proven, not inferred. The
invariant: decision 5's walk sets `orderBy=startTime`, so pages arrive ordered by start under T11 decision 6's caps (T11:51), and a page cut mid-stream by a cap is discarded whole, so the proven
interval ends at the last complete page — [window start, min(window end, that page's maximum end)]. Beyond it the batch is never authoritative: those dates render "unknown, more…", never empty,
the month marks the range partial with one retry, the agenda ends in decision 8's boundary row, and none is editable. The marker sits outside the evictable rows, and T11's eviction (T11:50) downgrades
each interval it touches to `truncated(evicted)` in the dropping transaction, so `complete` holds only while every substantiating row is retained — a T11-side write T12a requires. Reason: undefined,
"the interval it reached" closes at the last returned event and paints a half-read day complete — the defect `AGENTS.md` Review-Proven Rule 1 names — and neither cap exhaustion (200s) nor eviction
has a branch in T11 decision 8's matrices.

## Open verifications

- The ARIA roles `react-day-picker@^10.0.1` emits for grid and day cells, and that a `MonthGrid` override spreading every prop keeps them, off T12's DOM.
- That composing `labels.labelDayButton` keeps `today` and `selected` in the name, and that feeding `DayPicker`'s status region announces each change once.
- Lane and "+N more" thresholds on T12's 250-event fixture, and agenda focus traversal at decision 7's derived 5,121-row maximum.
- That Google honours `If-Match` (412 on a stale etag) and a client-supplied `events.insert` id on replay — T12's mock server first, the live checklist once.
- That `events.list` keeps equal-start events on one side of a page boundary, so decision 13's discarded page hides none inside the proven interval.

## Risks accepted

- No time-grid in v1: timed events in a day render as an ordered list, and decision 7 lists a multi-day event once, on its first covered day.
- Decision 8 gives v1 no way to see past T11's 120-day horizon; a date outside it is reachable only in Google's own UI until T11 is amended.
- Decision 13's `truncated(evicted)` downgrade is a T11-side write; until it lands, an evicted interval can still paint empty inside `stale_after`.
- The 4096-character `description` cap means long bodies render elided and, per decision 11, are not editable in Buzz at all.
- `Intl` reads the OS tz database, so a machine with stale tzdata mis-renders events across a zone-rule change until the OS is updated.
- To a screen reader the month grid carries counts, not events, and "+N more" has no tab stop; decision 9's Enter opens the same day agenda for both.
