import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Alert, AppShell, Box, Button, Container, Group, Loader, Stack, Text, Tooltip } from "@mantine/core";
import { Routes, Route, Navigate } from "react-router-dom";
import { tradeApi, onLoginUnavailable, API_BASE } from "./api/client";
import { TradePage } from "./pages/Trade";

/// Reachability of the OMS, polled on its own rather than inferred from whichever
/// screen happens to be mounted. `/health` is unauthenticated, so this answers
/// "can I reach the server", not "am I still signed in" — an expired session
/// navigates to sign-in on its own and does not belong in this indicator.
///
/// Fetched directly because `/health` answers plain `OK`, which is not JSON and
/// would throw in the shared client's parser.
function ConnectionDot() {
  const health = useQuery({
    queryKey: ["/health"],
    queryFn: async () => {
      const res = await fetch(`${API_BASE}/health`);
      if (!res.ok) throw new Error(String(res.status));
      return true;
    },
    refetchInterval: 10_000,
    retry: false,
  });

  const { color, label } = health.isError
    ? { color: "var(--mantine-color-offer-6)", label: "No connection to the OMS" }
    : health.isFetching
      ? { color: "var(--mantine-color-yellow-6)", label: "Checking the connection" }
      : { color: "var(--mantine-color-depth-6)", label: "Connected" };

  return (
    <Tooltip label={label}>
      <Box
        aria-label={label}
        style={{ width: 8, height: 8, borderRadius: "50%", background: color, flexShrink: 0 }}
      />
    </Tooltip>
  );
}

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
        <Group h="100%" px="md" justify="space-between" wrap="nowrap">
          <Text fw={700}>openOMS</Text>
          <Group gap="xs" wrap="nowrap">
            <ConnectionDot />
            <Text size="sm" c="dimmed">
              {me.data?.display_name ?? me.data?.code}
            </Text>
          </Group>
        </Group>
      </AppShell.Header>
      <AppShell.Main>
        <Routes>
          <Route path="/" element={<TradePage me={me.data!} />} />
          <Route path="*" element={<Navigate to="/" replace />} />
        </Routes>
      </AppShell.Main>
    </AppShell>
  );
}
