import type {
  McpRegistryEntry,
  McpRegistryInput,
} from "@/shared/api/tauriMcpRegistry";
import type { AcpRuntimeCatalogEntry, McpTransport } from "@/shared/api/types";

/**
 * Caps mirrored from the Rust loader
 * (`desktop/src-tauri/src/managed_agents/mcp_registry/schema.rs`).
 *
 * These are not a rival source of truth — the backend refuses anything past
 * them and its message is what the panel shows. They exist so the form can
 * stop a save the backend would reject, at the field that caused it, instead
 * of after a round trip. Every one is pinned to the consumer's own constant on
 * the Rust side by `mcp_registry_argument_bounds_match_the_consumer`.
 */
export const MCP_REGISTRY_LIMITS = {
  /** `MAX_ID_LEN`. */
  idLength: 64,
  /** `MAX_NAME_LEN`, itself the stricter of the desktop's and buzz-acp's. */
  nameLength: 32,
  /** `MAX_ARGS`. */
  args: 64,
  /** `MAX_ARG_LEN`, which also caps the command. */
  argLength: 1024,
  /** `MAX_ENV_ENTRIES`. */
  envEntries: 32,
  /** `MAX_ENV_NAME_LEN`. */
  envNameLength: 128,
  /** `MAX_ENV_VALUE_LEN`, derived so `NAME=VALUE` fits one argument. */
  envValueLength: 1024 - 128 - 1,
  /** `MAX_DOCUMENT_SERVERS`. */
  documentServers: 256,
  /** `MAX_SERVERS_PER_AGENT`, inherited from buzz-acp. */
  serversPerAgent: 16,
} as const;

/** A draft as the form holds it, before it becomes a registry entry. */
export type McpServerDraft = {
  id: string;
  name: string;
  transport: "stdio" | "http";
  command: string;
  /** One argument per line, as typed. */
  argsText: string;
  url: string;
  authScheme: string;
  /** Reference id (the part after the `mcp:` prefix) for the credential. */
  authSecretName: string;
  /** Declared variables, as typed. Values are reference ids, never secrets. */
  env: { name: string; reference: string }[];
  /**
   * Values the operator typed, keyed by reference id. Held only until the save
   * that consumes them; no command ever reads one back.
   */
  secrets: Record<string, string>;
};

/** An empty draft. */
export function emptyDraft(): McpServerDraft {
  return {
    id: "",
    name: "",
    transport: "stdio",
    command: "",
    argsText: "",
    url: "",
    authScheme: "bearer",
    authSecretName: "",
    env: [],
    secrets: {},
  };
}

/** Split the argument textarea the way the save does. */
export function draftArgs(draft: McpServerDraft): string[] {
  return draft.argsText
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line.length > 0);
}

const NUL = String.fromCharCode(0);

/**
 * Why this draft cannot be saved, or `null` when it can.
 *
 * Every rule here also exists in the Rust loader, which is the authority: this
 * one names the offending field before a round trip, and is deliberately no
 * stricter, so a draft the panel accepts is one the backend accepts.
 */
export function draftProblem(draft: McpServerDraft): string | null {
  const idProblem = identifierProblem(
    "id",
    draft.id,
    MCP_REGISTRY_LIMITS.idLength,
    /^[a-z0-9_-]+$/,
    "lowercase letters, digits, underscore and hyphen",
  );
  if (idProblem) return idProblem;

  const nameProblem = identifierProblem(
    "name",
    draft.name,
    MCP_REGISTRY_LIMITS.nameLength,
    /^[a-z0-9-]+$/,
    "lowercase letters, digits and hyphen",
  );
  if (nameProblem) return nameProblem;
  if (draft.name.startsWith("buzz-")) {
    return "The name may not use the reserved buzz- prefix.";
  }

  if (draft.transport === "stdio") {
    const stdioProblem = stdioDraftProblem(draft);
    if (stdioProblem) return stdioProblem;
  } else {
    const httpProblem = httpDraftProblem(draft);
    if (httpProblem) return httpProblem;
  }

  return envDraftProblem(draft);
}

