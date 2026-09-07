export type AgentHealthState = "active" | "paused" | "breaker" | "offline";

export type AgentHealthRow = {
  pubkey: string;
  name: string;
  state: AgentHealthState;
  pausedUntil: string | null;
  turns24h: number;
  turns7d: number;
  failed24h: number;
  failed7d: number;
  parked: number;
  needsReview: number;
  lastErrorClass: string | null;
  lastErrorAt: number | null;
  reconnects24h: number;
};

export type AgentHealthCounters = {
  agent?: string;
  pubkey?: string;
  turns?: number;
  failed?: number;
  parked?: number;
  needsReview?: number;
  needs_review?: number;
  reconnects?: number;
  lastFailureClass?: string | null;
  last_failure_class?: string | null;
  lastErrorClass?: string | null;
  lastFailureAt?: number | null;
  last_failure_at?: number | null;
  lastErrorAt?: number | null;
  latestPausedUntil?: string | null;
  latest_paused_until?: string | null;
  pausedUntil?: string | null;
  breakerOpen?: boolean;
  breaker_open?: boolean;
  latestBreakerOpen?: boolean;
  turns24h?: number;
  turns7d?: number;
  failed24h?: number;
  failed7d?: number;
  reconnects24h?: number;
};

export type AgentHealthSummaryData = {
  summary24h?: readonly AgentHealthCounters[] | null;
  summary7d?: readonly AgentHealthCounters[] | null;
  counters24h?: readonly AgentHealthCounters[] | null;
  counters7d?: readonly AgentHealthCounters[] | null;
};

export type AgentHealthSummaryCounters =
  | readonly AgentHealthCounters[]
  | AgentHealthSummaryData
  | null
  | undefined;

export type ManagedAgentLike = {
  pubkey: string;
  name?: string;
  status?: string;
};

export type ManagedAgentRuntimeLike = {
  pubkey: string;
  lifecycle?: string;
  relayUrl?: string;
  error?: string | null;
};

function parseNowMs(now?: number | string | Date | null): number {
  if (now instanceof Date) return now.getTime();
  if (typeof now === "string") {
    const ms = new Date(now).getTime();
    return Number.isNaN(ms) ? Date.now() : ms;
  }
  if (typeof now === "number") {
    return now > 1e11 ? now : now * 1000;
  }
  return Date.now();
}

function isPauseActive(
  pausedUntil: string | null | undefined,
  nowMs: number,
): boolean {
  if (!pausedUntil) return false;
  const pausedUntilMs = new Date(pausedUntil).getTime();
  if (Number.isNaN(pausedUntilMs)) return false;
  return pausedUntilMs > nowMs;
}

function evaluateRuntimeActive(
  pubkey: string,
  runtimes: readonly ManagedAgentRuntimeLike[] | null | undefined,
  managedAgents: readonly ManagedAgentLike[] | null | undefined,
): boolean {
  const matchingRuntimes = runtimes?.filter((r) => r.pubkey === pubkey) ?? [];
  if (matchingRuntimes.length > 0) {
    return matchingRuntimes.some(
      (r) =>
        r.lifecycle === "ready" ||
        r.lifecycle === "running" ||
        r.lifecycle === "listening" ||
        r.lifecycle === "waking" ||
        r.lifecycle === "starting",
    );
  }
  const matchingAgent = managedAgents?.find((a) => a.pubkey === pubkey);
  if (matchingAgent?.status) {
    return matchingAgent.status === "running";
  }
  return false;
}

function computeAgentState(
  isBreakerOpen: boolean,
  isPaused: boolean,
  isRuntimeActive: boolean,
): AgentHealthState {
  if (isBreakerOpen) return "breaker";
  if (isPaused) return "paused";
  if (isRuntimeActive) return "active";
  return "offline";
}

