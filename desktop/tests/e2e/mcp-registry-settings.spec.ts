import { expect, test } from "@playwright/test";

import { installMockBridge } from "../helpers/bridge";
import { openSettings } from "../helpers/settings";

/**
 * The MCP registry, end to end through the panel.
 *
 * Adds a stdio server from Settings, checks the approve step shows the exact
 * command line and no credential value, toggles it on for one agent, reads
 * back the configuration that agent's next spawn would use, and toggles it off
 * again.
 *
 * What this binds is the UI flow and the shape of what a toggle produces. The
 * byte-level guarantee — that the file the shipped generator writes names the
 * bundled launcher and exactly the selected servers, and that the next
 * generation drops one — is bound on the Rust side by
 * `mcp_registry_a_toggle_change_adopts_a_new_generation` and
 * `mcp_registry_generated_config_names_the_launcher_and_carries_no_value`,
 * which read the real files.
 */

const AGENT_PUBKEY =
  "e5ebc6cdb579be112e336cc319b5989b4bb6af11786ea90dbe52b5f08d741b34";
const SERVER_COMMAND = "/usr/local/bin/fake-mcp";

type GenerationProbe = {
  generation: number;
  artefact: {
    version: number;
    servers: { name: string; command: string; args: string[] }[];
  } | null;
  storedReferences: string[];
};

async function readGeneration(
  page: import("@playwright/test").Page,
): Promise<GenerationProbe> {
  return page.evaluate(async (pubkey) => {
    const internals = (
      window as unknown as {
        __TAURI_INTERNALS__: {
          invoke: (cmd: string, args: unknown) => Promise<unknown>;
        };
      }
    ).__TAURI_INTERNALS__;
    return (await internals.invoke("__buzz_e2e_mcp_generation__", {
      pubkey,
    })) as GenerationProbe;
  }, AGENT_PUBKEY);
}

async function setSelection(
  page: import("@playwright/test").Page,
  enabled: string[],
) {
  await page.evaluate(
    async ({ pubkey, servers }) => {
      const internals = (
        window as unknown as {
          __TAURI_INTERNALS__: {
            invoke: (cmd: string, args: unknown) => Promise<unknown>;
          };
        }
      ).__TAURI_INTERNALS__;
      await internals.invoke("set_agent_mcp_servers", {
        pubkey,
        enabled: servers,
      });
    },
    { pubkey: AGENT_PUBKEY, servers: enabled },
  );
}

test.beforeEach(async ({ page }) => {
  await installMockBridge(page);
  await page.goto("/");
});

test("a registry server is added behind an approve step and reaches one agent's configuration", async ({
  page,
}) => {
  await openSettings(page, "agents");

  const panel = page.getByTestId("settings-mcp-servers");
  await expect(panel).toBeVisible();
  await panel.getByRole("button", { name: "Add server" }).click();

  const form = page.getByTestId("mcp-server-form");
  await expect(form).toBeVisible();
  await form.getByLabel("Id").fill("fake");
  await form.getByLabel("Name", { exact: true }).fill("fake");
  await form.getByLabel("Command (absolute path)").fill(SERVER_COMMAND);
  await form.getByLabel("Arguments, one per line").fill("--stdio");

  // The approve step must show what will actually be spawned, verbatim.
  await form.getByRole("button", { name: "Review" }).click();
  const approve = page.getByTestId("mcp-server-approve");
  await expect(approve).toBeVisible();
  await expect(page.getByTestId("mcp-server-approve-target")).toHaveText(
    `${SERVER_COMMAND} --stdio`,
  );

  await approve.getByRole("button", { name: "Approve and save" }).click();
  await expect(page.getByTestId("mcp-server-row-fake")).toBeVisible();
  await expect(page.getByTestId("mcp-server-row-fake")).toContainText(
    SERVER_COMMAND,
  );

  // Nothing is staged for the agent until it is toggled on.
  const before = await readGeneration(page);
  expect(before.artefact).toBeNull();

  await setSelection(page, ["fake"]);
  const enabled = await readGeneration(page);
  expect(enabled.generation).toBeGreaterThan(before.generation);
  expect(enabled.artefact).not.toBeNull();
  const server = enabled.artefact?.servers[0];
  expect(server?.name).toBe("fake");
  expect(server?.command).toContain("buzz-mcp-launch");
  expect(server?.args).toContain(SERVER_COMMAND);
  expect(server?.args).toContain("--stdio");

  // And the next generation drops it.
  await setSelection(page, []);
  const dropped = await readGeneration(page);
  expect(dropped.generation).toBeGreaterThan(enabled.generation);
  expect(dropped.artefact).toBeNull();
});

test("a credential is entered once and never rendered back", async ({
  page,
}) => {
  await openSettings(page, "agents");

  const panel = page.getByTestId("settings-mcp-servers");
  await panel.getByRole("button", { name: "Add server" }).click();
  const form = page.getByTestId("mcp-server-form");
  await form.getByRole("radio", { name: "HTTP endpoint" }).click();
  await form.getByLabel("Id").fill("remote");
  await form.getByLabel("Name", { exact: true }).fill("remote");
  await form.getByLabel("Upstream URL").fill("https://mcp.example/v1");
  await form.getByLabel("Credential name").fill("remote-token");
  await page.getByTestId("mcp-server-secret-value").fill("sk-live-do-not-show");

  await form.getByRole("button", { name: "Review" }).click();
  const approve = page.getByTestId("mcp-server-approve");
  await expect(approve).toBeVisible();
  await expect(approve).toContainText("mcp:remote-token");
  await expect(approve).not.toContainText("sk-live-do-not-show");

  await approve.getByRole("button", { name: "Approve and save" }).click();
  await expect(page.getByTestId("mcp-server-row-remote")).toBeVisible();

  // The document the panel renders back holds the reference, never the value.
  const rendered = await page.getByTestId("settings-mcp-servers").innerText();
  expect(rendered).not.toContain("sk-live-do-not-show");

  const probe = await readGeneration(page);
  expect(probe.storedReferences).toContain("remote-token");

  // Re-opening the entry for edit brings back no value either.
  await page
    .getByTestId("mcp-server-row-remote")
    .getByRole("button", {
      name: "Edit",
    })
    .click();
  await expect(page.getByTestId("mcp-server-form")).toBeVisible();
  await expect(page.getByTestId("mcp-server-form")).not.toContainText(
    "sk-live-do-not-show",
  );
});
