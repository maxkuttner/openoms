import { useQueryClient } from "@tanstack/react-query";
import { Grid, Stack } from "@mantine/core";
import type { Me } from "../App";
import { TradeBlotter } from "../components/TradeBlotter";
import { OrderTicket } from "../components/OrderTicket";

export function TradePage({ me }: { me: Me }) {
  const queryClient = useQueryClient();

  return (
    <Grid>
      <Grid.Col span={{ base: 12, md: 4 }}>
        <OrderTicket
          portfolios={me.portfolios}
          // Nudge the blotter's own poll (queryKey ["/orders"], see
          // TradeBlotter.tsx) to refetch right away instead of waiting out
          // its interval, on every accepted submit — including the 409 the
          // idempotency contract absorbs, which is a live order too.
          onSubmitted={() => queryClient.invalidateQueries({ queryKey: ["/orders"] })}
        />
      </Grid.Col>
      <Grid.Col span={{ base: 12, md: 8 }}>
        <Stack>
          <TradeBlotter portfolios={me.portfolios} />
        </Stack>
      </Grid.Col>
    </Grid>
  );
}
