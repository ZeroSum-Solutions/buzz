import assert from "node:assert/strict";
import { test } from "node:test";
import { eventsForDay, isDayCovered } from "./calendarDates.ts";

const allDay = {
  id: "holiday",
  status: "confirmed",
  start: { kind: "all_day", date: "2026-09-07" },
  end: { kind: "all_day", date: "2026-09-09" },
};
test("all-day dates remain local dates and the ending date is exclusive", () => {
  assert.equal(eventsForDay([allDay], new Date(2026, 8, 7)).length, 1);
  assert.equal(eventsForDay([allDay], new Date(2026, 8, 8)).length, 1);
  assert.equal(eventsForDay([allDay], new Date(2026, 8, 9)).length, 0);
});
test("timed events overlap local days but midnight end does not spill", () => {
  const event = {
    id: "night",
    status: "confirmed",
    start: { kind: "timed", instant_ms: new Date(2026, 8, 7, 23).getTime() },
    end: { kind: "timed", instant_ms: new Date(2026, 8, 8).getTime() },
  };
  assert.equal(eventsForDay([event], new Date(2026, 8, 7)).length, 1);
  assert.equal(eventsForDay([event], new Date(2026, 8, 8)).length, 0);
  assert.equal(
    eventsForDay([{ ...event, status: "cancelled" }], new Date(2026, 8, 7))
      .length,
    0,
  );
});
test("a partially covered or stale day cannot be called empty", () => {
  const day = new Date(2026, 8, 7);
  const start_ms = day.getTime();
  const end_ms = new Date(2026, 8, 8).getTime();
  assert.equal(isDayCovered(day, { start_ms, end_ms }, false), true);
  assert.equal(
    isDayCovered(day, { start_ms, end_ms: end_ms - 1 }, false),
    false,
  );
  assert.equal(isDayCovered(day, { start_ms, end_ms }, true), false);
  assert.equal(isDayCovered(day, null, false), false);
});
