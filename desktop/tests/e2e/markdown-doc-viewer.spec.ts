import { readFileSync } from "node:fs";
import { expect, test } from "@playwright/test";
import type { Page } from "@playwright/test";

import { installMockBridge } from "../helpers/bridge";

// Exercises the markdown-attachment viewer end-to-end through the mock Tauri
// bridge: upload a `.md` file → send → FileCard opens the in-app markdown
// viewer panel (not the download dialog) → Preview renders, Code shows the
// source, Download still works from the panel header.
//
// The attachment URL deliberately mirrors production shape: the relay stores
// extension-less text as `{sha256}.bin` (markdown has no magic bytes), so the
// `.md` identity lives only in the imeta filename. The mock upload descriptor
// reproduces that.

const RELAY_HTTP_URL =
  process.env.BUZZ_E2E_RELAY_URL ?? "http://localhost:3000";
const DOC_SHA = "b".repeat(64);
const DOC_URL = `${RELAY_HTTP_URL}/media/${DOC_SHA}.bin`;
const DOC_MARKDOWN = [
  "# Release Notes",
  "",
  "Some **bold** text and a table:",
  "",
  "| Feature | Works |",
  "| --- | --- |",
  "| Headings | Yes |",
  "",
  "```js",
  'console.log("hi");',
  "```",
  "",
].join("\n");

test.beforeEach(async ({ page }) => {
  await installMockBridge(page, {
    // Route attach through the DOM file input (Playwright's filechooser)
    // instead of the native pick_and_upload_media dialog path.
    deferredComposerUploads: true,
    uploadDescriptors: [
      {
        url: DOC_URL,
        sha256: DOC_SHA,
        size: DOC_MARKDOWN.length,
        type: "application/octet-stream",
        uploaded: Math.floor(Date.now() / 1000),
        filename: "release-notes.md",
      },
    ],
  });
  // The bridge's `fetch_media_bytes` mock fetches the URL in-browser; serve
  // the document body from the spec instead of a real relay.
  await page.route(`**/media/${DOC_SHA}.bin`, (route) =>
    route.fulfill({
      body: DOC_MARKDOWN,
      contentType: "application/octet-stream",
    }),
  );
});

async function sendMarkdownAttachment(page: Page) {
  await page.goto("/");
  await page.getByTestId("channel-general").click();
  await expect(page.getByTestId("chat-title")).toHaveText("general");
  await attachAndSendMarkdown(page);
}

/** Attach + send in the already-open channel (mock re-serves one descriptor,
 * so every send yields a card with the same document URL). */
async function attachAndSendMarkdown(page: Page) {
  const [chooser] = await Promise.all([
    page.waitForEvent("filechooser"),
    page.getByRole("button", { name: "Attach file" }).click(),
  ]);
  await chooser.setFiles({
    buffer: Buffer.from(DOC_MARKDOWN),
    mimeType: "text/markdown",
    name: "release-notes.md",
  });
  await expect(page.getByTestId("message-composer")).toContainText(
    "release-notes.md",
  );
  await page.getByTestId("send-message").click();
  await expect(page.getByText("Sending")).toHaveCount(0);
}

test("markdown attachment opens the in-app viewer with Preview/Code toggle", async ({
  page,
}) => {
  await sendMarkdownAttachment(page);

  // The card advertises open-in-viewer, not download.
  const card = page.getByTestId("file-card").last();
  await expect(card).toContainText("release-notes.md");
  await expect(card).toHaveAttribute("aria-label", "Open release-notes.md");
  await card.click();

  // The viewer panel opens with the rendered document (no download dialog).
  const panel = page.getByTestId("markdown-doc-panel");
  await expect(panel).toBeVisible();
  await expect(panel).toContainText("release-notes.md");
  await expect(
    panel.getByRole("heading", { name: "Release Notes" }),
  ).toBeVisible();
  // GFM table rendered as a real table, not pipes.
  await expect(panel.locator("table")).toContainText("Headings");
  // GFM fenced code block rendered as a highlighted code block in the
  // Preview pane, not left as raw ``` fences or plain inline text.
  await expect(panel.locator("[data-code-block]")).toContainText(
    'console.log("hi");',
  );
  const commands = () =>
    page.evaluate(
      () =>
        (window as Window & { __BUZZ_E2E_COMMANDS__?: string[] })
          .__BUZZ_E2E_COMMANDS__ ?? [],
    );
  expect(await commands()).not.toContain("download_file");

  // Code view shows the raw source.
  await page.getByTestId("markdown-doc-view-code").click();
  const smallDocCodeView = page.getByTestId("markdown-doc-code");
  await expect(smallDocCodeView).toContainText("# Release Notes");
  // Positive control for the highlight-byte-bound test below (Sol audit
  // finding 2): a small document is well under CodeBlock's highlight caps,
  // so Shiki tokenization succeeds — each rendered line nests a `<span
  // style="color:...">` per token, distinct from the plain-text fallback's
  // bare `<span data-line="">` with no element children. Shiki's
  // language/theme assets load asynchronously after first mount, so poll
  // rather than taking one immediate count.
  await expect(
    smallDocCodeView.locator("[data-line] > span").first(),
  ).toBeAttached({ timeout: 5000 });
  await page.getByTestId("markdown-doc-view-preview").click();
  await expect(
    panel.getByRole("heading", { name: "Release Notes" }),
  ).toBeVisible();

  // Download stays available from the panel header.
  await page.getByTestId("markdown-doc-download").click();
  await expect.poll(commands).toContain("download_file");

  // Close returns to the plain channel view.
  await page.getByTestId("auxiliary-panel-close").click();
  await expect(page.getByTestId("markdown-doc-panel")).toHaveCount(0);
});

