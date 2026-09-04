import { expect, test } from "@playwright/test";

import { waitForAnimations } from "../helpers/animations";
import {
  installMockBridge,
  openNewMessagePage,
  TEST_IDENTITIES,
} from "../helpers/bridge";
import type { MockManagedAgentSeed } from "../../src/testing/e2eBridge";

const SHOTS = "test-results/mention-descriptions";

// Distinct pubkeys — no overlap with bridge fixtures or other specs.
const FIZZ_PUBKEY = "3a".repeat(32);
const HONEY_PUBKEY = "3b".repeat(32);
const BUMBLE_PUBKEY = "3c".repeat(32);
const BUZZY_PUBKEY = "3d".repeat(32);
const ATLAS_PUBKEY = "3e".repeat(32);

const FIZZ_ABOUT = "Builder — implements features and fixes bugs";
const HONEY_ABOUT = "Writer — drafts docs, posts, and summaries";
const BUMBLE_ABOUT = "Researcher — deep dives, sourcing, and citations";
const ATLAS_LONG_ABOUT =
  "Operations copilot for the whole hive: triages incoming requests, " +
  "routes work to the right specialist agent, keeps the runbook current, " +
  "and escalates anything ambiguous to a human before acting on it";

/** Locator scoped to the mention autocomplete dropdown inside the composer. */
function autocomplete(page: import("@playwright/test").Page) {
  return page
    .getByTestId("message-composer")
    .getByTestId("mention-autocomplete");
}

/** Full-page clip spanning the open dropdown down to the composer bottom. */
async function shootComposerWithDropdown(
  page: import("@playwright/test").Page,
  path: string,
) {
  await waitForAnimations(page);
  const dropdownBox = await autocomplete(page).boundingBox();
  const composerBox = await page.getByTestId("message-composer").boundingBox();
  if (!dropdownBox || !composerBox) {
    throw new Error("composer or dropdown not visible for screenshot");
  }
  const top = Math.max(0, dropdownBox.y - 8);
  await page.screenshot({
    path,
    clip: {
      x: Math.max(0, composerBox.x - 8),
      y: top,
      width: composerBox.width + 16,
      height: composerBox.y + composerBox.height + 8 - top,
    },
  });
}

