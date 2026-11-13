import { useEffect, useState } from "react";
import { AppShell, Box, Group, NavLink, Button, Center, Text, Loader, Anchor, Modal, Stack } from "@mantine/core";
import { Routes, Route, NavLink as RouterNavLink, Navigate, useLocation, useNavigate } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import { API_BASE, consoleAccess, goToLogin, isLegacySession, onMfaRequired, setLegacyPassword } from "./api/client";
import { authApi, TRADE_HOME } from "./login/api";
import { Totp } from "./login/Totp";
import { Frame } from "./login/Frame";
import { Logo } from "./components/Logo";
import { JobsIndicator } from "./components/JobsIndicator";
import { LoginPage } from "./pages/Login";
import { UsersPage } from "./pages/Users";
import { SignInPage } from "./pages/SignIn";
import { SetupChecklist } from "./components/SetupChecklist";
import { PortfoliosPage } from "./pages/Portfolios";
import { AccountsPage } from "./pages/Accounts";
import { BrokerConnectionsPage } from "./pages/BrokerConnections";
import { RiskLimitsPage } from "./pages/RiskLimits";
import { StrategiesPage } from "./pages/Strategies";
import { BlotterPage } from "./pages/Blotter";
import { InstrumentsPage } from "./pages/Instruments";
import { DataFeedsPage } from "./pages/DataFeeds";
import { ApiPage } from "./pages/Api";
import { palette } from "./palette";

const NAV = [
  { to: "/users", label: "Users" },
  { to: "/sign-in", label: "Sign-in" },
  { to: "/tokens", label: "API" },
  { to: "/portfolios", label: "Portfolios" },
  { to: "/accounts", label: "Accounts" },
  { to: "/broker-connections", label: "Broker connections" },
  { to: "/data-feeds", label: "Data feeds" },
  { to: "/risk-limits", label: "Risk limits" },
  { to: "/strategies", label: "Strategies" },
  { to: "/blotter", label: "Blotter" },
  { to: "/instruments", label: "Instruments" },
];

function ConnectionDot() {
  const { data: ok } = useQuery({
    queryKey: ["health"],
    queryFn: async () => (await fetch(`${API_BASE}/health`)).ok,
    refetchInterval: 10000,
  });
  return (
    <Group gap={6}>
      <span style={{ width: 7, height: 7, borderRadius: "50%", background: ok ? palette.buy : palette.red }} />
      <Text fz={12} c={palette.muted}>{ok ? "connected" : "offline"}</Text>
    </Group>
  );
}

/** Home: straight to Users. The setup checklist is a banner above every page
 *  (see Console) until all its steps are done, so it never vanishes on navigation. */
function Home() {
  return <Navigate to="/users" replace />;
}

/** The authenticated console. */
function Console({ onLogout }: { onLogout: () => void }) {
  const { pathname } = useLocation();
  return (
    <AppShell header={{ height: 56 }} navbar={{ width: 220, breakpoint: "sm" }} padding="md">
      <AppShell.Header style={{ background: palette.black, borderColor: palette.border }}>
        <Group h="100%" px="md" justify="space-between">
          <Logo />
          <Group gap="md">
            <JobsIndicator />
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
        <Group px="sm" py="xs">
          {/* The docs live on the public site now — they are what someone reads *before*
              installing, so they do not belong behind a login. */}
          <Anchor
            href="https://openoms.dev/"
            target="_blank"
            rel="noreferrer"
            fz="sm"
          >
            Docs ↗
          </Anchor>
        </Group>
      </AppShell.Navbar>
      <AppShell.Main>
        <Box mb="md">
          <SetupChecklist />
        </Box>
        <Routes>
          <Route path="/" element={<Home />} />
          <Route path="/users" element={<UsersPage />} />
          <Route path="/sign-in" element={<SignInPage />} />
          <Route path="/tokens" element={<ApiPage />} />
          <Route path="/portfolios" element={<PortfoliosPage />} />
          <Route path="/accounts" element={<AccountsPage />} />
          <Route path="/broker-connections" element={<BrokerConnectionsPage />} />
          <Route path="/data-feeds" element={<DataFeedsPage />} />
          <Route path="/risk-limits" element={<RiskLimitsPage />} />
          <Route path="/strategies" element={<StrategiesPage />} />
          <Route path="/blotter" element={<BlotterPage />} />
          <Route path="/instruments" element={<InstrumentsPage />} />
        </Routes>
      </AppShell.Main>
    </AppShell>
  );
}

type Status = "checking" | "admin" | "forbidden";

/** Signed in, but without the admin capability. */
function NoAccess() {
  return (
    <Frame title="Admin console">
      <Stack gap={12}>
        <Text fz={13}>You don't have access to the admin console.</Text>
        <Anchor href={TRADE_HOME}>Open the trader app</Anchor>
      </Stack>
    </Frame>
  );
}

/**
 * The authenticator prompt behind every 403 mfa_required: the request that
 * hit it waits, and is retried once the code is accepted.
 */
function MfaPrompt() {
  const [pending, setPending] = useState<((ok: boolean) => void) | null>(null);
  useEffect(
    () => onMfaRequired(() => new Promise<boolean>((resolve) => setPending(() => resolve))),
    [],
  );
  const finish = (ok: boolean) => {
    pending?.(ok);
    setPending(null);
  };
  return (
    <Modal opened={!!pending} onClose={() => finish(false)} title="Confirm" centered>
      {pending && <Totp prompt="Confirm with your authenticator code to continue." onDone={() => finish(true)} />}
    </Modal>
  );
}

/** Auth gate: the console needs a session holding the admin capability. */
export function App() {
  const { pathname } = useLocation();
  const navigate = useNavigate();
  const [status, setStatus] = useState<Status>("checking");
  const onLoginPage = pathname === "/login";

  useEffect(() => {
    if (onLoginPage) return;
    consoleAccess()
      .catch(() => "signed_out" as const)
      .then((a) => (a === "signed_out" ? goToLogin() : setStatus(a)));
  }, [onLoginPage]);

  if (onLoginPage) {
    return <LoginPage onLegacySignedIn={() => { setStatus("admin"); navigate("/users", { replace: true }); }} />;
  }

  if (status === "checking") {
    return (
      <Center h="100vh" bg={palette.black}>
        <Loader />
      </Center>
    );
  }
  if (status === "forbidden") return <NoAccess />;

  return (
    <>
      <MfaPrompt />
      <Console
        onLogout={async () => {
          if (!isLegacySession()) await authApi.logout().catch(() => undefined);
          setLegacyPassword(null);
          goToLogin();
        }}
      />
    </>
  );
}
