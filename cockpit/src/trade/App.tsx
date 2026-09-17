import { useQuery } from "@tanstack/react-query";
import { AppShell, Group, Loader, Text } from "@mantine/core";
import { Routes, Route, Navigate } from "react-router-dom";
import { tradeApi } from "./api/client";
import { TradePage } from "./pages/Trade";
import { PositionsPage } from "./pages/Positions";

export type GrantedPortfolio = {
  portfolio_id: string;
  code: string;
  name: string;
  status: string;
  base_currency: string | null;
  can_trade: boolean;
  can_view: boolean;
  can_allocate: boolean;
};

export type Me = {
  principal_id: string;
  code: string;
  display_name: string | null;
  portfolios: GrantedPortfolio[];
};

export function TradeApp() {
  // A 401 here redirects to the identity provider from inside the client, so
  // this query either resolves with an identity or the page navigates away.
  const me = useQuery<Me>({ queryKey: ["/auth/me"], queryFn: () => tradeApi.get<Me>("/auth/me") });

  if (me.isLoading) return <Loader />;
  if (me.error) return <Text c="red">Could not load your session.</Text>;

  return (
    <AppShell header={{ height: 48 }} padding="md">
      <AppShell.Header>
        <Group h="100%" px="md" justify="space-between">
          <Text fw={600}>openOMS</Text>
          <Text size="sm" c="dimmed">{me.data?.display_name ?? me.data?.code}</Text>
        </Group>
      </AppShell.Header>
      <AppShell.Main>
        <Routes>
          <Route path="/" element={<TradePage me={me.data!} />} />
          <Route path="/positions" element={<PositionsPage me={me.data!} />} />
          <Route path="*" element={<Navigate to="/" replace />} />
        </Routes>
      </AppShell.Main>
    </AppShell>
  );
}
