import { expect, test, type Page } from "@playwright/test";

import { installMockBridge, TEST_IDENTITIES } from "../helpers/bridge";

/**
 * Copying a mention and pasting it back must preserve the identity.
 *
 * The reported failure is specific to a **multi-word, non-member** display
 * name: the rendered chip drops the `@`, so a plain copy yields "John Smith",
 * and nothing downstream can tell that from two ordinary words. These tests
 * bind the production copy/paste seams — real `copy`/`cut`/`paste` DOM events
 * against the timeline and the composer — and assert both clipboard flavors:
 * a readable plain flavor with no pubkey in it, and an HTML sidecar that
 * carries one.
 */

/** `mockDisplayNames` maps this to "John Smith"; it joins no mock channel. */
const JOHN_SMITH_PUBKEY =
  "7c1f2ad0b4e93856a1d0c2f4e6b8093a5d7f1c3e5a79b1d3f5072a4c6e80931b";
const MESSAGE_BODY = "@John Smith fixed the bug";
/** A pubkey must never reach the flavor an external app pastes. */
const ANY_64_HEX = /[0-9a-f]{64}/i;
/** Nobody's key — what a crafted clipboard sidecar would name instead. */
const IMPOSTOR_PUBKEY =
  "1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f";

type ClipboardFlavors = {
  defaultPrevented: boolean;
  html: string;
  text: string;
};

test.beforeEach(async ({ page }) => {
  await installMockBridge(page);
});

async function waitForMockLiveSubscription(page: Page, channelName: string) {
  await expect
    .poll(() =>
      page.evaluate(
        (currentChannelName) =>
          window.__BUZZ_E2E_HAS_MOCK_LIVE_SUBSCRIPTION__?.({
            channelName: currentChannelName,
          }) ?? false,
        channelName,
      ),
    )
    .toBe(true);
}

// The timeline renders off a `useDeferredValue` snapshot; the list wrapper
// carries `data-render-pending` until that commit lands.
async function waitForTimelineSettled(page: Page) {
  await expect(page.locator("[data-render-pending]")).toHaveCount(0);
}

async function emitMentionMessage(page: Page, channelName: string) {
  const event = await page.evaluate(
    ({ channel, content, mentionPubkey, pubkey }) =>
      window.__BUZZ_E2E_EMIT_MOCK_MESSAGE__?.({
        channelName: channel,
        content,
        mentionPubkeys: [mentionPubkey],
        pubkey,
      }),
    {
      channel: channelName,
      content: MESSAGE_BODY,
      mentionPubkey: JOHN_SMITH_PUBKEY,
      pubkey: TEST_IDENTITIES.alice.pubkey,
    },
  );
  if (!event) throw new Error("Mock message emitter is not installed");
  // The chip is what every copy in this file selects against, so wait for the
  // resolved identity rather than for the row. A non-member's profile is not
  // in the channel roster, so this waits on a profile round trip — give it
  // room beyond the default, since this is setup and not the assertion.
  const chip = page
    .getByTestId("message-body")
    .locator(`[data-mention-pubkey="${JOHN_SMITH_PUBKEY}"]`);
  await expect(chip).toHaveText("John Smith", { timeout: 15_000 });
  await waitForTimelineSettled(page);
  return event;
}

/**
 * Copy a range of the rendered timeline through the real `copy` event.
 *
 * `selectChip` narrows the range to the first four characters *inside* the
 * mention chip, which is how a user drags across half a name.
 */
async function copyFromTimeline(
  page: Page,
  { partialChip = false }: { partialChip?: boolean } = {},
): Promise<ClipboardFlavors> {
  return page.evaluate(
    ({ pubkey, selectPartialChip }) => {
      // Anchor on the chip, not on the first message body: `general` is seeded
      // with unrelated messages that would otherwise win the query.
      const chip = document.querySelector<HTMLElement>(
        `[data-testid="message-body"] [data-mention-pubkey="${pubkey}"]`,
      );
      if (!chip) throw new Error("Message body rendered no mention chip.");
      const body = chip.closest<HTMLElement>(".message-markdown");
      if (!body) throw new Error("Mention chip is outside a rendered body.");

      const selection = window.getSelection();
      if (!selection) throw new Error("Selection API unavailable.");
      selection.removeAllRanges();
      const range = document.createRange();
      if (selectPartialChip) {
        const label = chip.firstChild;
        if (!label) throw new Error("Mention chip has no text node.");
        range.setStart(label, 0);
        range.setEnd(label, 4);
      } else {
        range.selectNodeContents(body);
      }
      selection.addRange(range);

      const clipboardData = new DataTransfer();
      const event = new ClipboardEvent("copy", {
        bubbles: true,
        cancelable: true,
        clipboardData,
      });
      (selectPartialChip ? chip : body).dispatchEvent(event);
      return {
        defaultPrevented: event.defaultPrevented,
        html: clipboardData.getData("text/html"),
        text: clipboardData.getData("text/plain"),
      };
    },
    { pubkey: JOHN_SMITH_PUBKEY, selectPartialChip: partialChip },
  );
}