test("narrow layout moves focus into the panel and returns it to the card", async ({
  page,
}) => {
  await sendMarkdownAttachment(page);

  // Below the split-pane threshold the panel replaces the channel section,
  // unmounting the focused attachment card — the focus contract under test.
  await page.setViewportSize({ width: 560, height: 720 });

  const card = page.getByTestId("file-card").last();
  await card.focus();
  await expect(card).toBeFocused();
  await page.keyboard.press("Enter");

  // Focus lands on the panel's close control, not <body>.
  const panel = page.getByTestId("markdown-doc-panel");
  await expect(panel).toBeVisible();
  await expect(page.getByTestId("auxiliary-panel-close")).toBeFocused();

  // Escape closes the panel and hands focus back to the invoking card —
  // overriding the remounted channel's composer autofocus.
  await page.keyboard.press("Escape");
  await expect(page.getByTestId("markdown-doc-panel")).toHaveCount(0);
  await expect(page.getByTestId("file-card").last()).toBeFocused();
});

test("close restores focus to the invoking card when the document appears twice", async ({
  page,
}) => {
  // The same attachment in two messages gives two cards with one URL, so a
  // URL-only anchor would always restore the first DOM match. The invoking
  // card's recorded identity must win.
  await sendMarkdownAttachment(page);
  await attachAndSendMarkdown(page);

  await page.setViewportSize({ width: 560, height: 720 });

  const docCards = page.locator(
    `[data-testid="file-card"][data-doc-url="${DOC_URL}"]`,
  );
  await expect(docCards).toHaveCount(2);

  // Open from the SECOND card.
  await docCards.nth(1).focus();
  await page.keyboard.press("Enter");
  await expect(page.getByTestId("auxiliary-panel-close")).toBeFocused();

  await page.keyboard.press("Escape");
  await expect(page.getByTestId("markdown-doc-panel")).toHaveCount(0);
  await expect(docCards).toHaveCount(2);
  await expect(docCards.nth(1)).toBeFocused();
  await expect(docCards.nth(0)).not.toBeFocused();
});

test("open document survives reload and back/forward navigation", async ({
  page,
}) => {
  await sendMarkdownAttachment(page);
  await page.getByTestId("file-card").last().click();

  const panel = () => page.getByTestId("markdown-doc-panel");
  await expect(
    panel().getByRole("heading", { name: "Release Notes" }),
  ).toBeVisible();

  // The document lives in the URL (`doc`/`docName` params), so a reload
  // must restore the open panel with its content.
  await page.reload();
  await expect(
    panel().getByRole("heading", { name: "Release Notes" }),
  ).toBeVisible();

  // Opening the panel pushed a history entry: back closes it, forward
  // restores it — the advertised back/forward contract.
  await page.goBack();
  await expect(page.getByTestId("markdown-doc-panel")).toHaveCount(0);
  await page.goForward();
  await expect(
    panel().getByRole("heading", { name: "Release Notes" }),
  ).toBeVisible();
});

test("a document over the native 2 MiB cap falls back to download", async ({
  page,
}) => {
  // The imeta `size` is untrusted and here it lies small — the card offers
  // the viewer. The served body is over the cap, so the (mocked) native
  // fetch refuses it mid-transfer and the panel must show the too-large
  // fallback instead of rendering, proving enforcement does not rest on
  // the advertised size.
  await page.unroute(`**/media/${DOC_SHA}.bin`);
  await page.route(`**/media/${DOC_SHA}.bin`, (route) =>
    route.fulfill({
      body: Buffer.alloc(2 * 1024 * 1024 + 1, 0x61),
      contentType: "application/octet-stream",
    }),
  );
  await sendMarkdownAttachment(page);

  const card = page.getByTestId("file-card").last();
  await expect(card).toHaveAttribute("aria-label", "Open release-notes.md");
  await card.click();

  const panel = page.getByTestId("markdown-doc-panel");
  await expect(panel).toBeVisible();
  await expect(panel).toContainText("This file is too large to preview.");
  await expect(
    panel.getByRole("button", { name: "Download file" }),
  ).toBeVisible();
});

