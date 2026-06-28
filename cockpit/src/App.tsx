import { useState } from "react";
import { AppShell, Group, Title, NavLink, Button, Modal, TextInput, Stack, Text } from "@mantine/core";
import { Routes, Route, NavLink as RouterNavLink, Navigate, useLocation } from "react-router-dom";
import { getToken, setToken } from "./api/client";
import { PrincipalsPage } from "./pages/Principals";
import { PortfoliosPage } from "./pages/Portfolios";
import { AccountsPage } from "./pages/Accounts";
import { BrokerConnectionsPage } from "./pages/BrokerConnections";
import { RiskLimitsPage } from "./pages/RiskLimits";
import { BlotterPage } from "./pages/Blotter";

const NAV = [
  { to: "/principals", label: "Principals" },
  { to: "/portfolios", label: "Portfolios" },
  { to: "/accounts", label: "Accounts" },
  { to: "/broker-connections", label: "Broker connections" },
  { to: "/risk-limits", label: "Risk limits" },
  { to: "/blotter", label: "Blotter" },
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

export function App() {
  const [tokenOpen, setTokenOpen] = useState(false);
  const { pathname } = useLocation();
  return (
    <AppShell header={{ height: 56 }} navbar={{ width: 220, breakpoint: "sm" }} padding="md">
      <AppShell.Header>
        <Group h="100%" px="md" justify="space-between">
          <Title order={4}>OMS Cockpit</Title>
          <Button variant="light" size="xs" onClick={() => setTokenOpen(true)}>
            {getToken() ? "Token set" : "Set token"}
          </Button>
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
        </Routes>
      </AppShell.Main>
      <TokenModal opened={tokenOpen} onClose={() => setTokenOpen(false)} />
    </AppShell>
  );
}