/** Copy or cut the composer's current selection through the real DOM event. */
async function copyFromComposer(
  page: Page,
  type: "copy" | "cut",
): Promise<ClipboardFlavors> {
  return page.getByTestId("message-input").evaluate((element, eventType) => {
    const clipboardData = new DataTransfer();
    const event = new ClipboardEvent(eventType, {
      bubbles: true,
      cancelable: true,
      clipboardData,
    });
    element.dispatchEvent(event);
    return {
      defaultPrevented: event.defaultPrevented,
      html: clipboardData.getData("text/html"),
      text: clipboardData.getData("text/plain"),
    };
  }, type);
}

async function pasteIntoComposer(
  page: Page,
  flavors: { html: string; text: string },
) {
  const input = page.getByTestId("message-input");
  await input.click();
  await input.evaluate((element, { html, text }) => {
    const clipboardData = new DataTransfer();
    clipboardData.setData("text/plain", text);
    clipboardData.setData("text/html", html);
    element.dispatchEvent(
      new ClipboardEvent("paste", {
        bubbles: true,
        cancelable: true,
        clipboardData,
      }),
    );
  }, flavors);
}

/**
 * The `p` tags of the outgoing message whose body is `content`.
 *
 * A DM is signed client-side and published over the socket rather than through
 * `send_channel_message`, so read the event handed to the signer.
 */
async function readSentMentionPubkeys(page: Page, content: string) {
  return page.evaluate((expectedContent) => {
    for (const entry of window.__BUZZ_E2E_COMMAND_LOG__ ?? []) {
      if (entry.command === "send_channel_message") {
        const payload = entry.payload as
          | { content?: string; mentionPubkeys?: string[] | null }
          | undefined;
        if (payload?.content !== expectedContent) continue;
        return payload.mentionPubkeys ?? [];
      }
      if (entry.command !== "sign_event") continue;
      const unsigned = entry.payload as
        | { content?: string; tags?: string[][] }
        | undefined;
      if (unsigned?.content !== expectedContent) continue;
      return (unsigned.tags ?? [])
        .filter((tag) => tag[0] === "p" && tag[1])
        .map((tag) => tag[1]);
    }
    return null;
  }, content);
}

function expectCarriesJohnSmith(flavors: ClipboardFlavors) {
  expect(flavors.defaultPrevented).toBe(true);
  // Readable anywhere, and safe to hand an external app: the sigil is back and
  // no identifier rode along.
  expect(flavors.text).toContain("@John Smith");
  expect(flavors.text).not.toMatch(ANY_64_HEX);
  // The identity travels in the sidecar flavor instead.
  expect(flavors.html).toContain(`data-mention-pubkey="${JOHN_SMITH_PUBKEY}"`);
  expect(flavors.html).toContain('data-mention-label="John Smith"');
}

async function expectComposerChip(page: Page) {
  const input = page.getByTestId("message-input");
  await expect(input).toHaveText(MESSAGE_BODY);
  await expect(input.locator(".mention-chip")).toHaveText("John Smith");
}

test("timeline selection copy carries a multi-word mention into another channel", async ({
  page,
}) => {
  await page.goto("/");
  await page.getByTestId("channel-general").click();
  await expect(page.getByTestId("chat-title")).toHaveText("general");
  await waitForMockLiveSubscription(page, "general");
  await emitMentionMessage(page, "general");

  const chip = page
    .getByTestId("message-row")
    .filter({ hasText: "John Smith fixed the bug" })
    .locator("[data-mention]");
  await expect(chip).toHaveAttribute("data-mention-pubkey", JOHN_SMITH_PUBKEY);
  await expect(chip).toHaveText("John Smith");

  const flavors = await copyFromTimeline(page);
  expectCarriesJohnSmith(flavors);
  expect(flavors.text.trim()).toBe(MESSAGE_BODY);

  // A DM is the destination so the send is not intercepted by the non-member
  // invite prompt — the assertion under test is the recovered `p` tag.
  await page.getByTestId("channel-bob-tyler").click();
  await expect(page.getByTestId("chat-title")).toHaveText("bob-tyler");
  await pasteIntoComposer(page, flavors);
  await expectComposerChip(page);

  await page.getByTestId("send-message").click();
  await expect(page.getByTestId("message-input")).toHaveText("");
  await expect
    .poll(() => readSentMentionPubkeys(page, MESSAGE_BODY))
    .toContain(JOHN_SMITH_PUBKEY);
});

