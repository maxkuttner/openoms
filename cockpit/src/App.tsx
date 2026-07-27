import { useEffect, useState, lazy, Suspense } from "react";
import { AppShell, Group, NavLink, Button, Center, Text, Loader } from "@mantine/core";
import { Routes, Route, NavLink as RouterNavLink, Navigate, useLocation } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import { api, setToken } from "./api/client";
import { Logo } from "./components/Logo";
import { LoginPage } from "./pages/Login";
import { PrincipalsPage } from "./pages/Principals";
import { PortfoliosPage } from "./pages/Portfolios";
import { AccountsPage } from "./pages/Accounts";
import { BrokerConnectionsPage } from "./pages/BrokerConnections";
import { RiskLimitsPage } from "./pages/RiskLimits";
import { BlotterPage } from "./pages/Blotter";
import { ReconciliationPage } from "./pages/Reconciliation";
import { InstrumentsPage } from "./pages/Instruments";
import { DataFeedsPage } from "./pages/DataFeeds";
import { ApiPage } from "./pages/Api";

// Heavy bundles (Scalar, Mermaid) — only load when their doc page is opened.
const ApiDocsPage = lazy(() => import("./pages/ApiDocs").then((m) => ({ default: m.ApiDocsPage })));
const ArchitecturePage = lazy(() =>
  import("./pages/Architecture").then((m) => ({ default: m.ArchitecturePage })),
);

const NAV = [
  { to: "/principals", label: "Principals" },
  { to: "/tokens", label: "API" },
  { to: "/portfolios", label: "Portfolios" },
  { to: "/accounts", label: "Accounts" },
  { to: "/broker-connections", label: "Broker connections" },
  { to: "/data-feeds", label: "Data feeds" },
  { to: "/risk-limits", label: "Risk limits" },
  { to: "/blotter", label: "Blotter" },
  { to: "/instruments", label: "Instruments" },
  { to: "/reconciliation", label: "Reconciliation" },
];

// Grouped under a "Docs" section in the navbar.
const DOCS_NAV = [
  { to: "/docs/architecture", label: "Architecture" },
  { to: "/api-docs", label: "API docs" },
];

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

/** The authenticated console. */
function Console({ onLogout }: { onLogout: () => void }) {
  const { pathname } = useLocation();
  return (
    <AppShell header={{ height: 56 }} navbar={{ width: 220, breakpoint: "sm" }} padding="md">
      <AppShell.Header style={{ background: "#0d1014", borderColor: "#2a2f38" }}>
        <Group h="100%" px="md" justify="space-between">
          <Logo />
          <Group gap="md">
            <ConnectionDot />
            <Button variant="subtle" size="xs" color="gray" onClick={onLogout}>
              Log out
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
        <NavLink label="Docs" defaultOpened childrenOffset={16}>
          {DOCS_NAV.map((n) => (
            <NavLink
              key={n.to}
              component={RouterNavLink}
              to={n.to}
              label={n.label}
              active={pathname.startsWith(n.to)}
            />
          ))}
        </NavLink>
      </AppShell.Navbar>
      <AppShell.Main>
        <Routes>
          <Route path="/" element={<Navigate to="/principals" replace />} />
          <Route path="/principals" element={<PrincipalsPage />} />
          <Route path="/tokens" element={<ApiPage />} />
          <Route path="/portfolios" element={<PortfoliosPage />} />
          <Route path="/accounts" element={<AccountsPage />} />
          <Route path="/broker-connections" element={<BrokerConnectionsPage />} />
          <Route path="/data-feeds" element={<DataFeedsPage />} />
          <Route path="/risk-limits" element={<RiskLimitsPage />} />
          <Route path="/blotter" element={<BlotterPage />} />
          <Route path="/instruments" element={<InstrumentsPage />} />
          <Route path="/reconciliation" element={<ReconciliationPage />} />
          <Route
            path="/docs/architecture"
            element={
              <Suspense fallback={<Loader />}>
                <ArchitecturePage />
              </Suspense>
            }
          />
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
    </AppShell>
  );
}

type Status = "checking" | "authed" | "login";

/**
 * Auth gate. Probes an authed endpoint rather than just checking for a token, so
 * dev with OMS_ADMIN_AUTH_ENABLED=false still enters straight through (no token
 * needed), while a real deployment shows the login screen on 401/403.
 */
export function App() {
  const [status, setStatus] = useState<Status>("checking");
  const [checking, setChecking] = useState(false);
  const [error, setError] = useState<string | undefined>();

  async function probe(): Promise<boolean> {
    try {
      await api.get("/admin/principals");
      return true;
    } catch {
      return false;
    }
  }

  useEffect(() => {
    probe().then((ok) => setStatus(ok ? "authed" : "login"));
  }, []);

  useEffect(() => {
    const onUnauthorized = () => setStatus("login");
    window.addEventListener("oms:unauthorized", onUnauthorized);
    return () => window.removeEventListener("oms:unauthorized", onUnauthorized);
  }, []);

  if (status === "checking") {
    return (
      <Center h="100vh" bg="#0d1014">
        <Loader />
      </Center>
    );
  }

  if (status === "login") {
    return (
      <LoginPage
        checking={checking}
        error={error}
        onSubmit={async (token) => {
          setToken(token);
          setError(undefined);
          setChecking(true);
          const ok = await probe();
          setChecking(false);
          if (ok) setStatus("authed");
          else setError("Invalid token");
        }}
      />
    );
  }

  return <Console onLogout={() => { setToken(""); setStatus("login"); }} />;
}