test("a binary payload behind a .md filename falls back to the invalid-text error with a working download", async ({
  page,
}) => {
  // The imeta filename says ".md" (so the card offers the viewer), but the
  // served bytes are not valid UTF-8 — strict decoding must reject them
  // rather than render mojibake, and the download escape hatch must still
  // work from that error state.
  await page.unroute(`**/media/${DOC_SHA}.bin`);
  await page.route(`**/media/${DOC_SHA}.bin`, (route) =>
    route.fulfill({
      body: Buffer.from([0xff, 0xfe, 0xfd, 0x00, 0x01, 0x02]),
      contentType: "application/octet-stream",
    }),
  );
  await sendMarkdownAttachment(page);

  const card = page.getByTestId("file-card").last();
  await expect(card).toHaveAttribute("aria-label", "Open release-notes.md");
  await card.click();

  const panel = page.getByTestId("markdown-doc-panel");
  await expect(panel).toBeVisible();
  await expect(panel).toContainText(
    "This file isn't valid text, so it can't be previewed.",
  );
  const downloadFallback = panel.getByRole("button", {
    name: "Download file",
  });
  await expect(downloadFallback).toBeVisible();

  const commands = () =>
    page.evaluate(
      () =>
        (window as Window & { __BUZZ_E2E_COMMANDS__?: string[] })
          .__BUZZ_E2E_COMMANDS__ ?? [],
    );
  await downloadFallback.click();
  await expect.poll(commands).toContain("download_file");
});

test("a relay fetch failure falls back to the couldn't-load error", async ({
  page,
}) => {
  // A 404 (deleted/expired media) or a network failure must not leave the
  // panel stuck loading — it must name the failure and still offer the
  // download escape hatch.
  await page.unroute(`**/media/${DOC_SHA}.bin`);
  await page.route(`**/media/${DOC_SHA}.bin`, (route) =>
    route.fulfill({ status: 404, body: "not found" }),
  );
  await sendMarkdownAttachment(page);

  const card = page.getByTestId("file-card").last();
  await expect(card).toHaveAttribute("aria-label", "Open release-notes.md");
  await card.click();

  const panel = page.getByTestId("markdown-doc-panel");
  await expect(panel).toBeVisible();
  await expect(panel).toContainText("Couldn't load this file from the relay.");
  await expect(
    panel.getByRole("button", { name: "Download file" }),
  ).toBeVisible();
});

test("a network failure fetching the relay media falls back to the couldn't-load error", async ({
  page,
}) => {
  await page.unroute(`**/media/${DOC_SHA}.bin`);
  await page.route(`**/media/${DOC_SHA}.bin`, (route) =>
    route.abort("connectionrefused"),
  );
  await sendMarkdownAttachment(page);

  const card = page.getByTestId("file-card").last();
  await card.click();

  const panel = page.getByTestId("markdown-doc-panel");
  await expect(panel).toBeVisible();
  await expect(panel).toContainText("Couldn't load this file from the relay.");
  await expect(
    panel.getByRole("button", { name: "Download file" }),
  ).toBeVisible();
});

test("non-markdown attachments keep the download-card behavior", async ({
  page,
}) => {
  await page.goto("/");
  await page.getByTestId("channel-general").click();

  // Re-point the mock upload at a PDF: same flow, no viewer affordance.
  await page.evaluate(() => {
    const e2e = (
      window as Window & {
        __BUZZ_E2E__?: {
          mock?: { uploadDescriptors?: Array<Record<string, unknown>> };
        };
      }
    ).__BUZZ_E2E__;
    if (e2e?.mock) {
      e2e.mock.uploadDescriptors = [
        {
          url: `http://localhost:3000/media/${"c".repeat(64)}.pdf`,
          sha256: "c".repeat(64),
          size: 128,
          type: "application/pdf",
          uploaded: 1_700_000_000,
          filename: "report.pdf",
        },
      ];
    }
  });

  const [chooser] = await Promise.all([
    page.waitForEvent("filechooser"),
    page.getByRole("button", { name: "Attach file" }).click(),
  ]);
  await chooser.setFiles({
    buffer: Buffer.from("pdf bytes"),
    mimeType: "application/pdf",
    name: "report.pdf",
  });
  await expect(page.getByTestId("message-composer")).toContainText(
    "report.pdf",
  );
  await page.getByTestId("send-message").click();
  await expect(page.getByText("Sending")).toHaveCount(0);

  const card = page.getByTestId("file-card").last();
  await expect(card).toContainText("report.pdf");
  await expect(card).toHaveAttribute("aria-label", "Download report.pdf");
  await card.click();
  await expect
    .poll(() =>
      page.evaluate(
        () =>
          (window as Window & { __BUZZ_E2E_COMMANDS__?: string[] })
            .__BUZZ_E2E_COMMANDS__ ?? [],
      ),
    )
    .toContain("download_file");
  await expect(page.getByTestId("markdown-doc-panel")).toHaveCount(0);
});

