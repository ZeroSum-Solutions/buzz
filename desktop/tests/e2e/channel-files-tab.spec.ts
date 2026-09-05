import { expect, test } from "@playwright/test";
import type { Page } from "@playwright/test";

import { installMockBridge } from "../helpers/bridge";

const CHANNEL_NAME = "general";

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

/**
 * The channel title, the tab strip and the first file row must stack without
 * overlapping, and nothing may push the document into horizontal scroll.
 */
async function assertNoOverlap(page: Page) {
  const title = await page.getByTestId("chat-title").boundingBox();
  const tabs = await page.getByRole("tab", { name: "Files" }).boundingBox();
  const row = await page
    .getByRole("link", { name: "release-notes.md", exact: true })
    .boundingBox();

  expect(title, "chat title has a box").not.toBeNull();
  expect(tabs, "tab strip has a box").not.toBeNull();
  expect(row, "first file row has a box").not.toBeNull();
  if (!title || !tabs || !row) return;

  expect(
    title.y + title.height,
    "the channel title must sit above the tab strip",
  ).toBeLessThanOrEqual(tabs.y + 1);
  expect(
    tabs.y + tabs.height,
    "the tab strip must sit above the first file row",
  ).toBeLessThanOrEqual(row.y + 1);

  const overflow = await page.evaluate(() => ({
    scrollWidth: document.documentElement.scrollWidth,
    clientWidth: document.documentElement.clientWidth,
  }));
  expect(
    overflow.scrollWidth,
    "the Files tab must not scroll the document horizontally",
  ).toBeLessThanOrEqual(overflow.clientWidth + 1);
}

