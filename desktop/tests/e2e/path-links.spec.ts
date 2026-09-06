import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { expect, test } from "@playwright/test";
import type { Page } from "@playwright/test";

import { installMockBridge } from "../helpers/bridge";

// A backticked local path in a message becomes a link only after a hover or a
// click asks the native resolver; a `.md` inside an allowed root opens in the
// same viewer panel a relay markdown attachment uses, and a path that resolves
// to nothing stays plain text.
//
// The document body is the real `tests/fixtures/path-link-note.md` on disk, so
// the panel renders the same markdown the resolver would have read.

const FIXTURE_PATH = path.join(
  path.dirname(fileURLToPath(import.meta.url)),
  "../fixtures/path-link-note.md",
);
const FIXTURE_TEXT = readFileSync(FIXTURE_PATH, "utf8");
const CANDIDATE = "docs/path-link-note.md";
const RESOLVED_PATH = "/Users/example/projects/buzz/docs/path-link-note.md";
const MISSING_CANDIDATE = "docs/not-here.md";

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

async function commands(page: Page) {
  return page.evaluate(
    () =>
      (window as Window & { __BUZZ_E2E_COMMANDS__?: string[] })
        .__BUZZ_E2E_COMMANDS__ ?? [],
  );
}

async function seedMessage(page: Page, content: string) {
  await installMockBridge(page, {
    pathLinkFiles: [
      {
        candidate: CANDIDATE,
        path: RESOLVED_PATH,
        filename: "path-link-note.md",
        kind: "markdown",
        text: FIXTURE_TEXT,
      },
    ],
  });
  await page.goto("/");
  await page.getByTestId("channel-general").click();
  await expect(page.getByTestId("chat-title")).toHaveText("general");
  await waitForMockLiveSubscription(page, "general");
  await page.evaluate((body) => {
    window.__BUZZ_E2E_EMIT_MOCK_MESSAGE__?.({
      channelName: "general",
      content: body,
    });
  }, content);
}

test("a backticked markdown path opens the viewer panel", async ({ page }) => {
  await seedMessage(page, `Report is ready: \`${CANDIDATE}\``);

  const token = page.locator(`[data-path-link]`, { hasText: CANDIDATE }).last();
  await expect(token).toBeVisible();
  // Nothing is resolved while the channel renders.
  expect(await commands(page)).not.toContain("resolve_path_link");
  await expect(token).toHaveAttribute("data-path-link", "idle");

  await token.hover();
  await expect(token).toHaveAttribute("data-path-link", "link");
  await expect.poll(() => commands(page)).toContain("resolve_path_link");

  await token.getByRole("button", { name: "Open path-link-note.md" }).click();

  const panel = page.getByTestId("markdown-doc-panel");
  await expect(panel).toBeVisible();
  await expect(panel).toContainText("path-link-note.md");
  await expect(
    panel.getByRole("heading", { name: "Approval Note" }),
  ).toBeVisible();
  await expect(panel.locator("table")).toContainText("Viewer opens");
  // A local document is read natively, never fetched from the relay, and the
  // click never runs the file.
  const seen = await commands(page);
  expect(seen).toContain("read_path_link_markdown");
  expect(seen).not.toContain("fetch_markdown_doc_bytes");
  expect(seen).not.toContain("open_path_link");
});

test("a path that resolves to nothing stays plain text", async ({ page }) => {
  await seedMessage(page, `Missing: \`${MISSING_CANDIDATE}\``);

  const token = page
    .locator(`[data-path-link]`, { hasText: MISSING_CANDIDATE })
    .last();
  await expect(token).toBeVisible();

  await token.hover();
  await expect(token).toHaveAttribute("data-path-link", "text");
  // No control is left behind: the token is ordinary inline code again.
  await expect(token.getByRole("button")).toHaveCount(0);
  await expect(page.getByTestId("markdown-doc-panel")).toHaveCount(0);
});

test("a bare word is never treated as a path", async ({ page }) => {
  await seedMessage(page, "Run `cargo` and then `SIGKILL`");

  await expect(page.locator("[data-path-link]")).toHaveCount(0);
  expect(await commands(page)).not.toContain("resolve_path_link");
});