test("mention selector shows each agent's kind-0 about as a role line", async ({
  page,
}) => {
  await installMockBridge(page, {
    managedAgents: [
      {
        pubkey: FIZZ_PUBKEY,
        name: "Fizz",
        status: "stopped",
        channelNames: ["general"],
      },
      {
        pubkey: HONEY_PUBKEY,
        name: "Honey",
        status: "stopped",
        channelNames: ["general"],
      },
      {
        pubkey: BUMBLE_PUBKEY,
        name: "Bumble",
        status: "stopped",
        channelNames: ["general"],
      },
      {
        pubkey: BUZZY_PUBKEY,
        name: "Buzzy",
        status: "stopped",
        channelNames: ["general"],
      },
    ],
    searchProfiles: [
      { pubkey: FIZZ_PUBKEY, displayName: "Fizz", about: FIZZ_ABOUT },
      { pubkey: HONEY_PUBKEY, displayName: "Honey", about: HONEY_ABOUT },
      { pubkey: BUMBLE_PUBKEY, displayName: "Bumble", about: BUMBLE_ABOUT },
      // Buzzy has no `about` — exercises the name-only fallback row.
      // bob is human — an `about` on a person must NOT render a role line.
      {
        pubkey: TEST_IDENTITIES.bob.pubkey,
        displayName: "bob",
        about: "A human bio that stays off the mention row",
      },
    ],
  });
  await page.goto("/");
  await page.getByTestId("channel-general").click();
  await expect(page.getByTestId("chat-title")).toHaveText("general");

  await page.getByTestId("message-input").fill("@");

  const dropdown = autocomplete(page);
  await expect(dropdown).toBeVisible();

  for (const [name, about] of [
    ["Fizz", FIZZ_ABOUT],
    ["Honey", HONEY_ABOUT],
    ["Bumble", BUMBLE_ABOUT],
  ] as const) {
    const row = dropdown.locator("button", { hasText: name });
    await expect(row.getByTestId("mention-agent-icon")).toBeVisible();
    await expect(row.getByTestId("mention-agent-description")).toHaveText(
      about,
    );
    // The row's aria-label ("Mention Fizz") overrides descendant text as the
    // accessible name, so the role line must reach screen readers through
    // aria-describedby instead — assert the accessible description directly
    // rather than only the visible text, or a regression that renders the
    // text but drops the wiring would pass silently.
    await expect(row).toHaveAccessibleDescription(about);
    await expect(row.getByText("managed by you")).toBeVisible();
  }

  // No `about` → today's exact row: bot icon + literal "agent" label.
  const buzzyRow = dropdown.locator("button", { hasText: "Buzzy" });
  await expect(buzzyRow.getByTestId("mention-agent-icon")).toBeVisible();
  await expect(buzzyRow.getByText("agent", { exact: true })).toBeVisible();
  await expect(buzzyRow.getByTestId("mention-agent-description")).toHaveCount(
    0,
  );
  await expect(buzzyRow).toHaveAccessibleDescription("");

  // Humans never get a role line, even with an `about` on their profile.
  const bobRow = dropdown.locator("button", { hasText: "bob" });
  await expect(bobRow).toBeVisible();
  await expect(bobRow.getByTestId("mention-agent-description")).toHaveCount(0);
  await expect(bobRow).toHaveAccessibleDescription("");

  // Scroll the agent rows into frame — the list opens scrolled to the top
  // where the viewer/member rows sit.
  await dropdown.evaluate((el) => {
    el.scrollTop = el.scrollHeight;
  });
  await shootComposerWithDropdown(page, `${SHOTS}/01-agent-role-lines.png`);
});

test("long about truncates to a single line beside the managed-by label", async ({
  page,
}) => {
  await installMockBridge(page, {
    managedAgents: [
      {
        pubkey: ATLAS_PUBKEY,
        name: "Atlas",
        status: "stopped",
        channelNames: ["general"],
      },
    ],
    searchProfiles: [
      { pubkey: ATLAS_PUBKEY, displayName: "Atlas", about: ATLAS_LONG_ABOUT },
    ],
  });
  await page.goto("/");
  await page.getByTestId("channel-general").click();
  await expect(page.getByTestId("chat-title")).toHaveText("general");

  await page.getByTestId("message-input").fill("@Atlas");

  const dropdown = autocomplete(page);
  const atlasRow = dropdown.locator("button", { hasText: "Atlas" });
  const description = atlasRow.getByTestId("mention-agent-description");
  await expect(description).toBeVisible();
  await expect(description).toHaveAttribute("title", ATLAS_LONG_ABOUT);
  await expect(atlasRow).toHaveAccessibleDescription(ATLAS_LONG_ABOUT);
  await expect(atlasRow.getByText("managed by you")).toBeVisible();

  // The full text must overflow its one-line box — proof it truncates
  // instead of wrapping or pushing the managed-by label out of the row.
  const truncates = await description.evaluate(
    (el) => el.scrollWidth > el.clientWidth,
  );
  expect(truncates).toBe(true);

  await shootComposerWithDropdown(page, `${SHOTS}/02-long-about-truncates.png`);
});

