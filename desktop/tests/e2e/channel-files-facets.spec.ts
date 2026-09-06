import { expect, test } from "@playwright/test";
import type { Page } from "@playwright/test";

import { installMockBridge } from "../helpers/bridge";

const CHANNEL_NAME = "general";
const SEEDED_FILES = 250;
/** The seed cycles five extensions, so md + pdf + csv is three fifths. */
const SEEDED_DOCUMENTS = (SEEDED_FILES / 5) * 3;
/** A facet or sort change must land this fast over the seeded index. */
const FACET_BUDGET_MS = 100;
const CANVAS_BODY = "# Kickoff\n\nThe plan lives here.";
const SORT_KEYS = ["oldest", "name", "size", "author", "newest"] as const;

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

/** Wait until a facet button reports `count` rows. */
async function waitForFacetCount(page: Page, facet: string, expected: number) {
  await page.waitForFunction(
    ({ label, count }) => {
      const button = [...document.querySelectorAll("button")].find((node) =>
        node.textContent?.trim().startsWith(label),
      );
      return button?.textContent?.includes(String(count)) ?? false;
    },
    { label: facet, count: expected },
    { timeout: 15_000 },
  );
}

/** The filenames the list is currently showing, in render order. */
async function listedNames(page: Page): Promise<string[]> {
  return page.evaluate(() =>
    [
      ...document.querySelectorAll(
        "[data-testid='channel-files-list'] a[title]",
      ),
    ]
      .map((node) => node.getAttribute("title") ?? "")
      .filter((title) => title !== ""),
  );
}

/**
 * Time one facet or sort change *inside the page*: from the event the control
 * dispatches to the first mutation of the list. Measuring here rather than
 * across the Playwright bridge keeps IPC round-trips out of the number, so the
 * budget is the tab's own filter-and-sort cost over the loaded index.
 */
async function measureListUpdate(
  page: Page,
  action: { kind: "sort"; value: string } | { kind: "facet"; label: string },
): Promise<number> {
  return page.evaluate(async (input) => {
    const rendered = () =>
      [
        ...document.querySelectorAll(
          "[data-testid='channel-files-list'] a[title]",
        ),
      ]
        .map((node) => node.getAttribute("title") ?? "")
        .join("|");

    const before = rendered();
    if (before === "") throw new Error("The file list is empty.");

    if (input.kind === "sort") {
      const select = document.querySelector<HTMLSelectElement>(
        "select[aria-label='Sort files']",
      );
      if (!select) throw new Error("The sort control is not rendered.");
      // React tracks the DOM value it wrote, so a plain assignment can be
      // swallowed as "unchanged"; the prototype setter is what the framework's
      // own change path sees.
      const setValue = Object.getOwnPropertyDescriptor(
        HTMLSelectElement.prototype,
        "value",
      )?.set;
      if (!setValue) throw new Error("No value setter on HTMLSelectElement.");
      setValue.call(select, input.value);
      select.dispatchEvent(new Event("change", { bubbles: true }));
    } else {
      const button = [...document.querySelectorAll("button")].find((node) =>
        node.textContent?.trim().startsWith(input.label),
      );
      if (!button) throw new Error(`No facet button for ${input.label}.`);
      (button as HTMLButtonElement).click();
    }

    const started = performance.now();
    for (;;) {
      const elapsed = performance.now() - started;
      if (rendered() !== before) return elapsed;
      if (elapsed > 10_000) {
        throw new Error(
          `The list never changed after ${JSON.stringify(input)} fired.`,
        );
      }
      await new Promise((resolve) => window.setTimeout(resolve, 0));
    }
  }, action);
}

const EXTENSIONS = ["md", "png", "pdf", "csv", "mp4"];

/**
 * Name, size and arrival order are three different permutations of the same
 * seed, so no two sort keys can agree by accident — a sort that silently fell
 * back to arrival order would still look sorted without this.
 */
function seedOrdinal(index: number, count: number): number {
  return ((index * 97 + 137) % count) + 1;
}

function seedSize(index: number, count: number): number {
  return 1024 + ((index * 89 + 31) % count);
}

function seedName(index: number, count: number): string {
  const ordinal = String(seedOrdinal(index, count)).padStart(4, "0");
  return `file-${ordinal}.${EXTENSIONS[index % EXTENSIONS.length]}`;
}

/** The index whose seeded row sorts first under `rank` (smaller sorts first). */
function firstBy(count: number, rank: (index: number) => number): string {
  let best = 0;
  for (let index = 1; index < count; index += 1) {
    if (rank(index) < rank(best)) best = index;
  }
  return seedName(best, count);
}

/**
 * Seed a channel past one history page with a mix of types. Every row claims
 * `application/octet-stream`, so only the filename can classify it — the case
 * the Documents facet exists for.
 */
async function seedFiles(page: Page, channelName: string, total: number) {
  await page.evaluate(
    ({ name, count, rows }) => {
      const emit = window.__BUZZ_E2E_EMIT_MOCK_MESSAGE__;
      if (!emit) throw new Error("Mock message emitter is unavailable.");
      const base = Math.floor(Date.now() / 1000) - count - 10;
      rows.forEach((row, index) => {
        emit({
          channelName: name,
          content: `attachment ${index}`,
          createdAt: base + index,
          extraTags: [
            [
              "imeta",
              `url http://localhost:3000/media/seeded-${index}.bin`,
              "m application/octet-stream",
              `size ${row.size}`,
              `filename ${row.filename}`,
            ],
          ],
        });
      });
    },
    {
      name: channelName,
      count: total,
      rows: Array.from({ length: total }, (_, index) => ({
        filename: seedName(index, total),
        size: seedSize(index, total),
      })),
    },
  );
}

