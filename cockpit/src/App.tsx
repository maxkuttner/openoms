import { useState, lazy, Suspense } from "react";
import { AppShell, Group, NavLink, Button, Modal, TextInput, Stack, Text, Loader } from "@mantine/core";
import { Routes, Route, NavLink as RouterNavLink, Navigate, useLocation } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import { getToken, setToken } from "./api/client";
import { Logo } from "./components/Logo";
import { PrincipalsPage } from "./pages/Principals";
import { PortfoliosPage } from "./pages/Portfolios";
import { AccountsPage } from "./pages/Accounts";
import { BrokerConnectionsPage } from "./pages/BrokerConnections";
import { RiskLimitsPage } from "./pages/RiskLimits";
import { BlotterPage } from "./pages/Blotter";

// Scalar's bundle is heavy — only load it when the API docs page is opened.
const ApiDocsPage = lazy(() => import("./pages/ApiDocs").then((m) => ({ default: m.ApiDocsPage })));

const NAV = [
  { to: "/principals", label: "Principals" },
  { to: "/portfolios", label: "Portfolios" },
  { to: "/accounts", label: "Accounts" },
  { to: "/broker-connections", label: "Broker connections" },
  { to: "/risk-limits", label: "Risk limits" },
  { to: "/blotter", label: "Blotter" },
  { to: "/api-docs", label: "API docs" },
];

function TokenModal({ opened, onClose }: { opened: boolean; onClose: () => void }) {
  const [value, setValue] = useState(getToken());
  return (
    <Modal opened={opened} onClose={onClose} title="Admin API token" centered>
      <Stack>
        <Text size="sm" c="dimmed">
          The OMS admin token (OMS_ADMIN_TOKEN). Leave blank in dev when
          OMS_ADMIN_AUTH_ENABLED=false. Stored in this browser only.
        </Text>
        <TextInput
          label="Bearer token"
          placeholder="token…"
          value={value}
          onChange={(e) => setValue(e.currentTarget.value)}
        />
        <Group justify="flex-end">
          <Button variant="default" onClick={onClose}>Cancel</Button>
          <Button onClick={() => { setToken(value.trim()); onClose(); }}>Save</Button>
        </Group>
      </Stack>
    </Modal>
  );
}

function ConnectionDot() {
  const { data: ok } = useQuery({
    queryKey: ["health"],
    queryFn: async () => (await fetch("/api/health")).ok,
    refetchInterval: 10000,
  });
  return (
    <Group gap={6}>
      <span style={{ width: 7, height: 7, borderRadius: "50%", background: ok ? "#22ae6c" : "#e0524d" }} />
      <Text fz={12} c="#9aa3af">{ok ? "connected" : "offline"}</Text>
    </Group>
  );
}

export function App() {
  const [tokenOpen, setTokenOpen] = useState(false);
  const { pathname } = useLocation();
  return (
    <AppShell header={{ height: 56 }} navbar={{ width: 220, breakpoint: "sm" }} padding="md">
      <AppShell.Header style={{ background: "#0d1014", borderColor: "#2a2f38" }}>
        <Group h="100%" px="md" justify="space-between">
          <Logo />
          <Group gap="md">
            <ConnectionDot />
            <Button variant="light" size="xs" onClick={() => setTokenOpen(true)}>
              {getToken() ? "Token set" : "Set token"}
            </Button>
          </Group>
        </Group>
      </AppShell.Header>
      <AppShell.Navbar p="xs">
        {NAV.map((n) => (
          <NavLink
            key={n.to}
            component={RouterNavLink}
            to={n.to}
            label={n.label}
            active={pathname.startsWith(n.to)}
          />
        ))}
      </AppShell.Navbar>
      <AppShell.Main>
        <Routes>
          <Route path="/" element={<Navigate to="/principals" replace />} />
          <Route path="/principals" element={<PrincipalsPage />} />
          <Route path="/portfolios" element={<PortfoliosPage />} />
          <Route path="/accounts" element={<AccountsPage />} />
          <Route path="/broker-connections" element={<BrokerConnectionsPage />} />
          <Route path="/risk-limits" element={<RiskLimitsPage />} />
          <Route path="/blotter" element={<BlotterPage />} />
          <Route
            path="/api-docs"
            element={
              <Suspense fallback={<Loader />}>
                <ApiDocsPage />
              </Suspense>
            }
          />
        </Routes>
      </AppShell.Main>
      <TokenModal opened={tokenOpen} onClose={() => setTokenOpen(false)} />
    </AppShell>
  );
}
