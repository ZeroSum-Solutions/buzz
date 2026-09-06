import { invokeTauri } from "@/shared/api/tauri";

/** One declared environment entry, names only — never a secret value. */
export type McpRegistryEnvEntry = {
  name: string;
  /** The `mcp:<id>` reference this entry names, when it names one. */
  reference: string | null;
  /**
   * The literal value, when this entry carries one. Only values the backend's
   * sentinel scan cleared as non-credential ever reach here; a
   * credential-shaped literal rejects the entry at load.
   */
  literal: string | null;
};

/** One registry entry, as the panel renders and approves it. */
export type McpRegistryEntry = {
  id: string;
  name: string;
  transport: "stdio" | "http";
  /** Absolute command path for a stdio entry. */
  command: string | null;
  args: string[];
  /** Upstream URL for an http entry. */
  url: string | null;
  auth_scheme: string | null;
  env: McpRegistryEnvEntry[];
  /**
   * The loader's reason this entry is disabled, or `null` when it is usable.
   * The same string a spawn refuses with, so the panel and the agent agree.
   */
  rejection: string | null;
};

/** What the panel loads. */
export type McpRegistryView = {
  servers: McpRegistryEntry[];
  document_path: string;
};

/** The shape the backend deserializes for a save. */
export type McpRegistryStdioInput = {
  id: string;
  name: string;
  transport: "stdio";
  command: string;
  args: string[];
  env: Record<string, string>;
};

/** The shape the backend deserializes for a save. */
export type McpRegistryHttpInput = {
  id: string;
  name: string;
  transport: "http";
  url: string;
  auth?: { scheme: string; secret: string };
  env: Record<string, string>;
};

export type McpRegistryInput = McpRegistryStdioInput | McpRegistryHttpInput;

/** Read the registry document and each entry's status. */
export async function listMcpRegistryServers(): Promise<McpRegistryView> {
  return invokeTauri<McpRegistryView>("list_mcp_registry_servers");
}

/**
 * Insert or replace one entry and adopt a new configuration generation.
 *
 * `secrets` maps a reference id (the part after `mcp:`) to the value the
 * operator typed. It travels one way — into the keychain, under the reserved
 * `mcp:` prefix — and no command reads one back.
 */
export async function saveMcpRegistryServer(
  entry: McpRegistryInput,
  secrets: Record<string, string> = {},
): Promise<McpRegistryView> {
  return invokeTauri<McpRegistryView>("save_mcp_registry_server", {
    entry,
    secrets,
  });
}

/** Delete one entry, drop its id from every agent, and adopt a generation. */
export async function deleteMcpRegistryServer(
  id: string,
): Promise<McpRegistryView> {
  return invokeTauri<McpRegistryView>("delete_mcp_registry_server", { id });
}

/**
 * Read one agent's selection.
 *
 * `null` means the record has never been configured, which is a different
 * state from an empty list (memo decision 8).
 */
export async function getAgentMcpServers(
  pubkey: string,
): Promise<string[] | null> {
  return invokeTauri<string[] | null>("get_agent_mcp_servers", { pubkey });
}

/** Set one agent's selection and adopt a new configuration generation. */
export async function setAgentMcpServers(
  pubkey: string,
  enabled: string[],
): Promise<McpRegistryView> {
  return invokeTauri<McpRegistryView>("set_agent_mcp_servers", {
    pubkey,
    enabled,
  });
}
