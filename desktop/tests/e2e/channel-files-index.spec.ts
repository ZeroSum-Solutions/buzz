import { expect, test } from "@playwright/test";
import type { Page } from "@playwright/test";

import { installMockBridge } from "../helpers/bridge";

const CHANNEL_NAME = "general";
const SEEDED_FILES = 250;
/** The tab must be readable this fast once its index is asked for. */
const RENDER_BUDGET_MS = 500;

async function waitForMockLiveSubscription(page: Page, channelName: string) {
  await page.waitForFunction(
    (name) =>
      typeof window.__BUZZ_E2E_EMIT_MOCK_MESSAGE__ === "function" &&
      (window.__BUZZ_E2E_HAS_MOCK_LIVE_SUBSCRIPTION__?.({
        channelName: name,
      }) ??
        false),
    channelName,
  );
}

/** Wait until the "All" filter reports `count` attachments in the index. */
async function waitForIndexedCount(page: Page, count: number) {
  await page.waitForFunction(
    (expected) => {
      const button = [...document.querySelectorAll("button")].find((node) =>
        node.textContent?.trim().startsWith("All"),
      );
      return button?.textContent?.includes(String(expected)) ?? false;
    },
    count,
    { timeout: 15_000 },
  );
}

test.describe("channel attachment index", () => {
  test("lists every attachment in the channel, thread replies included, and renders fast", async ({
    page,
  }) => {
    await installMockBridge(page);
    await page.goto("/");
    await page.getByTestId(`channel-${CHANNEL_NAME}`).click();
    await expect(page.getByTestId("chat-title")).toHaveText(CHANNEL_NAME);
    await waitForMockLiveSubscription(page, CHANNEL_NAME);

    // Seed the channel's history well past one window page (200 events,
    // top-level only) so the tab can only pass by reading its own index.
    // The last attachment is posted as a thread reply: the message-window
    // projection this replaced never saw those at all (issue #4428).
    await page.evaluate(
      ({ channelName, total }) => {
        const emit = window.__BUZZ_E2E_EMIT_MOCK_MESSAGE__;
        if (!emit) throw new Error("Mock message emitter is unavailable.");
        const base = Math.floor(Date.now() / 1000) - total - 10;
        let rootId: string | null = null;
        for (let index = 0; index < total - 1; index += 1) {
          const event = emit({
            channelName,
            content: `attachment ${index}`,
            createdAt: base + index,
            extraTags: [
              [
                "imeta",
                `url http://localhost:3000/media/seeded-${index}.bin`,
                "m application/pdf",
                "size 1024",
                `filename seeded-${index}.pdf`,
              ],
            ],
          });
          if (index === 0) rootId = event.id;
        }
        emit({
          channelName,
          content: "attachment in a thread",
          createdAt: base + total,
          parentEventId: rootId,
          extraTags: [
            [
              "imeta",
              "url http://localhost:3000/media/threaded.bin",
              "m application/pdf",
              "size 2048",
              "filename threaded-reply.pdf",
            ],
          ],
        });
      },
      { channelName: CHANNEL_NAME, total: SEEDED_FILES },
    );

    const durations: number[] = [];
    for (let run = 0; run < 3; run += 1) {
      if (run > 0) {
        await page.getByRole("tab", { name: "Chat" }).click();
        await expect(page.getByRole("tab", { name: "Chat" })).toHaveAttribute(
          "aria-selected",
          "true",
        );
      }
      const started = Date.now();
      await page.getByRole("tab", { name: "Files" }).click();
      await waitForIndexedCount(page, SEEDED_FILES);
      durations.push(Date.now() - started);
    }

    // Every seeded attachment is in the index, the threaded one included.
    await expect(
      page.getByRole("link", { name: "threaded-reply.pdf", exact: true }),
    ).toBeVisible();

    for (const [run, elapsed] of durations.entries()) {
      expect(
        elapsed,
        `run ${run + 1} rendered ${SEEDED_FILES} files in ${elapsed}ms`,
      ).toBeLessThan(RENDER_BUDGET_MS);
    }
  });
});
