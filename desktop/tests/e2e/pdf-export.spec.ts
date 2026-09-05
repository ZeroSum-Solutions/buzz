import { expect, test } from "@playwright/test";
import type { Page } from "@playwright/test";

import { installMockBridge } from "../helpers/bridge";

// Exercises Export PDF from the markdown viewer panel through the mock Tauri
// bridge: the panel's action renders the document in document mode and hands
// the resulting HTML to `export_document_pdf`.
//
// The bridge cannot run the native save dialog or a headless browser, so the
// real render is covered in Rust
// (`commands::pdf_export::tests::renders_the_fixture_to_a_three_page_pdf`,
// which prints the same fixture and checks page count, extracted text and a
// per-page raster). What this spec owns is the UI contract: what the panel
// sends, and how it reports a save, a cancelled dialog and a failure.

const RELAY_HTTP_URL =
  process.env.BUZZ_E2E_RELAY_URL ?? "http://localhost:3000";
const DOC_SHA = "c".repeat(64);
const DOC_URL = `${RELAY_HTTP_URL}/media/${DOC_SHA}.bin`;
const DOC_MARKDOWN = [
  "# Approval Page One",
  "",
  "See the [handbook](https://example.invalid/handbook).",
  "",
  "## Materials Table",
  "",
  "| Line | Lead Time |",
  "| --- | --- |",
  "| 1 | alpha-fixture-row |",
  "",
  "## Approval ID Generator",
  "",
  "```python",
  "# PDF_SPIKE_CODE_MARKER_7f3a",
  "```",
  "",
].join("\n");

// Above `MAX_MARKDOWN_DOC_PREVIEW_LINES` (3,000) and far under the 2 MiB byte
// cap: the shape the panel refuses to preview, and so must not offer to print.
const COMPLEX_MARKDOWN = "- entry\n".repeat(3200);

type PdfExportMode = "saved" | "cancelled" | "failed";

test.beforeEach(async ({ page }) => {
  await installMockBridge(page, {
    deferredComposerUploads: true,
    uploadDescriptors: [
      {
        url: DOC_URL,
        sha256: DOC_SHA,
        size: DOC_MARKDOWN.length,
        type: "application/octet-stream",
        uploaded: Math.floor(Date.now() / 1000),
        filename: "approval.md",
      },
    ],
  });
  await page.route(`**/media/${DOC_SHA}.bin`, (route) =>
    route.fulfill({
      body: DOC_MARKDOWN,
      contentType: "application/octet-stream",
    }),
  );
});

async function setExportMode(page: Page, mode: PdfExportMode) {
  await page.addInitScript((value) => {
    (
      window as unknown as { __BUZZ_E2E_PDF_EXPORT_MODE__?: string }
    ).__BUZZ_E2E_PDF_EXPORT_MODE__ = value;
  }, mode);
}

async function openDocumentPanel(page: Page, body = DOC_MARKDOWN) {
  await page.goto("/");
  await page.getByTestId("channel-general").click();
  await expect(page.getByTestId("chat-title")).toHaveText("general");

  const [chooser] = await Promise.all([
    page.waitForEvent("filechooser"),
    page.getByRole("button", { name: "Attach file" }).click(),
  ]);
  await chooser.setFiles({
    buffer: Buffer.from(body),
    mimeType: "text/markdown",
    name: "approval.md",
  });
  await expect(page.getByTestId("message-composer")).toContainText(
    "approval.md",
  );
  await page.getByTestId("send-message").click();
  await expect(page.getByText("Sending")).toHaveCount(0);

  await page
    .locator(`[data-testid="file-card"][data-doc-url="${DOC_URL}"]`)
    .click();
  await expect(page.getByTestId("markdown-doc-panel")).toBeVisible();
}

async function exportPayloads(page: Page) {
  return page.evaluate(() =>
    (
      window as unknown as {
        __BUZZ_E2E_COMMAND_PAYLOADS__?: Array<{
          command: string;
          payload: unknown;
        }>;
      }
    ).__BUZZ_E2E_COMMAND_PAYLOADS__?.filter(
      (entry) => entry.command === "export_document_pdf",
    ),
  );
}

test("Export PDF sends the document-mode render of the open document", async ({
  page,
}) => {
  await setExportMode(page, "saved");
  await openDocumentPanel(page);

  await page.getByTestId("markdown-doc-export-pdf").click();
  await expect(page.getByText("Exported approval.md as PDF")).toBeVisible();

  const calls = await exportPayloads(page);
  expect(calls?.length).toBe(1);
  const payload = calls?.[0]?.payload as {
    bodyHtml: string;
    title: string;
    filename: string;
  };
  expect(payload.title).toBe("approval");
  expect(payload.filename).toBe("approval.md");
  // Headings, table cells and the code marker line all survive the render …
  expect(payload.bodyHtml).toContain("<h1>Approval Page One</h1>");
  expect(payload.bodyHtml).toContain("alpha-fixture-row");
  expect(payload.bodyHtml).toContain("# PDF_SPIKE_CODE_MARKER_7f3a");
  // … links are kept …
  expect(payload.bodyHtml).toContain('href="https://example.invalid/handbook"');
  // … and the code block carries none of the viewer's collapse chrome.
  expect(payload.bodyHtml).not.toContain("data-code-block");
  expect(payload.bodyHtml).not.toContain("max-h-");
});

test("cancelling the save dialog reports nothing and writes nothing", async ({
  page,
}) => {
  await setExportMode(page, "cancelled");
  await openDocumentPanel(page);

  await page.getByTestId("markdown-doc-export-pdf").click();

  // The render happens before the command is invoked, so poll rather than
  // reading the log the instant after the click.
  await expect
    .poll(async () => (await exportPayloads(page))?.length ?? 0)
    .toBe(1);
  // A cancelled dialog is not a save and not a failure: no toast either way,
  // and the action returns to its idle state.
  await expect(page.getByTestId("markdown-doc-export-pdf")).toBeEnabled();
  await expect(page.getByText("Exported approval.md as PDF")).toHaveCount(0);
  await expect(page.locator("[data-sonner-toast]")).toHaveCount(0);
});

test("an export failure is surfaced instead of being swallowed", async ({
  page,
}) => {
  await setExportMode(page, "failed");
  await openDocumentPanel(page);

  await page.getByTestId("markdown-doc-export-pdf").click();
  await expect(
    page.getByText("PDF export needs Google Chrome or Chromium installed"),
  ).toBeVisible();
  await expect(page.getByTestId("markdown-doc-export-pdf")).toBeEnabled();
});

test("a document too complex to preview offers no Export PDF action", async ({
  page,
}) => {
  // The Export action and the Preview body are gated by one predicate
  // (`isMarkdownDocTooComplexForPreview`), so the affordance and the panel
  // agree. Before that gate the button was live on exactly these documents,
  // and clicking it ran the parse the panel had just refused. This test fails
  // if the button is offered again.
  await page.route(`**/media/${DOC_SHA}.bin`, (route) =>
    route.fulfill({
      body: COMPLEX_MARKDOWN,
      contentType: "application/octet-stream",
    }),
  );
  await setExportMode(page, "saved");
  await openDocumentPanel(page, COMPLEX_MARKDOWN);

  await expect(
    page.getByTestId("markdown-doc-preview-too-complex"),
  ).toBeVisible();
  await expect(page.getByTestId("markdown-doc-export-pdf")).toHaveCount(0);
  expect((await exportPayloads(page))?.length ?? 0).toBe(0);
});