// ── Panel-ready performance (T2 acceptance: fixed ~500 KB fixture, panel-ready
// under 1.0s, no main-thread task over 200ms, measured three times) ─────────
//
// Fixture hash (T2 acceptance clause: "hash recorded in the PR"), recompute
// with `shasum -a 256 desktop/tests/fixtures/long-doc.md` and diff on any
// change to the fixture:
//   SHA-256: 2eb8167839c832fea513de6d1d6dd47b55b01a595cb4167d4d54155446044ae1
//   Bytes:   506681

const LONG_DOC_CONTENT = readFileSync(
  new URL("../fixtures/long-doc.md", import.meta.url),
  "utf-8",
);
const LONG_DOC_SHA = "c".repeat(64);
const LONG_DOC_URL = `${RELAY_HTTP_URL}/media/${LONG_DOC_SHA}.bin`;
const PANEL_READY_BUDGET_MS = 1000;
const MAIN_THREAD_TASK_BUDGET_MS = 200;
const MEASURED_RUNS = 3;

test("MEASURE: a fixed ~500 KB document reaches panel-ready under 1.0s with no main-thread task over 200ms", async ({
  page,
}) => {
  test.setTimeout(60_000);

  await installMockBridge(page, {
    deferredComposerUploads: true,
    uploadDescriptors: [
      {
        url: LONG_DOC_URL,
        sha256: LONG_DOC_SHA,
        size: Buffer.byteLength(LONG_DOC_CONTENT),
        type: "application/octet-stream",
        uploaded: Math.floor(Date.now() / 1000),
        filename: "long-doc.md",
      },
    ],
  });
  await page.route(`**/media/${LONG_DOC_SHA}.bin`, (route) =>
    route.fulfill({
      body: LONG_DOC_CONTENT,
      contentType: "application/octet-stream",
    }),
  );

  // Arm a longtask observer before the first navigation so it is present for
  // every panel-open measured below. `buffered: true` catches entries queued
  // before a given read; each run clears the buffer immediately before its
  // own click so only that run's work is attributed to it.
  await page.addInitScript(() => {
    const store = window as unknown as { __LONGTASKS__?: number[] };
    store.__LONGTASKS__ = [];
    new PerformanceObserver((list) => {
      for (const entry of list.getEntries()) {
        store.__LONGTASKS__?.push(entry.duration);
      }
    }).observe({ type: "longtask", buffered: true });
  });

  await page.goto("/");
  await page.getByTestId("channel-general").click();
  await expect(page.getByTestId("chat-title")).toHaveText("general");

  const [chooser] = await Promise.all([
    page.waitForEvent("filechooser"),
    page.getByRole("button", { name: "Attach file" }).click(),
  ]);
  await chooser.setFiles({
    buffer: Buffer.from(LONG_DOC_CONTENT),
    mimeType: "text/markdown",
    name: "long-doc.md",
  });
  await expect(page.getByTestId("message-composer")).toContainText(
    "long-doc.md",
  );
  await page.getByTestId("send-message").click();
  await expect(page.getByText("Sending")).toHaveCount(0);

  const card = page.getByTestId("file-card").last();
  await expect(card).toContainText("long-doc.md");

  const panel = page.getByTestId("markdown-doc-panel");
  const readyHeading = panel.getByRole("heading", {
    name: "Long Document Fixture",
  });

  for (let run = 1; run <= MEASURED_RUNS; run += 1) {
    await page.evaluate(() => {
      (window as unknown as { __LONGTASKS__?: number[] }).__LONGTASKS__ = [];
    });

    // The panel's `useQuery` uses `staleTime: Infinity` (deliberately — a
    // fetched document never changes under its content-addressed URL). Left
    // alone, that means only the FIRST of these three runs pays for the
    // fetch + UTF-8 decode; runs 2 and 3 would hit the warm cache and time
    // nothing but a re-render. Remove the cached entry before every run
    // (through the harness's existing E2E query-client handle) so all three
    // measurements are genuine cold panel-opens.
    await page.evaluate(() => {
      const client = (
        window as unknown as {
          __BUZZ_E2E_QUERY_CLIENT__?: {
            removeQueries: (filters: { queryKey: unknown[] }) => void;
          };
        }
      ).__BUZZ_E2E_QUERY_CLIENT__;
      client?.removeQueries({ queryKey: ["markdown-doc"] });
    });

    // Measure entirely inside the page: dispatch the click and detect the
    // ready heading via `requestAnimationFrame` polling, timed with
    // `performance.now()` (sub-millisecond resolution, per the ticket's
    // named Performance API) rather than a Node-side `Date.now()` wrapped
    // around a Playwright locator wait — whose own polling cadence adds
    // slack that is significant against a 1000ms budget.
    const panelReadyMs = await page.evaluate(() => {
      const HEADING_TEXT = "Long Document Fixture";
      return new Promise<number>((resolve, reject) => {
        const cards = document.querySelectorAll<HTMLElement>(
          '[data-testid="file-card"]',
        );
        const target = cards[cards.length - 1];
        if (!target) {
          reject(new Error("No file-card found to open the panel."));
          return;
        }
        const start = performance.now();
        target.click();
        const poll = () => {
          const heading = Array.from(
            document.querySelectorAll<HTMLElement>(
              '[data-testid="markdown-doc-panel"] h1',
            ),
          ).find((el) => el.textContent?.trim() === HEADING_TEXT);
          if (heading) {
            const rect = heading.getBoundingClientRect();
            if (rect.width > 0 && rect.height > 0) {
              resolve(performance.now() - start);
              return;
            }
          }
          requestAnimationFrame(poll);
        };
        requestAnimationFrame(poll);
      });
    });

    expect(
      panelReadyMs,
      `run ${run}: panel-ready took ${panelReadyMs}ms, over the ${PANEL_READY_BUDGET_MS}ms budget`,
    ).toBeLessThan(PANEL_READY_BUDGET_MS);

    // Re-confirm through the normal Playwright API too — cheap, since the
    // in-page wait above has already resolved by the time this runs.
    await expect(readyHeading).toBeVisible();

    const longtasks = await page.evaluate(
      () =>
        (window as unknown as { __LONGTASKS__?: number[] }).__LONGTASKS__ ?? [],
    );
    const worstTaskMs = longtasks.length ? Math.max(...longtasks) : 0;

    expect(
      worstTaskMs,
      `run ${run}: longest main-thread task was ${worstTaskMs}ms, over the ${MAIN_THREAD_TASK_BUDGET_MS}ms budget`,
    ).toBeLessThan(MAIN_THREAD_TASK_BUDGET_MS);

    await page.getByTestId("auxiliary-panel-close").click();
    await expect(page.getByTestId("markdown-doc-panel")).toHaveCount(0);
  }
});