test("copy message writes the identity sidecar beside readable plain text", async ({
  page,
}) => {
  await page.goto("/");
  await page.getByTestId("channel-general").click();
  await expect(page.getByTestId("chat-title")).toHaveText("general");
  await waitForMockLiveSubscription(page, "general");
  const message = await emitMentionMessage(page, "general");

  const row = page
    .getByTestId("message-row")
    .filter({ hasText: "John Smith fixed the bug" });
  await row.hover();
  await row.getByTestId(`more-actions-${message.id}`).click({ force: true });
  await page.getByRole("menuitem", { name: "Copy message" }).click();

  const written = await page.evaluate(
    () => window.__BUZZ_E2E_LAST_CLIPBOARD__ ?? null,
  );
  expect(written?.text).toBe(MESSAGE_BODY);
  expect(written?.text).not.toMatch(ANY_64_HEX);
  expect(written?.html).toContain(`data-mention-pubkey="${JOHN_SMITH_PUBKEY}"`);
  // "Copy message" copies Markdown source, so paste must take the text path.
  expect(written?.html).toContain('data-buzz-copy="markdown"');

  await page.getByTestId("channel-bob-tyler").click();
  await expect(page.getByTestId("chat-title")).toHaveText("bob-tyler");
  await pasteIntoComposer(page, {
    html: written?.html ?? "",
    text: written?.text ?? "",
  });
  await expectComposerChip(page);
});

test("composer copy and cut round-trip the mention they were pasted with", async ({
  page,
}) => {
  await page.goto("/");
  await page.getByTestId("channel-general").click();
  await expect(page.getByTestId("chat-title")).toHaveText("general");
  await waitForMockLiveSubscription(page, "general");
  await emitMentionMessage(page, "general");

  const source = await copyFromTimeline(page);
  await pasteIntoComposer(page, source);
  await expectComposerChip(page);

  const input = page.getByTestId("message-input");
  await input.press("ControlOrMeta+a");
  expectCarriesJohnSmith(await copyFromComposer(page, "copy"));
  await expect(input).toHaveText(MESSAGE_BODY);

  const cut = await copyFromComposer(page, "cut");
  expectCarriesJohnSmith(cut);
  await expect(input).toHaveText("");

  // The cut flavors are a complete round trip on their own.
  await pasteIntoComposer(page, cut);
  await expectComposerChip(page);
});

test("an identity the pasted content never shows binds no name", async ({
  page,
}) => {
  await page.goto("/");
  await page.getByTestId("channel-bob-tyler").click();
  await expect(page.getByTestId("chat-title")).toHaveText("bob-tyler");

  // Any copied page can carry this: an empty span claiming a real display
  // name against a key of its choosing. A registration outlives its paste, so
  // accepting one the user cannot see would rebind the name for the session.
  await pasteIntoComposer(page, {
    html:
      '<span data-buzz-copy="markdown">' +
      `<span data-mention="" data-mention-pubkey="${IMPOSTOR_PUBKEY}" ` +
      'data-mention-label="John Smith"></span>look at this</span>',
    text: "look at this",
  });
  const input = page.getByTestId("message-input");
  await expect(input).toHaveText("look at this");

  // The name that sidecar tried to claim, written afterwards by hand.
  await input.press("ControlOrMeta+a");
  await input.press("Backspace");
  await expect(input).toHaveText("");
  await pasteIntoComposer(page, { html: "", text: MESSAGE_BODY });
  await expect(input).toHaveText(MESSAGE_BODY);
  await expect(input.locator(".mention-chip")).toHaveCount(0);

  await page.getByTestId("send-message").click();
  await expect(input).toHaveText("");
  await expect
    .poll(() => readSentMentionPubkeys(page, MESSAGE_BODY))
    .not.toBeNull();
  expect(await readSentMentionPubkeys(page, MESSAGE_BODY)).not.toContain(
    IMPOSTOR_PUBKEY,
  );
});

test("a half-selected chip copies as plain text with no identity attached", async ({
  page,
}) => {
  await page.goto("/");
  await page.getByTestId("channel-general").click();
  await expect(page.getByTestId("chat-title")).toHaveText("general");
  await waitForMockLiveSubscription(page, "general");
  await emitMentionMessage(page, "general");

  // Selecting "John" out of "John Smith" must not invent "@John": registering
  // a truncated label would bind the wrong name to a real pubkey.
  const flavors = await copyFromTimeline(page, { partialChip: true });
  expect(flavors.defaultPrevented).toBe(false);
  expect(flavors.html).toBe("");
  expect(flavors.text).toBe("");
});
