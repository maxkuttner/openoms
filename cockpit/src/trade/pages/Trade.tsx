import { Stack } from "@mantine/core";
import type { Me } from "../App";
import { TradeBlotter } from "../components/TradeBlotter";

// TODO(task 7): the order ticket lands here, alongside the blotter, on the
// same screen.
export function TradePage({ me }: { me: Me }) {
  return (
    <Stack>
      <TradeBlotter portfolios={me.portfolios} />
    </Stack>
  );
}