// ── Adversarial complexity, not just bytes (Sol audit finding 1) ──────────
//
// A valid under-cap document can still carry hundreds of thousands of
// block-level nodes if it is mostly one-line list items — the audit's own
// reproduction shape ("- a\n" repeated) parses at superlinear cost through
// mdast/micromark, well under the byte cap and before any React element
// exists. Both views must stay responsive without ever attempting that
// parse: Preview refuses it for a bounded fallback; Code view (which never
// runs the mdast parse) stays available but bounds its own rendering
// instead of one <span> per line.
//
// Fixture hash, recompute with
// `shasum -a 256 desktop/tests/fixtures/adversarial-list.md`:
//   SHA-256: b036c0de7e913846674358b662d4e47dc20b1cc64a63420ed287eacd95d0a43e
//   Bytes:   2000000
//   Lines:   500000

const ADVERSARIAL_LIST_CONTENT = readFileSync(
  new URL("../fixtures/adversarial-list.md", import.meta.url),
  "utf-8",
);
const ADVERSARIAL_LIST_SHA = "d".repeat(64);
const ADVERSARIAL_LIST_URL = `${RELAY_HTTP_URL}/media/${ADVERSARIAL_LIST_SHA}.bin`;

test("an adversarially list-dense document shows a bounded Preview fallback and a bounded Code view, not a freeze", async ({
  page,
}) => {
  test.setTimeout(30_000);

  await installMockBridge(page, {
    deferredComposerUploads: true,
    uploadDescriptors: [
      {
        url: ADVERSARIAL_LIST_URL,
        sha256: ADVERSARIAL_LIST_SHA,
        size: Buffer.byteLength(ADVERSARIAL_LIST_CONTENT),
        type: "application/octet-stream",
        uploaded: Math.floor(Date.now() / 1000),
        filename: "adversarial-list.md",
      },
    ],
  });
  await page.route(`**/media/${ADVERSARIAL_LIST_SHA}.bin`, (route) =>
    route.fulfill({
      body: ADVERSARIAL_LIST_CONTENT,
      contentType: "application/octet-stream",
    }),
  );

  await page.goto("/");
  await page.getByTestId("channel-general").click();
  await expect(page.getByTestId("chat-title")).toHaveText("general");

  const [chooser] = await Promise.all([
    page.waitForEvent("filechooser"),
    page.getByRole("button", { name: "Attach file" }).click(),
  ]);
  await chooser.setFiles({
    buffer: Buffer.from(ADVERSARIAL_LIST_CONTENT),
    mimeType: "text/markdown",
    name: "adversarial-list.md",
  });
  await expect(page.getByTestId("message-composer")).toContainText(
    "adversarial-list.md",
  );
  await page.getByTestId("send-message").click();
  await expect(page.getByText("Sending")).toHaveCount(0);

  const card = page.getByTestId("file-card").last();
  await expect(card).toContainText("adversarial-list.md");

  const openedAt = Date.now();
  await card.click();

  const panel = page.getByTestId("markdown-doc-panel");
  await expect(panel).toBeVisible();

  // Preview refuses the full mdast parse — a bounded fallback message, not
  // 500,000 rendered list items.
  await expect(
    page.getByTestId("markdown-doc-preview-too-complex"),
  ).toBeVisible({ timeout: 5000 });
  expect(Date.now() - openedAt).toBeLessThan(5000);

  // Code view stays available — bounded, not one <span> per line.
  await page.getByTestId("markdown-doc-view-code").click();
  const codeView = page.getByTestId("markdown-doc-code");
  await expect(codeView).toBeVisible({ timeout: 5000 });
  await expect(codeView).toContainText("- a");
  await expect(codeView).toContainText("more lines not shown");
  const renderedLineCount = await codeView.locator("[data-line]").count();
  expect(renderedLineCount).toBeLessThan(2100);
});

