import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Alert, AppShell, Box, Button, Container, Group, Loader, Stack, Text, Tooltip } from "@mantine/core";
import { Routes, Route, Navigate, useLocation } from "react-router-dom";
import { AccessPage } from "./pages/Access";
import { tradeApi, API_BASE, onMfaRequired } from "./api/client";
import { MfaPrompt } from "../login/MfaPrompt";
import { LoginPage } from "../login/LoginPage";
import { InvitePreview } from "../login/InvitePreview";
import { TRADE_HOME } from "../login/api";
import { TradePage } from "./pages/Trade";
import { LiveUpdatesProvider, useLive } from "./api/liveUpdates";

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
  const live = useLive();

  const { color, label } = health.isError
    ? { color: "var(--mantine-color-offer-6)", label: "No connection to the OMS" }
    : health.isFetching
      ? { color: "var(--mantine-color-yellow-6)", label: "Checking the connection" }
      : { color: "var(--mantine-color-depth-6)", label: live ? "Live updates" : "Connected" };

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
  // Sign-in, invite and access pages have no session to load: render them
  // before the identity query, which would bounce to sign-in.
  const { pathname } = useLocation();
  if (pathname === "/access") return <AccessPage />;
  if (pathname === "/login") return <LoginPage home={TRADE_HOME} />;
  if (pathname === "/invite") return <InvitePreview />;
  return <SignedInTradeApp />;
}

function SignedInTradeApp() {
  // A 401 here sends the browser to the sign-in page from inside the client,
  // so this query either resolves with an identity or the page navigates away.
  const me = useQuery<Me>({ queryKey: ["/auth/me"], queryFn: () => tradeApi.get<Me>("/auth/me") });

  if (me.isLoading) return <Loader />;
  if (me.error) return <Text c="red">Could not load your session.</Text>;

  return (
    <LiveUpdatesProvider>
      <MfaPrompt register={onMfaRequired} />
      <AppShell header={{ height: 48 }} padding="md">
        <AppShell.Header>
          <Group h="100%" px="md" justify="space-between" wrap="nowrap">
            <Text fw={700} ff="var(--mantine-font-family-headings)">
              openOMS
            </Text>
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
    </LiveUpdatesProvider>
  );
}
