import * as React from "react";
import { RefreshCw } from "lucide-react";

import { useAgentHealthSummaryQuery } from "@/features/agents/agentHealthHooks";
import {
  type AgentHealthState,
  buildHealthRows,
} from "@/features/agents/agentHealthSummary";
import { useManagedAgentsQuery } from "@/features/agents/hooks";
import { useManagedAgentRuntimesQuery } from "@/features/agents/managedAgentRuntimeHooks";
import { cn } from "@/shared/lib/cn";
import { normalizePubkey, truncatePubkey } from "@/shared/lib/pubkey";
import { Badge } from "@/shared/ui/badge";
import { Button } from "@/shared/ui/button";
import { AgentHealthDrawer } from "./AgentHealthDrawer";

function formatTimestamp(ts: number | string | null | undefined): string {
  if (!ts) return "-";
  const ms =
    typeof ts === "number"
      ? ts > 1e11
        ? ts
        : ts * 1000
      : new Date(ts).getTime();
  if (Number.isNaN(ms)) return String(ts);
  return new Date(ms).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function renderStateBadge(state: AgentHealthState, pausedUntil: string | null) {
  switch (state) {
    case "active":
      return <Badge variant="success">Active</Badge>;
    case "paused":
      return (
        <Badge variant="warning">
          Paused{pausedUntil ? ` until ${formatTimestamp(pausedUntil)}` : ""}
        </Badge>
      );
    case "breaker":
      return <Badge variant="destructive">Breaker Open</Badge>;
    case "offline":
    default:
      return <Badge variant="secondary">Offline</Badge>;
  }
}

export function AgentHealthTab() {
  const summaryQuery = useAgentHealthSummaryQuery();
  const managedAgentsQuery = useManagedAgentsQuery();
  const runtimesQuery = useManagedAgentRuntimesQuery();

  const [selectedPubkey, setSelectedPubkey] = React.useState<string | null>(
    null,
  );

  const managedPubkeys = React.useMemo(() => {
    return new Set(
      (managedAgentsQuery.data ?? []).map((agent) =>
        normalizePubkey(agent.pubkey),
      ),
    );
  }, [managedAgentsQuery.data]);

  const isRemoteOwned = React.useCallback(
    (pubkey: string) => !managedPubkeys.has(normalizePubkey(pubkey)),
    [managedPubkeys],
  );

  const rows = React.useMemo(() => {
    return buildHealthRows(
      summaryQuery.data,
      managedAgentsQuery.data,
      runtimesQuery.data,
    );
  }, [summaryQuery.data, managedAgentsQuery.data, runtimesQuery.data]);

  const selectedAgent = React.useMemo(() => {
    if (!selectedPubkey) return null;
    return rows.find((r) => r.pubkey === selectedPubkey) ?? null;
  }, [rows, selectedPubkey]);

  return (
    <div className="space-y-4" data-testid="agent-health-tab">
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-base font-semibold text-foreground">
            Agent Health
          </h2>
          <p className="text-xs text-muted-foreground">
            Operational status, turn counts, and failure rates across your
            agents.
          </p>
        </div>
        <Button
          aria-label="Refresh agent health"
          data-testid="agent-health-refresh"
          disabled={summaryQuery.isFetching}
          onClick={() => void summaryQuery.refetch()}
          size="sm"
          variant="outline"
        >
          <RefreshCw
            className={cn("h-4 w-4", summaryQuery.isFetching && "animate-spin")}
          />
          Refresh
        </Button>
      </div>

      {summaryQuery.data?.syncError ? (
        <div
          className="rounded-md border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-xs text-amber-500"
          data-testid="agent-health-stale-banner"
        >
          Health sync failed; metrics may be stale.
        </div>
      ) : null}

      {summaryQuery.isError ? (
        <div className="flex flex-col items-center justify-center gap-2 rounded-lg border border-border/70 p-12 text-center text-muted-foreground">
          <p className="text-sm text-destructive">
            Failed to load agent health data.
          </p>
          <Button
            aria-label="Retry loading agent health"
            onClick={() => void summaryQuery.refetch()}
            size="sm"
            variant="outline"
          >
            Retry
          </Button>
        </div>
      ) : rows.length === 0 ? (
        <div
          className="rounded-lg border border-dashed border-border/70 p-12 text-center text-sm text-muted-foreground"
          data-testid="agent-health-empty"
        >
          No agent health activity recorded yet.
        </div>
      ) : (
        <div
          className="overflow-hidden rounded-lg border border-border/70 bg-card"
          data-testid="agent-health-table-container"
        >
          <div className="overflow-x-auto">
            <table
              className="w-full border-collapse text-left text-sm"
              data-testid="agent-health-table"
            >
              <thead>
                <tr className="border-b border-border/70 bg-muted/50 text-xs font-medium text-muted-foreground">
                  <th className="px-4 py-3">Agent</th>
                  <th className="px-4 py-3">State</th>
                  <th className="px-4 py-3">Turns (24h / 7d)</th>
                  <th className="px-4 py-3">Failed (24h / 7d)</th>
                  <th className="px-4 py-3">Parked</th>
                  <th className="px-4 py-3">Last Error</th>
                  <th className="px-4 py-3">Reconnects (24h)</th>
                  <th className="px-4 py-3 text-right">Actions</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-border/50">
                {rows.map((row) => {
                  const isRemote = isRemoteOwned(row.pubkey);
                  return (
                    <tr
                      className="transition-colors hover:bg-muted/30"
                      data-testid={`agent-health-row-${row.pubkey}`}
                      key={row.pubkey}
                    >
                      <td className="px-4 py-3">
                        <div className="flex flex-col">
                          <span className="font-medium text-foreground">
                            {row.name}
                          </span>
                          {isRemote ? (
                            <span className="text-xs text-muted-foreground">
                              frames only, no local ledger
                            </span>
                          ) : (
                            <span className="max-w-[12rem] truncate font-mono text-2xs text-muted-foreground">
                              {truncatePubkey(row.pubkey)}
                            </span>
                          )}
                        </div>
                      </td>
                      <td className="px-4 py-3">
                        {renderStateBadge(row.state, row.pausedUntil)}
                      </td>
                      <td className="px-4 py-3 text-muted-foreground">
                        <span className="font-medium text-foreground">
                          {row.turns24h}
                        </span>{" "}
                        / {row.turns7d}
                      </td>
                      <td className="px-4 py-3">
                        <span
                          className={cn(
                            "font-medium",
                            row.failed24h > 0
                              ? "text-destructive"
                              : "text-foreground",
                          )}
                        >
                          {row.failed24h}
                        </span>{" "}
                        <span className="text-muted-foreground">
                          / {row.failed7d}
                        </span>
                      </td>
                      <td className="px-4 py-3">
                        <div className="flex items-center gap-1.5">
                          <span className="text-foreground">{row.parked}</span>
                          {row.needsReview > 0 ? (
                            <Badge className="ml-1" variant="destructive">
                              Needs Review ({row.needsReview})
                            </Badge>
                          ) : null}
                        </div>
                      </td>
                      <td className="px-4 py-3">
                        {row.lastErrorClass ? (
                          <div className="text-xs">
                            <span className="font-medium text-destructive">
                              {row.lastErrorClass}
                            </span>
                            {row.lastErrorAt ? (
                              <span className="block text-muted-foreground">
                                {formatTimestamp(row.lastErrorAt)}
                              </span>
                            ) : null}
                          </div>
                        ) : (
                          <span className="text-xs text-muted-foreground">
                            -
                          </span>
                        )}
                      </td>
                      <td className="px-4 py-3 text-muted-foreground">
                        {row.reconnects24h}
                      </td>
                      <td className="px-4 py-3 text-right">
                        <Button
                          aria-label={`Open health details for ${row.name}`}
                          data-testid={`agent-health-row-button-${row.pubkey}`}
                          onClick={() => setSelectedPubkey(row.pubkey)}
                          size="sm"
                          variant="outline"
                        >
                          Details
                        </Button>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        </div>
      )}

      <AgentHealthDrawer
        agent={selectedAgent}
        isRemote={selectedAgent ? isRemoteOwned(selectedAgent.pubkey) : false}
        onOpenChange={(open) => {
          if (!open) setSelectedPubkey(null);
        }}
        open={selectedPubkey !== null}
      />
    </div>
  );
}