// ── Adversarial complexity on a single line (Sol audit finding 1, round 2)
//
// The line-count gate above only bounds block-level node count, so a
// single-line document densely packed with inline link syntax passes it
// outright (1 line) while still driving the inline tokenizer into the same
// superlinear cost — the audit's own reproduction shape.
//
// Fixture, recompute with `shasum -a 256
// desktop/tests/fixtures/link-dense-line.md`:
//   SHA-256: d1f165cd61afa3898297ebba6777b83b6501504aec1315ef9989e7f91f9834fb
//   Bytes:   340000
//   Lines:   1

const LINK_DENSE_LINE_CONTENT = readFileSync(
  new URL("../fixtures/link-dense-line.md", import.meta.url),
  "utf-8",
);
const LINK_DENSE_LINE_SHA = "e".repeat(64);
const LINK_DENSE_LINE_URL = `${RELAY_HTTP_URL}/media/${LINK_DENSE_LINE_SHA}.bin`;

test("a one-line link-dense document shows a bounded Preview fallback, not a freeze", async ({
  page,
}) => {
  test.setTimeout(30_000);

  await installMockBridge(page, {
    deferredComposerUploads: true,
    uploadDescriptors: [
      {
        url: LINK_DENSE_LINE_URL,
        sha256: LINK_DENSE_LINE_SHA,
        size: Buffer.byteLength(LINK_DENSE_LINE_CONTENT),
        type: "application/octet-stream",
        uploaded: Math.floor(Date.now() / 1000),
        filename: "link-dense-line.md",
      },
    ],
  });
  await page.route(`**/media/${LINK_DENSE_LINE_SHA}.bin`, (route) =>
    route.fulfill({
      body: LINK_DENSE_LINE_CONTENT,
      contentType: "application/octet-stream",
    }),
  );

  await page.goto("/");
  await page.getByTestId("channel-general").click();
  await expect(page.getByTestId("chat-title")).toHaveText("general");

  const [chooser] = await Promise.all([
    page.waitForEvent("filechooser"),
    page.getByRole("button", { name: "Attach file" }).click(),
  ]);
  await chooser.setFiles({
    buffer: Buffer.from(LINK_DENSE_LINE_CONTENT),
    mimeType: "text/markdown",
    name: "link-dense-line.md",
  });
  await expect(page.getByTestId("message-composer")).toContainText(
    "link-dense-line.md",
  );
  await page.getByTestId("send-message").click();
  await expect(page.getByText("Sending")).toHaveCount(0);

  const card = page.getByTestId("file-card").last();
  await expect(card).toContainText("link-dense-line.md");

  const openedAt = Date.now();
  await card.click();

  const panel = page.getByTestId("markdown-doc-panel");
  await expect(panel).toBeVisible();

  // A single line passes the line-count gate outright; the link-marker
  // density gate must still refuse the full mdast parse.
  await expect(
    page.getByTestId("markdown-doc-preview-too-complex"),
  ).toBeVisible({ timeout: 5000 });
  expect(Date.now() - openedAt).toBeLessThan(5000);
});

