import { expect, test, type Page } from "@playwright/test";

import { installMockBridge, TEST_IDENTITIES } from "../helpers/bridge";
import healthFixture from "./fixtures/agent-health.json" with { type: "json" };

const PAUSED_AGENT_PUBKEY = TEST_IDENTITIES.alice.pubkey;
const PARKED_AGENT_PUBKEY = TEST_IDENTITIES.bob.pubkey;
const BATCH_ID = "b1000000-0000-0000-0000-000000000001";

type ControlRequest = {
  agentPubkey: string;
  payload: {
    type: string;
    [key: string]: unknown;
  };
};

async function readControlRequests(page: Page): Promise<ControlRequest[]> {
  return page.evaluate(
    () => (window.__BUZZ_E2E_OBSERVER_CONTROLS__ ?? []) as ControlRequest[],
  );
}

test.describe("agent health dashboard and controls", () => {
  test("shows health status and dispatches retry observer control", async ({
    page,
  }) => {
    await installMockBridge(page, {
      managedAgents: [
        {
          name: "Alice Agent",
          pubkey: PAUSED_AGENT_PUBKEY,
          status: "online",
        },
        {
          name: "Bob Agent",
          pubkey: PARKED_AGENT_PUBKEY,
          status: "online",
        },
      ],
      agentHealth: healthFixture,
    });

    await page.goto("/", { waitUntil: "domcontentloaded" });

    // Open Agents view
    const openAgentsButton = page.getByTestId("open-agents-view");
    await expect(openAgentsButton).toBeVisible({ timeout: 10_000 });
    await openAgentsButton.click();

    // Click Health tab
    const healthTabTrigger = page.getByTestId("health-tab-trigger");
    await expect(healthTabTrigger).toBeVisible();
    await healthTabTrigger.click();
    await expect(page.getByTestId("agent-health-tab")).toBeVisible();

    // Expect paused row text and needs-review count
    const pausedRow = page.getByTestId(
      `agent-health-row-${PAUSED_AGENT_PUBKEY}`,
    );
    await expect(pausedRow).toBeVisible();
    await expect(pausedRow).toContainText("Paused until");

    const parkedRow = page.getByTestId(
      `agent-health-row-${PARKED_AGENT_PUBKEY}`,
    );
    await expect(parkedRow).toBeVisible();
    await expect(parkedRow).toContainText("Needs Review (1)");

    // Open drawer
    await page
      .getByTestId(`agent-health-row-button-${PARKED_AGENT_PUBKEY}`)
      .click();
    const drawer = page.getByTestId("agent-health-drawer");
    await expect(drawer).toBeVisible();

    // Click Retry
    const retryButton = page.getByTestId(`agent-health-retry-${BATCH_ID}`);
    await expect(retryButton).toBeVisible();
    await retryButton.click();

    // Poll controls for {type:"replay_batch", batchId}
    await expect
      .poll(async () => {
        const controls = await readControlRequests(page);
        return controls.map((c) => c.payload);
      })
      .toContainEqual(
        expect.objectContaining({
          type: "replay_batch",
          batchId: BATCH_ID,
        }),
      );

    // Assert sidebar-agents-health-badge visible
    await expect(page.getByTestId("sidebar-agents-health-badge")).toBeVisible();
  });
});