export function buildHealthRows(
  counters: AgentHealthSummaryCounters,
  managedAgents?: readonly ManagedAgentLike[] | null,
  runtimes?: readonly ManagedAgentRuntimeLike[] | null,
  now?: number | string | Date | null,
): AgentHealthRow[] {
  const nowMs = parseNowMs(now);

  let list24h: readonly AgentHealthCounters[] = [];
  let list7d: readonly AgentHealthCounters[] = [];

  if (Array.isArray(counters)) {
    list24h = counters;
    list7d = counters;
  } else if (counters && typeof counters === "object") {
    const obj = counters as Record<string, unknown>;
    if (Array.isArray(obj.summary24h)) {
      list24h = obj.summary24h as readonly AgentHealthCounters[];
    } else if (Array.isArray(obj.counters24h)) {
      list24h = obj.counters24h as readonly AgentHealthCounters[];
    }
    if (Array.isArray(obj.summary7d)) {
      list7d = obj.summary7d as readonly AgentHealthCounters[];
    } else if (Array.isArray(obj.counters7d)) {
      list7d = obj.counters7d as readonly AgentHealthCounters[];
    }
  }

  const map24h = new Map<string, AgentHealthCounters>();
  for (const c of list24h) {
    const key = c.agent ?? c.pubkey;
    if (key) map24h.set(key, c);
  }

  const map7d = new Map<string, AgentHealthCounters>();
  for (const c of list7d) {
    const key = c.agent ?? c.pubkey;
    if (key) map7d.set(key, c);
  }

  const allPubkeys = new Set<string>();
  if (managedAgents) {
    for (const a of managedAgents) {
      if (a.pubkey) allPubkeys.add(a.pubkey);
    }
  }
  for (const key of map24h.keys()) {
    allPubkeys.add(key);
  }
  for (const key of map7d.keys()) {
    allPubkeys.add(key);
  }

  const rows: AgentHealthRow[] = [];

  for (const pubkey of allPubkeys) {
    const c24 = map24h.get(pubkey);
    const c7d = map7d.get(pubkey);
    const agentRecord = managedAgents?.find((a) => a.pubkey === pubkey);

    const name = agentRecord?.name ?? pubkey;

    const turns24h = c24?.turns24h ?? c24?.turns ?? 0;
    const turns7d = c7d?.turns7d ?? c7d?.turns ?? turns24h;

    const failed24h = c24?.failed24h ?? c24?.failed ?? 0;
    const failed7d = c7d?.failed7d ?? c7d?.failed ?? failed24h;

    const parked = c24?.parked ?? c7d?.parked ?? 0;
    const needsReview =
      c24?.needsReview ??
      c24?.needs_review ??
      c7d?.needsReview ??
      c7d?.needs_review ??
      0;

    const reconnects24h = c24?.reconnects24h ?? c24?.reconnects ?? 0;

    const lastErrorClass =
      c24?.lastErrorClass ??
      c24?.lastFailureClass ??
      c24?.last_failure_class ??
      c7d?.lastErrorClass ??
      c7d?.lastFailureClass ??
      c7d?.last_failure_class ??
      null;

    const lastErrorAt =
      c24?.lastErrorAt ??
      c24?.lastFailureAt ??
      c24?.last_failure_at ??
      c7d?.lastErrorAt ??
      c7d?.lastFailureAt ??
      c7d?.last_failure_at ??
      null;

    const rawPausedUntil =
      c24?.pausedUntil ??
      c24?.latestPausedUntil ??
      c24?.latest_paused_until ??
      c7d?.pausedUntil ??
      c7d?.latestPausedUntil ??
      c7d?.latest_paused_until ??
      null;

    const isBreakerOpen = Boolean(
      c24?.breakerOpen ??
        c24?.breaker_open ??
        c24?.latestBreakerOpen ??
        c7d?.breakerOpen ??
        c7d?.breaker_open ??
        c7d?.latestBreakerOpen ??
        false,
    );

    const isPaused = isPauseActive(rawPausedUntil, nowMs);
    const isRuntimeActive = evaluateRuntimeActive(
      pubkey,
      runtimes,
      managedAgents,
    );

    const state = computeAgentState(isBreakerOpen, isPaused, isRuntimeActive);
    const pausedUntil = rawPausedUntil ?? null;

    rows.push({
      pubkey,
      name,
      state,
      pausedUntil,
      turns24h,
      turns7d,
      failed24h,
      failed7d,
      parked,
      needsReview,
      lastErrorClass,
      lastErrorAt,
      reconnects24h,
    });
  }

  return rows;
}

export function hasHealthAlertBadge(
  rows:
    | readonly Pick<AgentHealthRow, "needsReview" | "state">[]
    | null
    | undefined,
): boolean {
  if (!rows || rows.length === 0) return false;
  return rows.some(
    (row) => (row.needsReview ?? 0) > 0 || row.state === "breaker",
  );
}