test("Code view for a large-but-few-lines document skips synchronous tokenization (highlight byte bound)", async ({
  page,
}) => {
  // long-doc.md (122 lines, 506,681 bytes) sits under the Code view's
  // 150-line highlight cap but well over its byte cap — the sub-axis the
  // MEASURE spec above never exercises (it never opens Code view).
  test.setTimeout(30_000);

  await installMockBridge(page, {
    deferredComposerUploads: true,
    uploadDescriptors: [
      {
        url: LONG_DOC_URL,
        sha256: LONG_DOC_SHA,
        size: Buffer.byteLength(LONG_DOC_CONTENT),
        type: "application/octet-stream",
        uploaded: Math.floor(Date.now() / 1000),
        filename: "long-doc.md",
      },
    ],
  });
  await page.route(`**/media/${LONG_DOC_SHA}.bin`, (route) =>
    route.fulfill({
      body: LONG_DOC_CONTENT,
      contentType: "application/octet-stream",
    }),
  );

  await page.goto("/");
  await page.getByTestId("channel-general").click();
  await expect(page.getByTestId("chat-title")).toHaveText("general");

  const [chooser] = await Promise.all([
    page.waitForEvent("filechooser"),
    page.getByRole("button", { name: "Attach file" }).click(),
  ]);
  await chooser.setFiles({
    buffer: Buffer.from(LONG_DOC_CONTENT),
    mimeType: "text/markdown",
    name: "long-doc.md",
  });
  await expect(page.getByTestId("message-composer")).toContainText(
    "long-doc.md",
  );
  await page.getByTestId("send-message").click();
  await expect(page.getByText("Sending")).toHaveCount(0);

  const card = page.getByTestId("file-card").last();
  await card.click();
  const panel = page.getByTestId("markdown-doc-panel");
  await expect(panel).toBeVisible();

  await page.getByTestId("markdown-doc-view-code").click();
  const codeView = page.getByTestId("markdown-doc-code");
  await expect(codeView).toBeVisible({ timeout: 5000 });
  await expect(codeView).toContainText("Long Document Fixture");
  // Well under the 2,000-line plain-text bound, so no truncation notice —
  // true regardless of which rendering path produced the text, so on its
  // own this does not prove synchronous tokenization was actually skipped
  // (Sol audit finding 2, round 2).
  await expect(page.getByTestId("code-block-truncated-notice")).toHaveCount(0);
  // Bind the actual production decision instead: the plain-text fallback
  // (CodeBlock.tsx) emits `<span data-line="">{line}</span>` with the line
  // text as a bare text node, while the highlighted path emits nested
  // `<span style="color:...">` children per token. Zero element children
  // under `[data-line]` here proves the byte gate routed this document to
  // the fallback rather than tokenizing it — this fails if the byte check
  // is removed, since the surviving line-count cap (150) does not catch a
  // 122-line document.
  //
  // Shiki's language/theme assets load asynchronously after mount, and the
  // pre-load render is the same plain-text fallback either way, so an
  // immediate read can't distinguish "byte-guarded" from "not yet loaded" —
  // give the async load time to settle before asserting the negative.
  await page.waitForTimeout(3000);
  const nestedTokenSpans = await codeView.locator("[data-line] > span").count();
  expect(nestedTokenSpans).toBe(0);
});

// ── Held-request cancellation (Sol audit finding 3) ────────────────────────
//
// `fetchMarkdownDocBytes` now shares `fetchMediaBytes`' renderer-owned
// cancellation handshake, so closing, replacing, or otherwise unmounting the
// panel while its native fetch is in flight must cancel the native request
// rather than leaving it (and its socket) running for up to the download
// timeout. `__BUZZ_E2E_HOLD_MEDIA_FETCHES__` holds every mock fetch open
// until cancelled, so `__BUZZ_E2E_MEDIA_FETCH_STATE__.active` staying pinned
// at the open count — and returning to zero only once every panel is truly
// gone — proves the cancellation handshake actually fires, not just that
// the UI stopped showing the request.

/** Reconfigures the mock's single upload descriptor and route to a distinct
 * document, then attaches + sends it — used to prove a *replacement* fetch
 * (a different `doc` URL opened over an in-flight one) is cancelled, not
 * just a same-URL reopen (which would share the in-flight query). */
