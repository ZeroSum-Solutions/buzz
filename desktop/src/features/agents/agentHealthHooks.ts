import * as React from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { listen } from "@tauri-apps/api/event";

import { invokeTauri } from "@/shared/api/tauri";
import type {
  AgentHealthAlert,
  AgentHealthIngestResult,
  ParkedBatchView,
} from "@/shared/api/types";
import { sendDesktopNotification } from "@/features/notifications/lib/desktop";
import { useAppFocused } from "@/shared/lib/useDocumentVisible";
import type {
  AgentHealthCounters,
  AgentHealthSummaryData,
} from "./agentHealthSummary.ts";

export type { AgentHealthAlert, AgentHealthIngestResult };

export type AgentHealthEvent = {
  agent: string;
  at: number;
  kind: string;
  eventKey: string;
  batchId?: string | null;
  channelId?: string | null;
  class?: string | null;
  payload?: string | null;
};

export const AGENT_HEALTH_FOCUS_STALE_TIME_MS = 10_000;

export const agentHealthSummaryFocusRefetchPolicy = {
  staleTime: AGENT_HEALTH_FOCUS_STALE_TIME_MS,
  refetchOnWindowFocus: false,
} as const;

export const agentHealthSummaryQueryKey = ["agent-health", "summary"] as const;

export const agentHealthEventsQueryKey = (
  agent: string,
  kinds?: readonly string[] | null,
) => ["agent-health", "events", agent, kinds ?? []] as const;

export const parkedBatchesQueryKey = (agent: string) =>
  ["agent-health", "parked-batches", agent] as const;

/** How long one notification delivery or acknowledgement may take before
 * `syncAgentHealth` gives up waiting on it. Notification permission
 * prompts, native delivery, and the ack invoke are all awaited in this
 * function's serial loop — a single never-resolving promise among them
 * would otherwise hang every later frame this store processes (the
 * observer relay queue awaits `syncAgentHealth`'s callers serially too). */
export const HEALTH_NOTIFICATION_TIMEOUT_MS = 5_000;

/**
 * Resolve to `onTimeout` if `promise` neither resolves nor rejects within
 * `ms`, and to `onTimeout` (not a thrown error) if it rejects — the caller
 * treats "failed" and "never settled" identically, so both collapse to the
 * same fallback value instead of one of them propagating an exception.
 */
export function withTimeout<T>(
  promise: Promise<T>,
  ms: number,
  onTimeout: T,
): Promise<T> {
  return new Promise((resolve) => {
    let settled = false;
    const timer = setTimeout(() => {
      if (!settled) {
        settled = true;
        resolve(onTimeout);
      }
    }, ms);
    promise.then(
      (value) => {
        if (!settled) {
          settled = true;
          clearTimeout(timer);
          resolve(value);
        }
      },
      () => {
        if (!settled) {
          settled = true;
          clearTimeout(timer);
          resolve(onTimeout);
        }
      },
    );
  });
}

export async function syncAgentHealth(
  agent?: string | null,
): Promise<AgentHealthIngestResult> {
  const result = await invokeTauri<AgentHealthIngestResult>(
    "sync_agent_health",
    {
      agent: agent ?? null,
    },
  );
  if (result?.alerts && Array.isArray(result.alerts)) {
    const delivered: AgentHealthAlert[] = [];
    for (const alert of result.alerts) {
      const deliveredSuccessfully = await withTimeout(
        sendDesktopNotification({
          title: alert.title,
          body: alert.body,
        }),
        HEALTH_NOTIFICATION_TIMEOUT_MS,
        false,
      );
      if (deliveredSuccessfully) {
        delivered.push(alert);
      }
    }
    if (delivered.length > 0) {
      await withTimeout(
        invokeTauri("record_delivered_alerts", { alerts: delivered }),
        HEALTH_NOTIFICATION_TIMEOUT_MS,
        undefined,
      );
    }
  }
  return result;
}

export async function getAgentHealthSummary(
  sinceHours?: number | null,
): Promise<AgentHealthCounters[]> {
  return invokeTauri<AgentHealthCounters[]>("get_agent_health_summary", {
    sinceHours: sinceHours ?? null,
  });
}

export async function getAgentHealthEvents(
  agent: string,
  options?: {
    kinds?: readonly string[] | null;
    sinceHours?: number | null;
    limit?: number | null;
  },
): Promise<AgentHealthEvent[]> {
  return invokeTauri<AgentHealthEvent[]>("get_agent_health_events", {
    agent,
    kinds: options?.kinds ?? null,
    sinceHours: options?.sinceHours ?? null,
    limit: options?.limit ?? null,
  });
}

