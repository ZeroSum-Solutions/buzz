import assert from "node:assert/strict";
import { after, afterEach, before, test } from "node:test";
import { JSDOM } from "jsdom";
const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});
before(() => {
  Object.assign(globalThis, {
    document: dom.window.document,
    HTMLElement: dom.window.HTMLElement,
    IS_REACT_ACT_ENVIRONMENT: true,
    window: dom.window,
  });
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    value: dom.window.navigator,
  });
});
after(() => dom.window.close());
afterEach(async () => (await import("@testing-library/react")).cleanup());

test("editing an all-day title preserves provider-exclusive dates and truncated fields", async () => {
  const { render, screen, fireEvent } = await import("@testing-library/react");
  const React = await import("react");
  const { CalendarEventForm } = await import("./CalendarEventForm.tsx");
  let saved;
  const text = (value) => ({ value, truncated: false });
  render(
    React.createElement(CalendarEventForm, {
      day: new Date(2026, 8, 7),
      busy: false,
      onCancel() {},
      onSave(fields) {
        saved = fields;
      },
      event: {
        id: "one",
        etag: "v1",
        status: "confirmed",
        start: { kind: "all_day", date: "2026-09-07" },
        end: { kind: "all_day", date: "2026-09-08" },
        summary: text("Planning"),
        location: text(""),
        description: { value: "truncated notes", truncated: true },
        can_edit: true,
        can_delete: true,
      },
    }),
  );
  assert.equal(screen.getByLabelText("Last day").value, "2026-09-07");
  assert.equal(screen.getByLabelText("Notes").disabled, true);
  fireEvent.change(screen.getByLabelText("Title"), {
    target: { value: "Updated" },
  });
  fireEvent.click(screen.getByRole("button", { name: "Save event" }));
  assert.deepEqual(saved, { summary: "Updated" });
});

test("new one-day events serialize a next-day exclusive end", async () => {
  const { render, screen, fireEvent } = await import("@testing-library/react");
  const React = await import("react");
  const { CalendarEventForm } = await import("./CalendarEventForm.tsx");
  let saved;
  render(
    React.createElement(CalendarEventForm, {
      day: new Date(2026, 8, 7),
      busy: false,
      onCancel() {},
      onSave(fields) {
        saved = fields;
      },
    }),
  );
  fireEvent.change(screen.getByLabelText("Title"), {
    target: { value: "One day" },
  });
  fireEvent.click(screen.getByLabelText("All day"));
  fireEvent.click(screen.getByRole("button", { name: "Save event" }));
  assert.deepEqual(saved.start, { kind: "all_day", date: "2026-09-07" });
  assert.deepEqual(saved.end, { kind: "all_day", date: "2026-09-08" });
});

for (const [label, start, end] of [
  [
    "repeated DST hour",
    "2026-11-01T01:30:00-07:00",
    "2026-11-01T01:15:00-08:00",
  ],
  [
    "seconds within one minute",
    "2026-09-08T10:00:05-07:00",
    "2026-09-08T10:00:45-07:00",
  ],
]) {
  test(`title-only edits preserve original instants across ${label}`, async () => {
    const oldZone = process.env.TZ;
    process.env.TZ = "America/Los_Angeles";
    try {
      const { render, screen, fireEvent } = await import(
        "@testing-library/react"
      );
      const React = await import("react");
      const { CalendarEventForm } = await import("./CalendarEventForm.tsx");
      let saved;
      const text = (value) => ({ value, truncated: false });
      render(
        React.createElement(CalendarEventForm, {
          day: new Date(start),
          busy: false,
          onCancel() {},
          onSave(fields) {
            saved = fields;
          },
          event: {
            id: "fixture",
            etag: "v1",
            status: "confirmed",
            start: { kind: "timed", instant_ms: Date.parse(start) },
            end: { kind: "timed", instant_ms: Date.parse(end) },
            summary: text("Before"),
            location: text(""),
            description: text(""),
            can_edit: true,
            can_delete: true,
          },
        }),
      );
      fireEvent.change(screen.getByLabelText("Title"), {
        target: { value: "After" },
      });
      fireEvent.click(screen.getByRole("button", { name: "Save event" }));
      assert.deepEqual(saved, { summary: "After" });
    } finally {
      if (oldZone === undefined) delete process.env.TZ;
      else process.env.TZ = oldZone;
    }
  });
}

test("edited nonexistent local times are rejected instead of shifted across the DST gap", async () => {
  const oldZone = process.env.TZ;
  process.env.TZ = "America/Los_Angeles";
  try {
    const { render, screen, fireEvent } = await import(
      "@testing-library/react"
    );
    const React = await import("react");
    const { CalendarEventForm } = await import("./CalendarEventForm.tsx");
    let saved;
    render(
      React.createElement(CalendarEventForm, {
        day: new Date(2026, 2, 8),
        busy: false,
        onCancel() {},
        onSave(fields) {
          saved = fields;
        },
      }),
    );
    fireEvent.change(screen.getByLabelText("Title"), {
      target: { value: "Gap" },
    });
    fireEvent.change(screen.getByLabelText("Start"), {
      target: { value: "2026-03-08T02:30" },
    });
    fireEvent.change(screen.getByLabelText("End"), {
      target: { value: "2026-03-08T04:00" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Save event" }));
    assert.equal(saved, undefined);
    assert.match(screen.getByRole("alert").textContent, /does not exist/i);
  } finally {
    if (oldZone === undefined) delete process.env.TZ;
    else process.env.TZ = oldZone;
  }
});

test("changing one endpoint carries its untouched partner with original seconds and zone", async () => {
  const { render, screen, fireEvent } = await import("@testing-library/react");
  const React = await import("react");
  const { CalendarEventForm } = await import("./CalendarEventForm.tsx");
  const start = {
    kind: "timed",
    instant_ms: new Date(2026, 8, 8, 10, 0, 5).getTime(),
    time_zone: "America/Los_Angeles",
  };
  const end = {
    kind: "timed",
    instant_ms: new Date(2026, 8, 8, 10, 1).getTime(),
    time_zone: "America/Los_Angeles",
  };
  const text = (value) => ({ value, truncated: false });
  let saved;
  render(
    React.createElement(CalendarEventForm, {
      day: new Date(start.instant_ms),
      busy: false,
      onCancel() {},
      onSave(fields) {
        saved = fields;
      },
      event: {
        id: "fixture",
        etag: "v1",
        status: "confirmed",
        start,
        end,
        summary: text("Planning"),
        location: text(""),
        description: text(""),
        can_edit: true,
        can_delete: true,
      },
    }),
  );
  fireEvent.change(screen.getByLabelText("End"), {
    target: { value: "2026-09-08T10:02" },
  });
  fireEvent.click(screen.getByRole("button", { name: "Save event" }));
  assert.deepEqual(saved.start, start);
  assert.equal(saved.end.instant_ms, new Date(2026, 8, 8, 10, 2).getTime());
  assert.equal(saved.summary, undefined);
});