async function attachAndSendDistinctMarkdown(
  page: Page,
  {
    url,
    sha,
    content,
    filename,
  }: { url: string; sha: string; content: string; filename: string },
) {
  await page.evaluate(
    ({ url, sha, content, filename }) => {
      const e2e = (
        window as Window & {
          __BUZZ_E2E__?: {
            mock?: { uploadDescriptors?: Array<Record<string, unknown>> };
          };
        }
      ).__BUZZ_E2E__;
      if (e2e?.mock) {
        e2e.mock.uploadDescriptors = [
          {
            url,
            sha256: sha,
            size: content.length,
            type: "application/octet-stream",
            uploaded: Math.floor(Date.now() / 1000),
            filename,
          },
        ];
      }
    },
    { url, sha, content, filename },
  );
  await page.route(`**/media/${sha}.bin`, (route) =>
    route.fulfill({ body: content, contentType: "application/octet-stream" }),
  );
  const [chooser] = await Promise.all([
    page.waitForEvent("filechooser"),
    page.getByRole("button", { name: "Attach file" }).click(),
  ]);
  await chooser.setFiles({
    buffer: Buffer.from(content),
    mimeType: "text/markdown",
    name: filename,
  });
  await expect(page.getByTestId("message-composer")).toContainText(filename);
  await page.getByTestId("send-message").click();
  await expect(page.getByText("Sending")).toHaveCount(0);
}

test("closing, replacing, and switching channels releases held native document fetches", async ({
  page,
}) => {
  await page.addInitScript(() => {
    window.__BUZZ_E2E_HOLD_MEDIA_FETCHES__ = true;
  });
  await sendMarkdownAttachment(page);

  const activeCount = () =>
    page.evaluate(() => window.__BUZZ_E2E_MEDIA_FETCH_STATE__?.active ?? -1);
  const commandCounts = () =>
    page.evaluate(() => {
      const commands = window.__BUZZ_E2E_COMMANDS__ ?? [];
      return {
        fetched: commands.filter((c) => c === "fetch_markdown_doc_bytes")
          .length,
        cancelled: commands.filter((c) => c === "cancel_media_fetch").length,
        released: commands.filter((c) => c === "release_media_fetch").length,
      };
    });

  // Locators scoped by `data-doc-url` rather than `.last()`: two distinct
  // cards exist by the "replacement" step below, and `.last()` re-resolves
  // to whichever card is newest at click time — not necessarily the one
  // captured here.
  const firstCard = page.locator(
    `[data-testid="file-card"][data-doc-url="${DOC_URL}"]`,
  );

  // ── close ──────────────────────────────────────────────────────────────
  await firstCard.click();
  await expect(page.getByTestId("markdown-doc-panel")).toBeVisible();
  await expect.poll(activeCount).toBe(1);

  await page.getByTestId("auxiliary-panel-close").click();
  await expect(page.getByTestId("markdown-doc-panel")).toHaveCount(0);
  await expect.poll(activeCount).toBe(0);

  // ── replacement (a different document opened over an in-flight one) ────
  const secondUrl = `${RELAY_HTTP_URL}/media/${"e".repeat(64)}.bin`;
  await attachAndSendDistinctMarkdown(page, {
    url: secondUrl,
    sha: "e".repeat(64),
    content: "# Second Doc\n\nMore content.\n",
    filename: "second-doc.md",
  });
  const secondCard = page.locator(
    `[data-testid="file-card"][data-doc-url="${secondUrl}"]`,
  );
  await secondCard.click();
  await expect(page.getByTestId("markdown-doc-panel")).toBeVisible();
  await expect.poll(activeCount).toBe(1);

  // Opening the (still-open, held) first document again over the second —
  // MarkdownDocAuxiliaryPanel keys by `doc.url`, so this unmounts the
  // second document's panel (and its query) and mounts a fresh one for the
  // first document's URL. `active` must settle back at 1, not 2: the
  // superseded fetch has to actually release, not just stop being shown.
  await firstCard.click();
  await expect.poll(activeCount).toBe(1);

  await page.getByTestId("auxiliary-panel-close").click();
  await expect(page.getByTestId("markdown-doc-panel")).toHaveCount(0);
  await expect.poll(activeCount).toBe(0);

  // ── channel switch (unmounts the whole channel section, panel included —
  // the same full-unmount shape as a community switch) ───────────────────
  await secondCard.click();
  await expect(page.getByTestId("markdown-doc-panel")).toBeVisible();
  await expect.poll(activeCount).toBe(1);

  await page.getByTestId("channel-random").click();
  await expect(page.getByTestId("markdown-doc-panel")).toHaveCount(0);
  await expect.poll(activeCount).toBe(0);

  const counts = await commandCounts();
  expect(counts.fetched).toBeGreaterThanOrEqual(4);
  expect(counts.cancelled).toBe(counts.fetched);
  expect(counts.released).toBe(counts.fetched);
});
