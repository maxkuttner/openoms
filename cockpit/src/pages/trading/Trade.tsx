import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import type { Me } from "../../access";
import { TradeBlotter } from "../../components/trading/TradeBlotter";
import { OrderTicket } from "../../components/trading/OrderTicket";
import { StrategyTicket } from "../../components/trading/StrategyTicket";
import { InstrumentSearchModal } from "../../components/trading/InstrumentSearchModal";
import { InstrumentHeader } from "../../components/trading/InstrumentHeader";
import { OrderBook } from "../../components/market/OrderBook";
import { PriceChart } from "../../components/market/PriceChart";
import { Notice } from "../../components/common";
import { sampleMarketData } from "../../marketData";
import { PositionsPage } from "./Positions";
import type { Side, StrategyLeg } from "../../components/trading/types";

export function TradePage({ me }: { me: Me }) {
  const queryClient = useQueryClient();

  // The order the ticket last put into the system. POST /orders/submit answers
  // 204 with an empty body, so the only way to observe what happened to it is
  // to fetch it: the blotter takes this id, moves to that order's row and polls
  // GET /orders/{id} until it leaves `submitted`. That is the spec's "after
  // submission" behaviour, and it is what makes a recorded-but-never-routed
  // order visible to the trader instead of leaving it looking live.
  const [followOrderId, setFollowOrderId] = useState<string | null>(null);

  // Orders and positions used to be separate routes; they are now tabs over
  // the same blotter panel so a trader never has to leave the ticket to check
  // either one. Submitting an order always jumps back to the Orders tab, since
  // that is where `followOrderId` becomes visible.
  const [activeTab, setActiveTab] = useState<string | null>("orders");

  // The instrument a trader last clicked in the Watchlist — the full row
  // (id/symbol/venue/name), not just the id: OrderTicket needs the full
  // shape to set BOTH its instrumentId and selectedInstrument (the
  // confirmation modal's label is built from selectedInstrument, not
  // instrumentId). Handed down to OrderTicket as selectedInstrument, which
  // adopts it into its own internal state (see OrderTicket.tsx) — this does
  // not make the ticket's instrument field fully controlled from here.
  const [selectedInstrument, setSelectedInstrument] = useState<{
    id: string;
    symbol: string;
    venue: string;
    name: string;
    side?: Side;
  } | null>(null);

  // Set only when InstrumentSearchModal returns more than one leg — while
  // set, StrategyTicket renders in the order-ticket slot instead of
  // OrderTicket. A single-leg pick (search or Watchlist) always goes through
  // the unmodified OrderTicket via selectedInstrument above.
  const [pendingLegs, setPendingLegs] = useState<StrategyLeg[] | null>(null);
  const [searchOpen, setSearchOpen] = useState(false);

  function onOrdersSubmitted(latestOrderId: string) {
    setFollowOrderId(latestOrderId);
    setActiveTab("orders");
    queryClient.invalidateQueries({ queryKey: ["/orders"] });
  }

  return (
    <>
      <InstrumentSearchModal
        opened={searchOpen}
        onClose={() => setSearchOpen(false)}
        onPickSingle={(instrument) => {
          setPendingLegs(null);
          setSelectedInstrument(instrument);
        }}
        onPickLegs={(legs) => setPendingLegs(legs)}
      />
      <div className="flex flex-col gap-3">
        <InstrumentHeader
          source={sampleMarketData}
          instrument={selectedInstrument}
          onPick={(i) => { setPendingLegs(null); setSelectedInstrument(i); }}
          onOpenSearch={() => setSearchOpen(true)}
        />
        <div className="flex flex-col gap-3 lg:flex-row lg:items-start">
          <div className="h-[420px] min-w-0 flex-1 xl:h-[460px]">
            {selectedInstrument ? (
              // Sample prices only: nothing here feeds the ticket below.
              <PriceChart source={sampleMarketData} instrumentId={selectedInstrument.symbol} />
            ) : (
              <Notice tone="info">Pick an instrument to see its chart and order book.</Notice>
            )}
          </div>
          <div className="flex w-full flex-col gap-3 lg:w-[360px] lg:shrink-0">
            {selectedInstrument && <OrderBook source={sampleMarketData} instrumentId={selectedInstrument.symbol} />}
        <div>
          {pendingLegs ? (
            // Keyed by the combo's own legs so a new "Use this combo" pick
            // mounts a fresh ticket (fresh order ids, fresh per-leg status)
            // instead of reusing one still showing a previous attempt's
            // results. The ticket itself decides when it's done — it stays
            // mounted after submit so the trader can read every leg's real
            // outcome; only its own "Close" button (onCancel) dismisses it.
            <StrategyTicket
              key={pendingLegs.map((l) => l.instrumentId + l.side).join(",")}
              portfolios={me.portfolios}
              legs={pendingLegs}
              onCancel={() => setPendingLegs(null)}
              onSubmitted={(orderIds) => {
                if (orderIds.length > 0) onOrdersSubmitted(orderIds[orderIds.length - 1]);
              }}
            />
          ) : (
            <OrderTicket
              portfolios={me.portfolios}
              selectedWatchlistInstrument={selectedInstrument}
              onSubmitted={onOrdersSubmitted}
            />
          )}
        </div>
          </div>
        </div>
        <div className="min-w-0">
          <Tabs value={activeTab ?? "orders"} onValueChange={setActiveTab}>
            <TabsList>
              <TabsTrigger value="orders">Orders</TabsTrigger>
              <TabsTrigger value="positions">Positions</TabsTrigger>
            </TabsList>
            <TabsContent value="orders" className="pt-2">
              <TradeBlotter portfolios={me.portfolios} followOrderId={followOrderId} />
            </TabsContent>
            <TabsContent value="positions" className="pt-2">
              <PositionsPage me={me} />
            </TabsContent>
          </Tabs>
        </div>
      </div>
    </>
  );
}
