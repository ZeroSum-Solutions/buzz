import { sendAgentObserverControl } from "@/shared/api/observerRelay";

export async function replayParkedBatch(
  pubkey: string,
  batchId: string,
): Promise<void> {
  await sendAgentObserverControl(pubkey, {
    type: "replay_batch",
    batchId,
  });
}

export async function discardParkedBatch(
  pubkey: string,
  batchId: string,
): Promise<void> {
  await sendAgentObserverControl(pubkey, {
    type: "discard_batch",
    batchId,
  });
}

export async function resumeAgentNow(pubkey: string): Promise<void> {
  await sendAgentObserverControl(pubkey, {
    type: "resume_now",
  });
}

export async function keepAgentPaused(
  pubkey: string,
  untilIso: string,
): Promise<void> {
  await sendAgentObserverControl(pubkey, {
    type: "keep_paused",
    until: untilIso,
  });
}