export async function getParkedBatches(
  agent: string,
): Promise<ParkedBatchView[]> {
  return invokeTauri<ParkedBatchView[]>("get_parked_batches", { agent });
}

export async function fetchAgentHealthSummary(): Promise<AgentHealthSummaryData> {
  let syncError = false;
  let syncErrorAgents: string[] = [];
  try {
    const result = await syncAgentHealth();
    // A resolved `Ok` response can still carry per-agent failures the
    // backend swallowed rather than aborting the whole sync for (a corrupt
    // ledger, a park-file read failure, …) — those must degrade this
    // summary the same way an outright rejection does, not be silently
    // dropped just because the promise itself resolved.
    if (result?.errors && result.errors.length > 0) {
      syncError = true;
      syncErrorAgents = result.errors.map(([agent]) => agent);
    }
  } catch (error) {
    console.debug("sync_agent_health error (skipped):", error);
    syncError = true;
  }

  const [summary24h, summary7d] = await Promise.all([
    getAgentHealthSummary(24),
    getAgentHealthSummary(168),
  ]);

  return { summary24h, summary7d, syncError, syncErrorAgents };
}

export function useAgentHealthSummaryQuery(options?: { enabled?: boolean }) {
  const queryClient = useQueryClient();
  const appFocused = useAppFocused();
  const wasFocused = React.useRef(appFocused);

  React.useEffect(() => {
    const returnedToForeground = appFocused && !wasFocused.current;
    wasFocused.current = appFocused;
    if (!returnedToForeground) return;

    void queryClient.refetchQueries({
      queryKey: agentHealthSummaryQueryKey,
      stale: true,
      type: "active",
    });
  }, [appFocused, queryClient]);

  React.useEffect(() => {
    let active = true;
    const unlistenPromise = Promise.resolve()
      .then(() =>
        listen("managed-agent-runtime-status", () => {
          if (!active) return;
          void queryClient.invalidateQueries({
            queryKey: agentHealthSummaryQueryKey,
          });
        }),
      )
      .catch(() => () => {});

    return () => {
      active = false;
      void unlistenPromise.then((fn) => fn?.());
    };
  }, [queryClient]);

  return useQuery({
    queryKey: agentHealthSummaryQueryKey,
    queryFn: fetchAgentHealthSummary,
    enabled: options?.enabled ?? true,
    ...agentHealthSummaryFocusRefetchPolicy,
  });
}

export function useAgentHealthEventsQuery(
  agent: string | null | undefined,
  options?: {
    kinds?: readonly string[] | null;
    sinceHours?: number | null;
    limit?: number | null;
    enabled?: boolean;
  },
) {
  const queryClient = useQueryClient();
  const queryKey = agentHealthEventsQueryKey(agent ?? "", options?.kinds);

  React.useEffect(() => {
    if (!agent) return;
    let active = true;
    const unlistenPromise = Promise.resolve()
      .then(() =>
        listen("managed-agent-runtime-status", () => {
          if (!active) return;
          void queryClient.invalidateQueries({ queryKey });
        }),
      )
      .catch(() => () => {});

    return () => {
      active = false;
      void unlistenPromise.then((fn) => fn?.());
    };
  }, [agent, queryClient, queryKey]);

  return useQuery({
    queryKey,
    queryFn: () =>
      getAgentHealthEvents(agent ?? "", {
        kinds: options?.kinds,
        sinceHours: options?.sinceHours,
        limit: options?.limit,
      }),
    enabled: Boolean(agent) && (options?.enabled ?? true),
    ...agentHealthSummaryFocusRefetchPolicy,
  });
}

export function useParkedBatchesQuery(
  agent: string | null | undefined,
  options?: { enabled?: boolean },
) {
  const queryClient = useQueryClient();
  const queryKey = parkedBatchesQueryKey(agent ?? "");

  React.useEffect(() => {
    if (!agent) return;
    let active = true;
    const unlistenPromise = Promise.resolve()
      .then(() =>
        listen("managed-agent-runtime-status", () => {
          if (!active) return;
          void queryClient.invalidateQueries({ queryKey });
        }),
      )
      .catch(() => () => {});

    return () => {
      active = false;
      void unlistenPromise.then((fn) => fn?.());
    };
  }, [agent, queryClient, queryKey]);

  return useQuery({
    queryKey,
    queryFn: () => getParkedBatches(agent ?? ""),
    enabled: Boolean(agent) && (options?.enabled ?? true),
    ...agentHealthSummaryFocusRefetchPolicy,
  });
}
