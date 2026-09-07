export const HEALTH_FRAME_KINDS = new Set<string>([
  "turn_failed",
  "batch_parked",
  "batch_replayed",
  "batch_needs_review",
  "agent_paused",
  "agent_resumed",
  "breaker_opened",
  "breaker_closed",
  "relay_reconnected",
]);

export interface HealthFrame {
  at: string;
  kind: string;
  batchId?: string;
  channelId?: string;
  class?: string;
  payload?: unknown;
}

export function parseHealthFrame(event: unknown): HealthFrame | null {
  if (typeof event !== "object" || event === null) {
    return null;
  }

  const e = event as Record<string, unknown>;
  const payload =
    typeof e.payload === "object" && e.payload !== null
      ? (e.payload as Record<string, unknown>)
      : null;

  const kind =
    typeof e.kind === "string" && HEALTH_FRAME_KINDS.has(e.kind)
      ? e.kind
      : typeof payload?.kind === "string" &&
          HEALTH_FRAME_KINDS.has(payload.kind)
        ? payload.kind
        : null;

  if (!kind) {
    return null;
  }

  const at =
    typeof e.at === "string" && e.at.length > 0
      ? e.at
      : typeof payload?.at === "string" && payload.at.length > 0
        ? payload.at
        : null;

  if (!at) {
    return null;
  }

  const batchId =
    typeof e.batchId === "string" && e.batchId.length > 0
      ? e.batchId
      : typeof e.batch_id === "string" && e.batch_id.length > 0
        ? e.batch_id
        : typeof payload?.batchId === "string" && payload.batchId.length > 0
          ? payload.batchId
          : typeof payload?.batch_id === "string" && payload.batch_id.length > 0
            ? payload.batch_id
            : undefined;

  const channelId =
    typeof e.channelId === "string" && e.channelId.length > 0
      ? e.channelId
      : typeof e.channel_id === "string" && e.channel_id.length > 0
        ? e.channel_id
        : typeof payload?.channelId === "string" && payload.channelId.length > 0
          ? payload.channelId
          : typeof payload?.channel_id === "string" &&
              payload.channel_id.length > 0
            ? payload.channel_id
            : undefined;

  const classVal =
    typeof e.class === "string" && e.class.length > 0
      ? e.class
      : typeof payload?.class === "string" && payload.class.length > 0
        ? payload.class
        : payload?.outcome &&
            typeof payload.outcome === "object" &&
            typeof (payload.outcome as Record<string, unknown>).class ===
              "string" &&
            (payload.outcome as Record<string, unknown>).class
          ? ((payload.outcome as Record<string, unknown>).class as string)
          : undefined;

  const payloadVal = e.payload !== undefined ? e.payload : e;

  const result: HealthFrame = {
    at,
    kind,
  };

  if (batchId !== undefined) {
    result.batchId = batchId;
  }
  if (channelId !== undefined) {
    result.channelId = channelId;
  }
  if (classVal !== undefined) {
    result.class = classVal;
  }
  if (payloadVal !== undefined) {
    result.payload = payloadVal;
  }

  return result;
}
