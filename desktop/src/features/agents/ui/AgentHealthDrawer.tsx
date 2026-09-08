import * as React from "react";
import { useQueryClient } from "@tanstack/react-query";

import { useAppNavigation } from "@/app/navigation/useAppNavigation";
import {
  agentHealthEventsQueryKey,
  agentHealthSummaryQueryKey,
  parkedBatchesQueryKey,
  useAgentHealthEventsQuery,
  useParkedBatchesQuery,
} from "@/features/agents/agentHealthHooks";
import type { AgentHealthRow } from "@/features/agents/agentHealthSummary";
import {
  discardParkedBatch,
  keepAgentPaused,
  replayParkedBatch,
  resumeAgentNow,
} from "@/shared/api/agentHealthControl";
import { Badge } from "@/shared/ui/badge";
import { Button } from "@/shared/ui/button";
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
} from "@/shared/ui/sheet";

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
    second: "2-digit",
  });
}

export interface AgentHealthDrawerProps {
  agent: AgentHealthRow | null;
  isRemote?: boolean;
  onOpenChange: (open: boolean) => void;
  open: boolean;
}

export function AgentHealthDrawer({
  agent,
  isRemote = false,
  onOpenChange,
  open,
}: AgentHealthDrawerProps) {
  const queryClient = useQueryClient();
  const { goChannel } = useAppNavigation();
  const [actionPendingId, setActionPendingId] = React.useState<string | null>(
    null,
  );
  const [actionError, setActionError] = React.useState<string | null>(null);

  const pubkey = agent?.pubkey;
  const agentName = agent?.name ?? pubkey ?? "Agent";

  const parkedBatchesQuery = useParkedBatchesQuery(pubkey, {
    enabled: open && Boolean(pubkey) && !isRemote,
  });

  const eventsQuery = useAgentHealthEventsQuery(pubkey, {
    limit: 50,
    enabled: open && Boolean(pubkey),
  });

  const parkedBatches = parkedBatchesQuery.data ?? [];
  const events = eventsQuery.data ?? [];

  const handleRetry = React.useCallback(
    async (batchId: string) => {
      if (!pubkey) return;
      setActionPendingId(`retry-${batchId}`);
      setActionError(null);
      try {
        await replayParkedBatch(pubkey, batchId);
        await Promise.all([
          queryClient.invalidateQueries({
            queryKey: parkedBatchesQueryKey(pubkey),
          }),
          queryClient.invalidateQueries({
            queryKey: agentHealthSummaryQueryKey,
          }),
          queryClient.invalidateQueries({
            queryKey: agentHealthEventsQueryKey(pubkey),
          }),
        ]);
      } catch (err) {
        setActionError(err instanceof Error ? err.message : String(err));
      } finally {
        setActionPendingId(null);
      }
    },
    [pubkey, queryClient],
  );

  const handleDiscard = React.useCallback(
    async (batchId: string) => {
      if (!pubkey) return;
      setActionPendingId(`discard-${batchId}`);
      setActionError(null);
      try {
        await discardParkedBatch(pubkey, batchId);
        await Promise.all([
          queryClient.invalidateQueries({
            queryKey: parkedBatchesQueryKey(pubkey),
          }),
          queryClient.invalidateQueries({
            queryKey: agentHealthSummaryQueryKey,
          }),
          queryClient.invalidateQueries({
            queryKey: agentHealthEventsQueryKey(pubkey),
          }),
        ]);
      } catch (err) {
        setActionError(err instanceof Error ? err.message : String(err));
      } finally {
        setActionPendingId(null);
      }
    },
    [pubkey, queryClient],
  );

  const handleResumeNow = React.useCallback(async () => {
    if (!pubkey) return;
    setActionPendingId("resume-now");
    setActionError(null);
    try {
      await resumeAgentNow(pubkey);
      await Promise.all([
        queryClient.invalidateQueries({
          queryKey: agentHealthSummaryQueryKey,
        }),
        queryClient.invalidateQueries({
          queryKey: agentHealthEventsQueryKey(pubkey),
        }),
      ]);
    } catch (err) {
      setActionError(err instanceof Error ? err.message : String(err));
    } finally {
      setActionPendingId(null);
    }
  }, [pubkey, queryClient]);

  const handleKeepPaused = React.useCallback(async () => {
    if (!pubkey) return;
    setActionPendingId("keep-paused");
    setActionError(null);
    try {
      const baseMs = agent?.pausedUntil
        ? Math.max(Date.now(), new Date(agent.pausedUntil).getTime())
        : Date.now();
      const untilIso = new Date(baseMs + 60 * 60 * 1000).toISOString();
      await keepAgentPaused(pubkey, untilIso);
      await Promise.all([
        queryClient.invalidateQueries({
          queryKey: agentHealthSummaryQueryKey,
        }),
        queryClient.invalidateQueries({
          queryKey: agentHealthEventsQueryKey(pubkey),
        }),
      ]);
    } catch (err) {
      setActionError(err instanceof Error ? err.message : String(err));
    } finally {
      setActionPendingId(null);
    }
  }, [pubkey, agent?.pausedUntil, queryClient]);

  return (
    <Sheet onOpenChange={onOpenChange} open={open}>
      <SheetContent
        className="w-full space-y-6 overflow-y-auto p-6 sm:max-w-xl"
        data-testid="agent-health-drawer"
        side="right"
      >
        <SheetHeader>
          <SheetTitle>{agentName} Health</SheetTitle>
          <SheetDescription>
            {isRemote
              ? "Remote-owned agent: frames only, no local ledger."
              : `Health status, parked tasks, and recent events for ${agentName}.`}
          </SheetDescription>
        </SheetHeader>

        {/* Pause card */}
        <div
          className="space-y-3 rounded-lg border border-border/70 bg-card p-4"
          data-testid="agent-health-pause-card"
        >
          <div className="flex items-center justify-between">
            <span className="text-sm font-medium text-foreground">
              Pause status
            </span>
            <Badge
              variant={
                agent?.state === "paused"
                  ? "warning"
                  : agent?.state === "breaker"
                    ? "destructive"
                    : "secondary"
              }
            >
              {agent?.state === "paused"
                ? "Paused"
                : agent?.state === "breaker"
                  ? "Breaker Open"
                  : "Not paused"}
            </Badge>
          </div>
          {agent?.pausedUntil ? (
            <p className="text-xs text-muted-foreground">
              Paused until:{" "}
              <span className="font-medium text-foreground">
                {formatTimestamp(agent.pausedUntil)}
              </span>
            </p>
          ) : null}
          <p className="text-xs text-muted-foreground">
            Waiting tasks:{" "}
            <span className="font-medium text-foreground">
              {parkedBatches.length || (agent?.parked ?? 0)}
            </span>
          </p>
          {actionError ? (
            <p className="text-xs text-destructive">{actionError}</p>
          ) : null}
          <div className="flex flex-wrap gap-2 pt-1">
            <Button
              aria-label={`Resume ${agentName} now`}
              data-testid="agent-health-resume-now"
              disabled={actionPendingId === "resume-now"}
              onClick={() => void handleResumeNow()}
              size="sm"
              variant="outline"
            >
              Resume now
            </Button>
            <Button
              aria-label={`Keep ${agentName} paused (+1 h)`}
              data-testid="agent-health-keep-paused"
              disabled={actionPendingId === "keep-paused"}
              onClick={() => void handleKeepPaused()}
              size="sm"
              variant="outline"
            >
              Keep paused (+1 h)
            </Button>
          </div>
        </div>

        {/* Failed list with Retry / Discard */}
        <div className="space-y-3" data-testid="agent-health-failed-list">
          <div className="flex items-center justify-between">
            <h3 className="text-sm font-medium text-foreground">
              Failed &amp; Parked Tasks
            </h3>
            <span className="text-xs text-muted-foreground">
              {parkedBatches.length} items
            </span>
          </div>

          {isRemote ? (
            <p className="text-xs text-muted-foreground">
              frames only, no local ledger
            </p>
          ) : parkedBatchesQuery.isLoading ? (
            <p className="text-xs text-muted-foreground">
              Loading parked tasks...
            </p>
          ) : parkedBatchesQuery.isError ? (
            <p className="text-xs text-destructive">
              Failed to load parked tasks
            </p>
          ) : parkedBatches.length === 0 ? (
            <p className="text-xs text-muted-foreground">
              No failed or parked tasks for this agent.
            </p>
          ) : (
            <div className="space-y-3">
              {parkedBatches.map((batch) => (
                <div
                  className="space-y-2 rounded-lg border border-border/70 bg-muted/40 p-3"
                  data-testid={`agent-health-parked-batch-${batch.batchId}`}
                  key={batch.batchId}
                >
                  <div className="flex items-start justify-between gap-2">
                    <div className="min-w-0 flex-1">
                      <div className="flex flex-wrap items-center gap-2">
                        <span className="text-xs font-semibold text-foreground">
                          Batch {batch.batchId.slice(0, 8)}
                        </span>
                        {batch.needsReview ? (
                          <Badge variant="destructive">Needs Review</Badge>
                        ) : (
                          <Badge variant="warning">Parked</Badge>
                        )}
                      </div>
                      <div className="mt-0.5 text-xs text-muted-foreground">
                        <span>{formatTimestamp(batch.parkedAt)}</span>
                        {batch.channelId ? (
                          <>
                            <span className="mx-1.5">·</span>
                            <button
                              aria-label={`Navigate to channel ${batch.channelId} for batch ${batch.batchId} on ${agentName}`}
                              className="cursor-pointer underline hover:text-foreground"
                              onClick={() => goChannel(batch.channelId)}
                              type="button"
                            >
                              Channel: {batch.channelId.slice(0, 8)}...
                            </button>
                          </>
                        ) : null}
                      </div>
                    </div>
                  </div>

                  {batch.reason ? (
                    <p className="text-xs text-muted-foreground">
                      <span className="font-medium text-foreground">
                        Reason:{" "}
                      </span>
                      {batch.reason}
                    </p>
                  ) : null}

                  {batch.excerpt ? (
                    <div className="break-all rounded bg-background/80 p-2 font-mono text-xs text-muted-foreground">
                      {batch.excerpt}
                    </div>
                  ) : null}

                  <div className="flex items-center justify-end gap-2 pt-1">
                    <Button
                      aria-label={`Retry batch ${batch.batchId} for ${agentName}`}
                      data-testid={`agent-health-retry-${batch.batchId}`}
                      disabled={actionPendingId === `retry-${batch.batchId}`}
                      onClick={() => void handleRetry(batch.batchId)}
                      size="sm"
                      variant="outline"
                    >
                      Retry
                    </Button>
                    <Button
                      aria-label={`Discard batch ${batch.batchId} for ${agentName}`}
                      data-testid={`agent-health-discard-${batch.batchId}`}
                      disabled={actionPendingId === `discard-${batch.batchId}`}
                      onClick={() => void handleDiscard(batch.batchId)}
                      size="sm"
                      variant="destructive"
                    >
                      Discard
                    </Button>
                  </div>
                </div>
              ))}
            </div>
          )}
        </div>

        {/* Last 50 events */}
        <div className="space-y-3" data-testid="agent-health-events-list">
          <div className="flex items-center justify-between">
            <h3 className="text-sm font-medium text-foreground">
              Recent Health Events
            </h3>
            <span className="text-xs text-muted-foreground">Last 50</span>
          </div>

          {eventsQuery.isLoading ? (
            <p className="text-xs text-muted-foreground">
              Loading recent events...
            </p>
          ) : eventsQuery.isError ? (
            <p className="text-xs text-destructive">
              Failed to load recent events
            </p>
          ) : events.length === 0 ? (
            <p className="text-xs text-muted-foreground">
              No recent health events.
            </p>
          ) : (
            <div className="divide-y divide-border/50 overflow-hidden rounded-lg border border-border/70 bg-card">
              {events.map((event, idx) => (
                <div
                  className="space-y-1 p-3 text-xs"
                  key={`${event.at}-${event.eventKey || idx}`}
                >
                  <div className="flex items-center justify-between gap-2">
                    <span className="font-mono font-medium text-foreground">
                      {event.kind}
                    </span>
                    <span className="text-muted-foreground">
                      {formatTimestamp(event.at)}
                    </span>
                  </div>
                  {event.class ? (
                    <p className="text-muted-foreground">
                      Class:{" "}
                      <span className="text-foreground">{event.class}</span>
                    </p>
                  ) : null}
                  {event.batchId ? (
                    <p className="text-muted-foreground">
                      Batch:{" "}
                      <span className="font-mono text-foreground">
                        {event.batchId}
                      </span>
                    </p>
                  ) : null}
                  {event.channelId ? (
                    <p className="text-muted-foreground">
                      Channel:{" "}
                      <span className="font-mono text-foreground">
                        {event.channelId}
                      </span>
                    </p>
                  ) : null}
                  {event.payload ? (
                    <p className="truncate font-mono text-2xs text-muted-foreground">
                      {event.payload}
                    </p>
                  ) : null}
                </div>
              ))}
            </div>
          )}
        </div>
      </SheetContent>
    </Sheet>
  );
}