test.describe("channel files facets", () => {
  test("sorts and filters the loaded index inside the budget", async ({
    page,
  }) => {
    await installMockBridge(page);
    await page.goto("/");
    await page.getByTestId(`channel-${CHANNEL_NAME}`).click();
    await expect(page.getByTestId("chat-title")).toHaveText(CHANNEL_NAME);
    await waitForMockLiveSubscription(page, CHANNEL_NAME);
    await seedFiles(page, CHANNEL_NAME, SEEDED_FILES);

    await page.getByRole("tab", { name: "Files" }).click();
    await waitForFacetCount(page, "All", SEEDED_FILES);

    // Classification is from the filename: every seeded row claims
    // `application/octet-stream`, so a MIME-only rule would count zero here.
    await waitForFacetCount(page, "Documents", SEEDED_DOCUMENTS);

    const timings: { what: string; run: number; elapsed: number }[] = [];
    const firstBySort = new Map<string, string>();
    const sortControl = page.getByRole("combobox", { name: "Sort files" });

    for (const key of SORT_KEYS) {
      for (let run = 0; run < 3; run += 1) {
        // Bounce off the key under test so each run is a real re-sort.
        await sortControl.selectOption(key === "newest" ? "oldest" : "newest");
        await expect(sortControl).toHaveValue(
          key === "newest" ? "oldest" : "newest",
        );
        timings.push({
          what: `sort:${key}`,
          run,
          elapsed: await measureListUpdate(page, { kind: "sort", value: key }),
        });
        await expect(sortControl).toHaveValue(key);
      }
      firstBySort.set(key, (await listedNames(page))[0]);
    }

    // Each key puts a different row first, and each is the row the seed says.
    expect(firstBySort.get("oldest")).toBe(seedName(0, SEEDED_FILES));
    expect(firstBySort.get("newest")).toBe(
      seedName(SEEDED_FILES - 1, SEEDED_FILES),
    );
    expect(firstBySort.get("name")).toBe(
      firstBy(SEEDED_FILES, (index) => seedOrdinal(index, SEEDED_FILES)),
    );
    expect(firstBySort.get("size")).toBe(
      firstBy(SEEDED_FILES, (index) => -seedSize(index, SEEDED_FILES)),
    );
    for (const key of ["oldest", "name", "size"] as const) {
      expect(
        firstBySort.get(key),
        `${key} must not fall back to arrival order`,
      ).not.toEqual(firstBySort.get("newest"));
    }

    await sortControl.selectOption("name");
    const named = await listedNames(page);
    expect(named.length).toBeGreaterThan(1);
    expect([...named].sort()).toEqual(named);

    for (let run = 0; run < 3; run += 1) {
      await page.getByRole("button", { name: /^All/ }).click();
      await expect(page.getByRole("button", { name: /^All/ })).toHaveAttribute(
        "aria-pressed",
        "true",
      );
      timings.push({
        what: "filter:documents",
        run,
        elapsed: await measureListUpdate(page, {
          kind: "facet",
          label: "Documents",
        }),
      });
      await expect(
        page.getByRole("button", { name: /^Documents/ }),
      ).toHaveAttribute("aria-pressed", "true");
    }

    for (const name of await listedNames(page)) {
      expect(name).toMatch(/\.(md|pdf|csv)$/);
    }

    for (const { what, run, elapsed } of timings) {
      expect(
        elapsed,
        `${what} run ${run + 1} took ${elapsed}ms over ${SEEDED_FILES} files`,
      ).toBeLessThan(FACET_BUDGET_MS);
    }
  });

  test("the pinned canvas row opens the Canvas surface and an edit saves", async ({
    page,
  }) => {
    await installMockBridge(page, { canvasContent: CANVAS_BODY });
    await page.goto("/");
    await page.getByTestId(`channel-${CHANNEL_NAME}`).click();
    await expect(page.getByTestId("chat-title")).toHaveText(CHANNEL_NAME);
    await page.getByRole("tab", { name: "Files" }).click();

    const canvasRow = page.getByTestId("channel-files-canvas-row");
    await expect(canvasRow).toBeVisible();
    await expect(canvasRow).toContainText("Kickoff");
    await canvasRow.click();

    // The channel's own Canvas surface, never the attachment viewer.
    await expect(
      page.getByTestId("channel-files-canvas-surface"),
    ).toBeVisible();
    await expect(page.getByTestId("channel-canvas-content")).toContainText(
      "The plan lives here.",
    );

    await page.getByTestId("channel-canvas-edit").click();
    const editor = page.getByTestId("channel-canvas-editor");
    await expect(editor).toBeVisible();
    await editor.fill("# Kickoff\n\nEdited from the Files tab.");
    await page.getByTestId("channel-canvas-save").click();
    await expect(page.getByTestId("channel-canvas-content")).toContainText(
      "Edited from the Files tab.",
    );
  });

  test("a channel with no canvas pins no canvas row", async ({ page }) => {
    await installMockBridge(page);
    await page.goto("/");
    await page.getByTestId(`channel-${CHANNEL_NAME}`).click();
    await expect(page.getByTestId("chat-title")).toHaveText(CHANNEL_NAME);
    await page.getByRole("tab", { name: "Files" }).click();
    await expect(
      page.getByRole("combobox", { name: "Sort files" }),
    ).toBeVisible();
    await expect(page.getByTestId("channel-files-canvas-row")).toHaveCount(0);
  });
});
