import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Alert, AppShell, Button, Container, Group, Loader, Stack, Text } from "@mantine/core";
import { Routes, Route, Navigate } from "react-router-dom";
import { tradeApi, onLoginUnavailable } from "./api/client";
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
  // Set when the client finds /auth/login missing: /trade/ and /auth/* are
  // mounted under the same condition, but a provider-discovery failure can
  // leave this app served with no auth routes behind it. Without this the
  // trader goes /trade/ -> 401 -> /auth/login -> bare 404, with nothing said
  // and no way back.
  const [loginUnavailable, setLoginUnavailable] = useState(false);
  useEffect(() => onLoginUnavailable(() => setLoginUnavailable(true)), []);

  // A 401 here redirects to the identity provider from inside the client, so
  // this query either resolves with an identity or the page navigates away —
  // unless sign-in itself is unavailable, handled above.
  const me = useQuery<Me>({ queryKey: ["/auth/me"], queryFn: () => tradeApi.get<Me>("/auth/me") });

  if (loginUnavailable) {
    return (
      <Container size="sm" py="xl">
        <Alert color="orange" title="Sign-in is unavailable">
          <Stack gap="sm" align="flex-start">
            <Text size="sm">
              This screen needs a signed-in session, but the OMS is not serving its sign-in
              routes: <code>/auth/login</code> answered 404. That usually means the server
              started without an identity provider configured, or its discovery call is
              failing. Nothing you can do here will sign you in — an operator has to fix the
              server. Your orders and positions are unaffected.
            </Text>
            <Button variant="default" onClick={() => window.location.reload()}>
              Try again
            </Button>
          </Stack>
        </Alert>
      </Container>
    );
  }

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
