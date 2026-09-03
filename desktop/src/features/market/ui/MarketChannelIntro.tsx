import type * as React from "react";
import { useAppNavigation } from "@/app/navigation/useAppNavigation";

import { MessageCircle } from "lucide-react";

import { useMarketChannel } from "@/features/market/lib/MarketChannelContext";
import type { MarketBid } from "@/features/market/lib/marketProtocol";
import { isMarketProtocolMessage } from "@/features/market/lib/marketTimeline";
import { MarketContractCard } from "@/features/market/ui/MarketContractCard";
import type { TimelineMessage } from "@/features/messages/types";
import { Button } from "@/shared/ui/button";
import { UserAvatar } from "@/shared/ui/UserAvatar";

function BidList({
  bids,
  channelId,
}: {
  bids: MarketBid[];
  channelId: string;
}) {
  const { goChannel } = useAppNavigation();
  return (
    <section className="mt-4" data-testid="market-bid-list">
      <div className="mb-2 flex items-center justify-between gap-3">
        <h3 className="text-sm font-semibold">Bids</h3>
        <span className="text-xs text-muted-foreground">
          {bids.length} {bids.length === 1 ? "bid" : "bids"}
        </span>
      </div>
      {bids.length === 0 ? (
        <p className="text-sm text-muted-foreground">No bids yet.</p>
      ) : (
        <div className="divide-y divide-border/60 overflow-hidden rounded-xl border border-border/70">
          {bids.map((bid) => (
            <div
              className="flex items-center gap-3 px-3 py-2.5"
              key={bid.eventId}
            >
              <UserAvatar
                avatarUrl={null}
                className="h-8 w-8 shrink-0 text-xs"
                displayName={bid.actorName}
                shape="squircle"
                size="sm"
              />
              <div className="min-w-0 flex-1">
                <div className="flex flex-wrap items-baseline gap-x-2">
                  <span className="text-sm font-medium">{bid.actorName}</span>
                  <span className="text-xs text-muted-foreground">
                    {bid.amountSats != null
                      ? `${bid.amountSats} sats per unit`
                      : "Terms in thread"}
                    {` · ${bid.quantity} ${bid.quantity === 1 ? "unit" : "units"}`}
                  </span>
                </div>
                <p className="truncate text-sm text-muted-foreground">
                  {bid.message}
                </p>
              </div>
              <Button
                aria-label={`Open negotiation with ${bid.actorName}`}
                onClick={() => {
                  void goChannel(channelId, { thread: bid.eventId });
                }}
                size="sm"
                type="button"
                variant="ghost"
              >
                <MessageCircle className="mr-1.5 h-4 w-4" />
                Thread
              </Button>
            </div>
          ))}
        </div>
      )}
    </section>
  );
}

export function MarketChannelIntro({
  anchorMessage,
}: {
  anchorMessage: Pick<TimelineMessage, "body" | "createdAt" | "id">;
}): React.ReactNode {
  const projection = useMarketChannel();
  if (!projection || isMarketProtocolMessage(anchorMessage)) return undefined;
  const bids = projection.bids.filter(
    (bid) =>
      bid.createdAt > anchorMessage.createdAt ||
      (bid.createdAt === anchorMessage.createdAt &&
        bid.eventId.localeCompare(anchorMessage.id) > 0),
  );
  return (
    <div className="mt-2">
      <MarketContractCard scenario={projection.scenario} />
      <BidList bids={bids} channelId={projection.channelId} />
    </div>
  );
}