function stdioDraftProblem(draft: McpServerDraft): string | null {
  const absolute =
    draft.command.startsWith("/") || /^[A-Za-z]:[/\\]/.test(draft.command);
  if (!absolute) {
    return "The command must be an absolute path: the launcher clears PATH, so a bare name would resolve through an environment nobody controls.";
  }
  if (draft.command.length > MCP_REGISTRY_LIMITS.argLength) {
    return `The command is ${draft.command.length} bytes, over the ${MCP_REGISTRY_LIMITS.argLength}-byte cap.`;
  }
  const args = draftArgs(draft);
  if (args.length > MCP_REGISTRY_LIMITS.args) {
    return `That is ${args.length} arguments, over the ${MCP_REGISTRY_LIMITS.args} cap.`;
  }
  const long = args.find((arg) => arg.length > MCP_REGISTRY_LIMITS.argLength);
  if (long !== undefined) {
    return `One argument is ${long.length} bytes, over the ${MCP_REGISTRY_LIMITS.argLength}-byte cap.`;
  }
  if (draft.command.includes(NUL) || args.some((arg) => arg.includes(NUL))) {
    return "A command line cannot carry a NUL byte.";
  }
  return null;
}

function httpDraftProblem(draft: McpServerDraft): string | null {
  if (!/^https:\/\//.test(draft.url) && !isLoopbackHttp(draft.url)) {
    return "The URL must use https, except on a loopback host.";
  }
  if (/^[a-z]+:\/\/[^/@]*@/.test(draft.url)) {
    return "The URL carries userinfo, which is itself a credential. Name a secret reference instead.";
  }
  return null;
}

function envDraftProblem(draft: McpServerDraft): string | null {
  if (draft.env.length > MCP_REGISTRY_LIMITS.envEntries) {
    return `That is ${draft.env.length} variables, over the ${MCP_REGISTRY_LIMITS.envEntries} cap.`;
  }
  for (const entry of draft.env) {
    if (entry.name.length === 0) return "A variable has no name.";
    if (entry.name.length > MCP_REGISTRY_LIMITS.envNameLength) {
      return `The variable name ${entry.name.slice(0, 24)} is over the ${MCP_REGISTRY_LIMITS.envNameLength}-byte cap.`;
    }
    if (entry.name.includes("=") || entry.name.includes(NUL)) {
      return `${entry.name} holds a NUL or an equals sign, which no NAME=VALUE argument can carry.`;
    }
    if (!/^[a-z0-9_-]+$/.test(entry.reference)) {
      return `${entry.name} does not name a usable secret; a reference id may only use lowercase letters, digits, underscore and hyphen.`;
    }
  }
  for (const [reference, value] of Object.entries(draft.secrets)) {
    if (value.length > MCP_REGISTRY_LIMITS.envValueLength) {
      return `The value for ${reference} is ${value.length} bytes, over the ${MCP_REGISTRY_LIMITS.envValueLength}-byte cap.`;
    }
  }
  return null;
}

function identifierProblem(
  field: string,
  value: string,
  cap: number,
  charset: RegExp,
  described: string,
): string | null {
  if (value.length === 0) return `The ${field} is empty.`;
  if (value.length > cap) {
    return `The ${field} is ${value.length} bytes, over the ${cap}-byte cap.`;
  }
  if (!charset.test(value)) {
    return `The ${field} may only use ${described}.`;
  }
  return null;
}

function isLoopbackHttp(url: string): boolean {
  return /^http:\/\/(127\.0\.0\.1|\[::1\]|localhost)(:\d+)?(\/|$)/.test(url);
}

/** What the approve step puts in front of the operator, verbatim. */
export type McpApprovalSummary = {
  headline: string;
  /** The exact command line, or the exact URL. */
  target: string;
  /** Variable name to reference name. Never a value. */
  references: string[];
  /** Reference names whose value this save will store for the first time. */
  newSecrets: string[];
};

/**
 * What the operator is asked to approve, verbatim.
 *
 * The approve step is the point of the panel: a registry entry starts a
 * process on this machine, or reaches a remote endpoint with a credential
 * attached. So the exact command line, or the exact URL, is shown before the
 * entry is written, together with the *names* of every variable and the
 * reference each one resolves.
 *
 * A secret value is never part of this, and no argument shape can put one
 * here: this function reads `draft.env` and the *keys* of `draft.secrets`, and
 * never a value.
 */
