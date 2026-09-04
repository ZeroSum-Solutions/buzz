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

test.describe("channel Files tab", () => {
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