// Regression coverage for the agent-profile fallback in useMentions.ts
// (agentProfilePubkeys / agentProfilesQuery / mentionProfiles). The tests
// above drive ChannelScreen, whose `messageProfilePubkeys` already includes
// every known agent, so those agents' `about` arrives through that OUTER
// batch — the fallback never actually fires there. New Message's composer
// (`NewMessageScreen.tsx`) is the fallback's real production seam: it
// passes no `profiles` prop to `MessageComposer` at all, so any agent role
// line here can only come from useMentions' own fallback batch.
//
// The numeric request bound (never more than MENTION_SUGGESTION_LIMIT
// pubkeys, even with 150+ mentionable agents, and a past-the-window agent
// still resolving once ranked into view) is covered directly and cheaply at
// the unit level in mentionFallbackWindow.test.mjs, which exercises the
// exact functions useMentions.ts calls. This spec proves the other half —
// that useMentions.ts is actually wired to call them on a real composer
// with no scoped `profiles` — through observable UI behavior only.
const MANY_AGENTS_COUNT = 120;
// Zero-padded to a fixed width so no agent's name is a substring of
// another's (`Agent001` vs. `Agent010` vs. `Agent100`) — Playwright's
// `hasText` matches substrings of the whole row, and these names are the
// only thing that disambiguates one row from another.
const agentName = (n: number) => `Agent${String(n).padStart(3, "0")}`;
const FIRST_AGENT_NAME = agentName(1);
const LAST_AGENT_NAME = agentName(MANY_AGENTS_COUNT);
const LAST_AGENT_ABOUT = `Role ${MANY_AGENTS_COUNT}`;

function manyAgentPubkey(index: number): string {
  // 64 lowercase-hex chars, unique per index, distinct from every other
  // fixture pubkey in this file and the shared bridge fixtures.
  return `9f${index.toString(16).padStart(6, "0")}`.padEnd(64, "0");
}

const manyManagedAgents: MockManagedAgentSeed[] = Array.from(
  { length: MANY_AGENTS_COUNT },
  (_, i) => ({
    pubkey: manyAgentPubkey(i),
    name: agentName(i + 1),
    status: "stopped",
  }),
);
const manySearchProfiles = manyManagedAgents.map((agent, i) => ({
  pubkey: agent.pubkey,
  displayName: agent.name,
  about: `Role ${i + 1}`,
}));

test("resolves an agent's about through the fallback batch on a composer with no profiles prop, bounded even past the ranked window", async ({
  page,
}) => {
  await installMockBridge(page, {
    managedAgents: manyManagedAgents,
    relayAgents: [],
    searchProfiles: manySearchProfiles,
  });
  await page.goto("/");
  await openNewMessagePage(page);

  // New Message requires a recipient before the composer accepts input.
  await page.getByTestId("new-dm-search").fill("charlie");
  await page
    .getByTestId(`new-dm-result-${TEST_IDENTITIES.charlie.pubkey}`)
    .click();
  await page.getByTestId("new-dm-search").press("Escape");
  await expect(page.getByTestId("new-message-recipient-popover")).toBeHidden();

  // A bare "@" keeps the mention query empty, which keeps global user
  // search disabled (`canSearchGlobalPeople` requires non-empty text) — the
  // fallback batch is the ONLY thing that can resolve an about here.
  await page.getByTestId("message-input").fill("@");
  const dropdown = autocomplete(page);
  await expect(dropdown).toBeVisible();

  // Ranked window caps at MENTION_SUGGESTION_LIMIT (50) even though 120
  // agents are mentionable.
  await expect(dropdown.locator("button")).toHaveCount(50);
  await expect(
    dropdown.locator("button", { hasText: LAST_AGENT_NAME }),
  ).toHaveCount(0);

  // A visible agent's role line resolved through the fallback. A bare "@"
  // keeps global user search disabled (see above), so this text can only
  // have come from useMentions' fallback batch — deleting that block drops
  // this to the generic "agent" label and fails this assertion.
  const firstRow = dropdown.locator("button", { hasText: FIRST_AGENT_NAME });
  await expect(firstRow.getByTestId("mention-agent-description")).toHaveText(
    "Role 1",
  );

  // Narrowing the query to the 120th agent's exact name ranks it first and
  // brings it into the visible window — proof the bound doesn't strand an
  // agent past the first page; it resolves once brought into view.
  await page.getByTestId("message-input").fill(`@${LAST_AGENT_NAME}`);
  const lastRow = dropdown.locator("button", { hasText: LAST_AGENT_NAME });
  await expect(lastRow).toBeVisible();
  await expect(lastRow.getByTestId("mention-agent-description")).toHaveText(
    LAST_AGENT_ABOUT,
  );
});
