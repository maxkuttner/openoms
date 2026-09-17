import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Grid, Stack } from "@mantine/core";
import type { Me } from "../App";
import { TradeBlotter } from "../components/TradeBlotter";
import { OrderTicket } from "../components/OrderTicket";

export function TradePage({ me }: { me: Me }) {
  const queryClient = useQueryClient();

  // The order the ticket last put into the system. POST /orders/submit answers
  // 204 with an empty body, so the only way to observe what happened to it is
  // to fetch it: the blotter takes this id, moves to that order's row and polls
  // GET /orders/{id} until it leaves `submitted`. That is the spec's "after
  // submission" behaviour, and it is what makes a recorded-but-never-routed
  // order visible to the trader instead of leaving it looking live.
  const [followOrderId, setFollowOrderId] = useState<string | null>(null);

  return (
    <Grid>
      <Grid.Col span={{ base: 12, md: 4 }}>
        <OrderTicket
          portfolios={me.portfolios}
          onSubmitted={(orderId) => {
            setFollowOrderId(orderId);
            // Nudge the blotter's own poll (queryKey ["/orders"], see
            // TradeBlotter.tsx) to refetch right away instead of waiting out
            // its interval.
            queryClient.invalidateQueries({ queryKey: ["/orders"] });
          }}
        />
      </Grid.Col>
      <Grid.Col span={{ base: 12, md: 8 }}>
        <Stack>
          <TradeBlotter portfolios={me.portfolios} followOrderId={followOrderId} />
        </Stack>
      </Grid.Col>
    </Grid>
  );
}
