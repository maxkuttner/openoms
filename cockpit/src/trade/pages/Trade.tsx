import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Grid, Tabs } from "@mantine/core";
import type { Me } from "../App";
import { TradeBlotter } from "../components/TradeBlotter";
import { OrderTicket } from "../components/OrderTicket";
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

  return (
    <Grid>
      <Grid.Col span={{ base: 12, md: 4 }}>
        <OrderTicket
          portfolios={me.portfolios}
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
      <Grid.Col span={{ base: 12, md: 8 }}>
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