export function approvalSummary(draft: McpServerDraft): McpApprovalSummary {
  const references = draft.env.map(
    (entry) => `${entry.name} = mcp:${entry.reference}`,
  );
  if (draft.transport === "http" && draft.authSecretName.length > 0) {
    references.push(
      `Authorization: ${draft.authScheme} mcp:${draft.authSecretName}`,
    );
  }
  return {
    headline:
      draft.transport === "stdio"
        ? "This starts a process on this machine:"
        : "This sends requests, with the credential attached, to:",
    target:
      draft.transport === "stdio"
        ? [draft.command, ...draftArgs(draft)].join(" ")
        : draft.url,
    references,
    newSecrets: Object.keys(draft.secrets).sort(),
  };
}

/** Turn a draft into the entry the backend deserializes. */
export function draftToInput(draft: McpServerDraft): McpRegistryInput {
  const env: Record<string, string> = {};
  for (const entry of draft.env) {
    env[entry.name] = `mcp:${entry.reference}`;
  }
  if (draft.transport === "stdio") {
    return {
      id: draft.id,
      name: draft.name,
      transport: "stdio",
      command: draft.command,
      args: draftArgs(draft),
      env,
    };
  }
  return {
    id: draft.id,
    name: draft.name,
    transport: "http",
    url: draft.url,
    ...(draft.authSecretName.length > 0
      ? {
          auth: {
            scheme: draft.authScheme,
            secret: `mcp:${draft.authSecretName}`,
          },
        }
      : {}),
    env,
  };
}

/** Rebuild a draft from a stored entry, with no secret value in it. */
export function entryToDraft(entry: McpRegistryEntry): McpServerDraft {
  return {
    id: entry.id,
    name: entry.name,
    transport: entry.transport,
    command: entry.command ?? "",
    argsText: entry.args.join("\n"),
    url: entry.url ?? "",
    authScheme: entry.auth_scheme ?? "bearer",
    authSecretName: "",
    env: entry.env.map((variable) => ({
      name: variable.name,
      reference: (variable.reference ?? "").replace(/^mcp:/, ""),
    })),
    // Deliberately empty. A stored value is never returned by any command, so
    // an edit that does not retype one leaves the stored value untouched.
    secrets: {},
  };
}

/** Why one entry cannot be offered to one runtime, or that it can. */
export type McpServerSupport =
  | { kind: "supported" }
  | { kind: "rejected"; reason: string }
  | { kind: "unsupported"; reason: string }
  | { kind: "runtime-unavailable"; reason: string };

/**
 * Whether `runtime` may be offered `entry`.
 *
 * The transport question is answered from the runtime catalog's
 * `mcpTransports` and from nowhere else, per
 * `desktop/src/features/agents/AGENTS.md`: no component compares a runtime id.
 * An entry the runtime cannot take is reported `unsupported` and the toggle is
 * refused — never quietly left off, because an agent short a server it was
 * told to have is a behaviour change the operator cannot see.
 */
export function serverSupport(
  entry: McpRegistryEntry,
  runtime: Pick<
    AcpRuntimeCatalogEntry,
    "id" | "label" | "mcpTransports"
  > | null,
): McpServerSupport {
  if (entry.rejection !== null) {
    return { kind: "rejected", reason: entry.rejection };
  }
  if (runtime === null) {
    return {
      kind: "runtime-unavailable",
      reason:
        "This agent's harness is not one the registry can configure, so it is offered no registry servers.",
    };
  }
  const needed: McpTransport = entry.transport === "http" ? "http" : "stdio";
  if (!runtime.mcpTransports.includes(needed)) {
    return {
      kind: "unsupported",
      reason: `${entry.name} is an ${entry.transport} server, which the ${runtime.id} runtime cannot use.`,
    };
  }
  return { kind: "supported" };
}

/**
 * The next selection after toggling `id`, or a refusal.
 *
 * A refusal is returned rather than silently ignored: the operator clicked
 * something, and a click that does nothing and says nothing is the
 * silently-dropped entry this design exists to prevent.
 */
export function toggleServer(
  enabled: readonly string[],
  id: string,
  on: boolean,
  support: McpServerSupport,
): { enabled: string[] } | { refused: string } {
  if (!on) {
    return { enabled: enabled.filter((each) => each !== id) };
  }
  if (support.kind !== "supported") {
    return { refused: support.reason };
  }
  if (enabled.includes(id)) {
    return { enabled: [...enabled] };
  }
  if (enabled.length >= MCP_REGISTRY_LIMITS.serversPerAgent) {
    return {
      refused: `An agent may enable at most ${MCP_REGISTRY_LIMITS.serversPerAgent} mcp servers.`,
    };
  }
  return { enabled: [...enabled, id] };
}
