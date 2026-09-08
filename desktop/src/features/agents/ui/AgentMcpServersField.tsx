import * as React from "react";
import { useQueryClient } from "@tanstack/react-query";

import {
  MCP_REGISTRY_QUERY_KEY,
  useMcpRegistryQuery,
} from "@/features/settings/ui/McpServersSettingsPanel";
import {
  serverSupport,
  toggleServer,
  type McpServerSupport,
} from "@/features/settings/ui/mcpRegistryLogic";
import { setAgentMcpServers } from "@/shared/api/tauriMcpRegistry";
import type { AcpRuntimeCatalogEntry } from "@/shared/api/types";
import { Switch } from "@/shared/ui/switch";
import { cn } from "@/shared/lib/cn";

/** Copy for the badge one entry gets on this runtime. */
export function supportBadge(
  support: McpServerSupport,
): { label: string; tone: "ok" | "warn" } | null {
  switch (support.kind) {
    case "supported":
      return null;
    case "rejected":
      return { label: "Disabled", tone: "warn" };
    case "unsupported":
      return { label: "Unsupported", tone: "warn" };
    case "runtime-unavailable":
      return { label: "Not configurable", tone: "warn" };
  }
}

export type AgentMcpSelectionState =
  | { status: "loading" }
  | { status: "loaded"; enabled: string[] }
  | { status: "error"; error: string };

export function isSelectionSwitchDisabled(
  selection: AgentMcpSelectionState,
): boolean {
  return selection.status !== "loaded";
}

/**
 * The per-agent registry toggles in the agent definition dialog.
 *
 * Selection state is a union over `{ status: "loading" }`, `{ status: "loaded", enabled }`,
 * and `{ status: "error", error }`. Switches are disabled unless `status === "loaded"`,
 * preventing mutations against uninitialized or failed state.
 *
 * Whether a server may be offered at all is read from the runtime catalog's
 * `mcpTransports`, projected through `AcpRuntimeCatalogEntry` — never from a
 * runtime-id comparison in this component (`features/agents/AGENTS.md`).
 */
export function AgentMcpServersField({
  selection: selectionProp,
  enabled,
  onEnabledChange,
  pubkey,
  runtime,
}: {
  selection?: AgentMcpSelectionState;
  enabled?: readonly string[] | null;
  onEnabledChange: (enabled: string[]) => void;
  /** `null` for a definition that has not been instantiated yet. */
  pubkey: string | null;
  runtime: AcpRuntimeCatalogEntry | null;
}) {
  const registry = useMcpRegistryQuery();
  const queryClient = useQueryClient();
  const [refusal, setRefusal] = React.useState<string | null>(null);

  const selectionState: AgentMcpSelectionState =
    selectionProp ??
    (enabled !== undefined
      ? enabled === null
        ? { status: "loading" }
        : { status: "loaded", enabled: [...enabled] }
      : { status: "loading" });

  const isLoaded = selectionState.status === "loaded";
  const selection = isLoaded ? selectionState.enabled : [];
  const servers = registry.data?.servers ?? [];

  if (registry.isError) {
    return (
      <p className="text-sm text-amber-600" role="alert">
        {String(registry.error)}
      </p>
    );
  }
  if (servers.length === 0) {
    return (
      <p
        className="text-sm text-muted-foreground/70"
        data-testid="agent-mcp-servers-empty"
      >
        No MCP servers are registered. Add one in Settings, Agents, MCP servers.
      </p>
    );
  }

  const apply = async (next: string[]) => {
    onEnabledChange(next);
    if (pubkey === null) return;
    try {
      const view = await setAgentMcpServers(pubkey, next);
      // A successful write can still convergence-refuse THIS agent (its
      // selection could not be resolved into the adopted generation). Inspect
      // the response for that before clearing the alert — an `Ok` response is
      // not the same as a refusal-free convergence, and the post-invalidate
      // refetch below always reports an empty `refused` (list_mcp_registry_servers
      // never recomputes it), so it cannot be relied on to surface this.
      const ownRefusal = view.refused.find(([agentId]) => agentId === pubkey);
      setRefusal(ownRefusal ? ownRefusal[1] : null);
      void queryClient.invalidateQueries({ queryKey: MCP_REGISTRY_QUERY_KEY });
    } catch (error) {
      // Surfaced, not swallowed: the write failed, so the agent will spawn
      // with the previous selection and the operator has to know that.
      setRefusal(String(error));
    }
  };

  return (
    <div className="space-y-2" data-testid="agent-mcp-servers">
      {selectionState.status === "error" ? (
        <p
          className="text-sm text-amber-600"
          data-testid="agent-mcp-servers-error"
          role="alert"
        >
          {selectionState.error}
        </p>
      ) : null}
      {servers.map((entry) => {
        const support = serverSupport(entry, runtime);
        const badge = supportBadge(support);
        const checked = selection.includes(entry.id);
        const inputId = `agent-mcp-server-${entry.id}`;
        return (
          <div
            className="flex items-start justify-between gap-3"
            data-testid={`agent-mcp-server-${entry.id}`}
            key={entry.id}
          >
            <div className="min-w-0">
              <label
                className="flex flex-wrap items-center gap-2 text-sm font-medium"
                htmlFor={inputId}
              >
                {entry.name}
                {badge !== null ? (
                  <span
                    className={cn(
                      "inline-flex shrink-0 items-center rounded-md px-2 py-0.5 text-xs font-medium",
                      badge.tone === "warn"
                        ? "bg-amber-500/15 text-amber-600 dark:text-amber-400"
                        : "bg-emerald-500/15 text-emerald-600 dark:text-emerald-400",
                    )}
                    data-testid={`agent-mcp-server-badge-${entry.id}`}
                  >
                    {badge.label}
                  </span>
                ) : null}
              </label>
              <p className="truncate font-mono text-xs text-muted-foreground/70">
                {entry.transport === "stdio"
                  ? [entry.command, ...entry.args].join(" ")
                  : entry.url}
              </p>
              {support.kind !== "supported" ? (
                <p
                  className="mt-0.5 text-xs text-amber-600"
                  data-testid={`agent-mcp-server-reason-${entry.id}`}
                >
                  {support.reason}
                </p>
              ) : null}
            </div>
            <Switch
              aria-label={`Enable ${entry.name}`}
              checked={checked}
              disabled={!isLoaded}
              id={inputId}
              onCheckedChange={(next) => {
                if (!isLoaded) return;
                const result = toggleServer(selection, entry.id, next, support);
                if ("refused" in result) {
                  setRefusal(result.refused);
                  return;
                }
                setRefusal(null);
                void apply(result.enabled);
              }}
            />
          </div>
        );
      })}
      {refusal !== null ? (
        <p
          className="text-sm text-amber-600"
          data-testid="agent-mcp-servers-refusal"
          role="alert"
        >
          {refusal}
        </p>
      ) : null}
    </div>
  );
}
