import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Grid, Tabs } from "@mantine/core";
import type { Me } from "../App";
import { TradeBlotter } from "../components/TradeBlotter";
import { OrderTicket } from "../components/OrderTicket";
import { Watchlist } from "../components/Watchlist";
import { PositionsPage } from "./Positions";

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
  // confirmation modal's label is built from the latter, not the id). Handed
  // down to OrderTicket as selectedInstrument, which adopts it into its own
  // internal state (see OrderTicket.tsx) — this does not make the ticket's
  // instrument field fully controlled from here.
  const [selectedInstrument, setSelectedInstrument] = useState<{
    id: string;
    symbol: string;
    venue: string;
    name: string;
  } | null>(null);

  return (
    <Grid>
      <Grid.Col span={{ base: 12, md: 3 }}>
        <Watchlist onSelectInstrument={setSelectedInstrument} />
      </Grid.Col>
      <Grid.Col span={{ base: 12, md: 3 }}>
        <OrderTicket
          portfolios={me.portfolios}
          selectedWatchlistInstrument={selectedInstrument}
          onSubmitted={(orderId) => {
            setFollowOrderId(orderId);
            setActiveTab("orders");
            // Nudge the blotter's own poll (queryKey ["/orders"], see
            // TradeBlotter.tsx) to refetch right away instead of waiting out
            // its interval.
            queryClient.invalidateQueries({ queryKey: ["/orders"] });
          }}
        />
      </Grid.Col>
      <Grid.Col span={{ base: 12, md: 6 }}>
        <Tabs value={activeTab} onChange={setActiveTab}>
          <Tabs.List>
            <Tabs.Tab value="orders">Orders</Tabs.Tab>
            <Tabs.Tab value="positions">Positions</Tabs.Tab>
          </Tabs.List>
          <Tabs.Panel value="orders" pt="md">
            <TradeBlotter portfolios={me.portfolios} followOrderId={followOrderId} />
          </Tabs.Panel>
          <Tabs.Panel value="positions" pt="md">
            <PositionsPage me={me} />
          </Tabs.Panel>
        </Tabs>
      </Grid.Col>
    </Grid>
  );
}
