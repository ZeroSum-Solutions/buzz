/**
 * The Files index's live relay subscription.
 *
 * Kept out of the hook so a test can drive the exact production wiring against
 * a real `RelayClient` rather than a stub that can only throw. The whole point
 * of this seam is what a stub cannot reproduce: a relay that answers the REQ
 * with CLOSED, or closes an open subscription later, both of which the shared
 * client reports through callbacks rather than by rejecting.
 */

import type { RelayClient } from "@/shared/api/relayClientSession";
import { CHANNEL_EVENT_KINDS } from "@/shared/constants/kinds";
import type { RelayEvent } from "@/shared/api/types";
import type { LiveSubscriptionHandlers } from "./channelFilesBackfill";

/**
 * Live events the index looks at, from now on.
 *
 * The channel's own kind set, minus the thread-summary overlay the timeline's
 * window store subscribes to: this index reads attachment-bearing events plus
 * the deletion and edit markers that change them, and nothing else.
 */
export function buildChannelFilesLiveFilter(channelId: string) {
  return {
    kinds: [...CHANNEL_EVENT_KINDS],
    "#h": [channelId],
    limit: 1000,
    since: Math.floor(Date.now() / 1_000),
  };
}

/**
 * Open the index's live subscription on `client`.
 *
 * Both readiness and the later terminal CLOSED are forwarded to `handlers`, so
 * the controller can tell an open subscription from a refused one. Resolves to
 * the unsubscribe closure.
 */
export function subscribeChannelFilesLive(
  client: Pick<RelayClient, "subscribeLive">,
  channelId: string,
  onEvent: (event: RelayEvent) => void,
  handlers: LiveSubscriptionHandlers,
): Promise<() => void | Promise<void>> {
  return client.subscribeLive(
    buildChannelFilesLiveFilter(channelId),
    onEvent,
    handlers.onReady,
    undefined,
    handlers.onTerminalClose,
  );
}
