import { expect, test, type Page } from "@playwright/test";
import { installMockBridge } from "../helpers/bridge";
import type {
  CalendarEvent,
  CalendarEventFields,
} from "../../src/shared/api/tauriCalendar";

async function installCalendar(
  page: Page,
  options: {
    stale?: boolean;
    partial?: boolean;
    connected?: boolean;
    count?: number;
    pendingCleanup?: boolean;
  } = {},
) {
  await installMockBridge(page);
  await page.goto("/");
  await expect(page.getByTestId("open-calendar-view")).toBeVisible();
  await page.evaluate((options) => {
    const internals = (
      window as unknown as {
        __TAURI_INTERNALS__: {
          invoke: (command: string, args?: unknown) => Promise<unknown>;
        };
      }
    ).__TAURI_INTERNALS__;
    const original = internals.invoke;
    const now = new Date();
    const date = `${now.getFullYear()}-${String(now.getMonth() + 1).padStart(2, "0")}-${String(now.getDate()).padStart(2, "0")}`;
    const tomorrow = new Date(
      now.getFullYear(),
      now.getMonth(),
      now.getDate() + 1,
    );
    const endDate = `${tomorrow.getFullYear()}-${String(tomorrow.getMonth() + 1).padStart(2, "0")}-${String(tomorrow.getDate()).padStart(2, "0")}`;
    const text = (value: string) => ({ value, truncated: false });
    let events: CalendarEvent[] = [
      {
        id: "test-event",
        etag: "v1",
        recurring_event_id: null,
        status: "confirmed",
        start: { kind: "all_day", date },
        end: { kind: "all_day", date: endDate },
        summary: text("Project planning"),
        location: text("Studio"),
        description: text("Review milestones"),
        can_edit: true,
        can_delete: true,
      },
    ];
    const template = events[0];
    if (options.count !== undefined)
      events = Array.from({ length: options.count }, (_, index) => ({
        ...template,
        id: `event-${index}`,
        summary: text(`Event ${index}`),
      }));
    let revocations = options.pendingCleanup
      ? [
          {
            generation: 7,
            state: "revocation_unconfirmed",
            purge_confirmed: true,
          },
        ]
      : [];
    internals.invoke = async (command, args) => {
      if (command === "calendar_abandon_revocation") {
        revocations = revocations.map((entry) => ({
          ...entry,
          state: "abandoned",
        }));
        return;
      }
      if (command === "calendar_clear_revocation") {
        revocations = [];
        return;
      }
      if (command === "calendar_status")
        return {
          configured: true,
          connected: options.connected !== false,
          email: "calendar@example.test",
          generation: 1,
          pending_revocations: revocations.length,
          revocations,
          error: null,
        };
      if (command === "calendar_events")
        return {
          events,
          generation: 1,
          stale: options.stale === true,
          refreshed_at_ms: Date.now(),
          interval: {
            start_ms: Date.now() - 30 * 86400000,
            end_ms: options.partial ? Date.now() : Date.now() + 90 * 86400000,
            coverage: options.partial
              ? { coverage: "truncated", reason: "page_cap" }
              : { coverage: "complete" },
          },
        };
      if (command === "calendar_create") {
        const { input } = args as {
          input: { eventId: string; fields: CalendarEventFields };
        };
        const created = {
          ...template,
          id: input.eventId,
          start: input.fields.start,
          end: input.fields.end,
          summary: text(input.fields.summary),
          location: text(input.fields.location),
          description: text(input.fields.description),
        };
        events.push(created);
        return created;
      }
      if (command === "calendar_update") {
        const input = (
          args as { input: { eventId: string; fields: { summary?: string } } }
        ).input;
        events = events.map((event) =>
          event.id === input.eventId
            ? {
                ...event,
                summary: text(input.fields.summary ?? event.summary.value),
                etag: "v2",
              }
            : event,
        );
        return events[0];
      }
      if (command === "calendar_delete") {
        events = [];
        return;
      }
      return original(command, args);
    };
  }, options);
  await page.getByTestId("open-calendar-view").click();
}