test.describe("channel Files tab", () => {
  test("the tab strip grows the header box without shrinking the Chat column", async ({
    page,
  }) => {
    // The composer chip is labelled from the upload descriptor the bridge
    // answers with, so name it after the file this test drops.
    await installMockBridge(page, {
      uploadDescriptors: [
        {
          url: `https://mock.relay/media/${"c".repeat(64)}.pdf`,
          sha256: "c".repeat(64),
          size: 5,
          type: "application/pdf",
          uploaded: Math.floor(Date.now() / 1000),
          filename: "handover.pdf",
        },
      ],
    });
    await page.goto("/");
    await page.getByTestId(`channel-${CHANNEL_NAME}`).click();
    await expect(page.getByTestId("chat-title")).toHaveText(CHANNEL_NAME);
    // The strip is present and Chat is the selected tab: this is the layout
    // every assertion below is about.
    await expect(page.getByRole("tab", { name: "Files" })).toBeVisible();
    await expect(page.getByRole("tab", { name: "Chat" })).toHaveAttribute(
      "aria-selected",
      "true",
    );

    // The channel header is a measured overlay: the chrome wrapper's height
    // becomes --buzz-channel-content-top-padding and that same variable is its
    // negative bottom margin, so the header contributes zero flow height. The
    // tab strip lives inside that measured box, so the variable must equal the
    // whole header — measuring the title row alone would leave the strip's
    // height uncancelled and push the entire Chat column down by it.
    const measured = await page.evaluate(() => {
      const header = document.querySelector<HTMLElement>(
        '[data-testid="chat-header"]',
      );
      if (!header) return null;
      const declared = getComputedStyle(header)
        .getPropertyValue("--buzz-channel-content-top-padding")
        .trim();
      return {
        declaredPx: Number.parseFloat(declared),
        headerHeight: header.getBoundingClientRect().height,
      };
    });
    expect(measured, "the channel header is on screen").not.toBeNull();
    expect(
      measured?.declaredPx,
      "the measured chrome variable must span the whole header, tab strip included",
    ).toBeCloseTo(measured?.headerHeight ?? 0, 0);

    // The consequence that the regression actually broke: the channel column's
    // drop overlay must still cover the column edge to edge, and a drop on it
    // must still attach to the composer while Chat is the active tab.
    const dataTransfer = await page.evaluateHandle(() => {
      const transfer = new DataTransfer();
      transfer.items.add(
        new File(["notes"], "handover.pdf", { type: "application/pdf" }),
      );
      return transfer;
    });
    const dropZone = page.getByTestId("channel-drop-zone");
    await dropZone.dispatchEvent("dragenter", { dataTransfer });
    const overlay = dropZone.getByTestId("drop-zone-overlay");
    await expect(overlay).toBeVisible();
    const [dropZoneBox, overlayBox] = await Promise.all([
      dropZone.boundingBox(),
      overlay.boundingBox(),
    ]);
    expect(
      overlayBox,
      "the drop overlay covers the whole channel column",
    ).toEqual(dropZoneBox);

    await dropZone.dispatchEvent("drop", { dataTransfer });
    await expect(page.getByTestId("message-composer")).toContainText(
      "handover.pdf",
    );
  });

  test("lists file attachments, labels a Markdown file by its imeta filename, and jumps back to the message", async ({
    page,
  }) => {
    await installMockBridge(page);
    await page.goto("/");
    await page.getByTestId(`channel-${CHANNEL_NAME}`).click();
    await expect(page.getByTestId("chat-title")).toHaveText(CHANNEL_NAME);
    await waitForMockLiveSubscription(page, CHANNEL_NAME);

    const attachmentSha = "b".repeat(64);
    // Raw URL intentionally ends in .bin (no useful extension) — the Files
    // tab must label the row from the imeta `filename` field, not this URL.
    const attachmentUrl = `http://localhost:3000/media/${attachmentSha}.bin`;
    const imetaTag = [
      "imeta",
      `url ${attachmentUrl}`,
      "m text/markdown",
      `x ${attachmentSha}`,
      "size 512",
      "filename release-notes.md",
    ];

    const { fileMessageId } = await page.evaluate(
      ({ channelName, tag }) => {
        const emit = window.__BUZZ_E2E_EMIT_MOCK_MESSAGE__;
        if (!emit) throw new Error("Mock message emitter is unavailable.");
        const plain = emit({
          channelName,
          content: "Just a plain message, no attachment.",
        });
        const withFile = emit({
          channelName,
          content: `[release-notes.md](${(tag[1] as string).slice(4)})`,
          extraTags: [tag],
        });
        return { fileMessageId: withFile.id, plainMessageId: plain.id };
      },
      { channelName: CHANNEL_NAME, tag: imetaTag },
    );

    await page.getByRole("tab", { name: "Files" }).click();

    const fileRow = page.getByRole("link", {
      name: "release-notes.md",
      exact: true,
    });
    await expect(fileRow).toBeVisible();

    // Visibility alone does not prove the layout: Playwright reports an
    // element visible while sticky header chrome sits on top of it, which is
    // exactly the overlap this header/tab-strip layout exists to prevent.
    // Assert the stacking order geometrically, at the default width and again
    // narrow, so removing the measured header ref or the Files padding fails
    // here rather than shipping.
    await assertNoOverlap(page);
    await page.setViewportSize({ width: 720, height: 720 });
    await expect(fileRow).toBeVisible();
    await assertNoOverlap(page);
    await page.setViewportSize({ width: 1280, height: 720 });
    await expect(fileRow).toBeVisible();
    // The plain, attachment-less message must not appear in the Files tab.
    await expect(
      page.getByText("Just a plain message, no attachment."),
    ).toHaveCount(0);

    await page.getByRole("button", { name: "Jump to message" }).click();

    // Jumping back switches to the Chat tab and the composer/timeline for
    // the message that carried the attachment becomes visible again.
    await expect(page.getByRole("tab", { name: "Chat" })).toHaveAttribute(
      "aria-selected",
      "true",
    );
    await expect(
      page
        .getByTestId("message-timeline")
        .locator(`[data-message-id="${fileMessageId}"]`),
    ).toBeVisible();
  });
});