test("calendar sidebar opens month and agenda and edits and deletes an event", async ({
  page,
}) => {
  await installCalendar(page);
  await expect(
    page.getByRole("heading", { name: "Calendar", exact: true }),
  ).toBeVisible();
  await page.screenshot({ path: test.info().outputPath("calendar-month.png") });
  await page.getByRole("button", { name: "Agenda", exact: true }).click();
  await page.getByRole("button", { name: /Project planning/ }).click();
  await page.getByRole("button", { name: "Edit event", exact: true }).click();
  await page.getByLabel("Title", { exact: true }).fill("Updated planning");
  await page.getByRole("button", { name: "Save event", exact: true }).click();
  await expect(
    page.getByRole("button", { name: /Updated planning/ }),
  ).toBeVisible();
  await page.getByRole("button", { name: /Updated planning/ }).click();
  await page.getByRole("button", { name: "Delete event", exact: true }).click();
  await expect(page.getByText("No events in this period.")).toBeVisible();
  await page.getByRole("button", { name: "New event", exact: true }).click();
  await page.getByLabel("Title", { exact: true }).fill("Created in Buzz");
  await page.getByRole("button", { name: "Save event", exact: true }).click();
  await expect(
    page.getByRole("button", { name: /Created in Buzz/ }),
  ).toBeVisible();
});

test("keyboard reaches the end of a virtualized agenda", async ({ page }) => {
  await installCalendar(page, { count: 300 });
  await page.getByRole("button", { name: "Agenda", exact: true }).click();
  const first = page.locator('[data-calendar-index="0"]');
  await first.focus();
  await first.press("End");
  await expect(page.locator('[data-calendar-index="299"]')).toBeFocused();
  expect(await page.getByRole("listitem").count()).toBeLessThan(40);
  await page.locator('[data-calendar-index="299"]').press("Home");
  await expect(first).toBeFocused();
});

test("partial empty day is unknown rather than falsely empty", async ({
  page,
}) => {
  await installCalendar(page, { partial: true, count: 0 });
  await expect(
    page.getByText("No events loaded for this view yet."),
  ).toBeVisible();
  await expect(page.getByText("No events on this day.")).not.toBeVisible();
});

test("stale calendar preserves visible events and disables mutations", async ({
  page,
}) => {
  await installCalendar(page, { stale: true });
  await expect(
    page.getByText("These events may be out of date.", { exact: false }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "New event", exact: true }),
  ).toBeDisabled();
  await expect(
    page.getByRole("button", { name: /Project planning/ }),
  ).toBeVisible();
});

test("disconnected calendar explains personal connection", async ({ page }) => {
  await installCalendar(page, { connected: false });
  await expect(
    page.getByRole("button", { name: "Connect Google Calendar", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByText("Your schedule, private to your Buzz account."),
  ).toBeVisible();
});

test("unconfirmed cleanup needs explicit action before reconnect", async ({
  page,
}) => {
  await installCalendar(page, { connected: false, pendingCleanup: true });
  const connect = page.getByRole("button", {
    name: "Connect Google Calendar",
    exact: true,
  });
  await expect(connect).toBeDisabled();
  await page
    .getByRole("button", { name: "Stop retrying cleanup…", exact: true })
    .click();
  await expect(
    page.getByText("Google may still allow access.", { exact: false }),
  ).toBeVisible();
  await page.getByRole("button", { name: "Keep cleanup", exact: true }).click();
  await expect(connect).toBeDisabled();
  await page
    .getByRole("button", { name: "Stop retrying cleanup…", exact: true })
    .click();
  await page
    .getByRole("button", {
      name: "Stop retries and allow reconnect",
      exact: true,
    })
    .click();
  await expect(connect).toBeEnabled();
  await page
    .getByRole("button", { name: "Clear this cleanup record", exact: true })
    .click();
  await expect(
    page.getByText("Previous Google access removal needs attention.", {
      exact: false,
    }),
  ).not.toBeVisible();
});

test("calendar connection is reachable through personal settings navigation", async ({
  page,
}) => {
  await installCalendar(page, { connected: false });
  await page.getByRole("link", { name: "Calendar settings" }).click();
  const personal = page.getByLabel("Personal settings sections", {
    exact: true,
  });
  await personal.getByRole("button", { name: "Profile", exact: true }).click();
  await personal.getByRole("button", { name: "Calendar", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "Connect Google Calendar", exact: true }),
  ).toBeVisible();
});
