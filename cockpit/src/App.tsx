import { Navigate, Route, Routes } from "react-router-dom";
import { APP_HOME } from "./login/api";
import { LoginPage } from "./login/LoginPage";
import { AccountSettings } from "./login/AccountSettings";
import { AuthGate } from "./shell/AuthGate";
import { Shell } from "./shell/Shell";
import { AdminPanel } from "./shell/AdminPanel";
import { useSession } from "./shell/session";
import { TradePage } from "./pages/trading/Trade";
import { PositionsPage } from "./pages/trading/Positions";
import { AccessPage } from "./pages/trading/Access";
import { UsersPage } from "./pages/admin/Users";
import { GroupsPage } from "./pages/admin/Groups";
import { SignInPage } from "./pages/admin/SignIn";
import { ApiPage } from "./pages/admin/Api";
import { PortfoliosPage } from "./pages/admin/Portfolios";
import { AccountsPage } from "./pages/admin/Accounts";
import { StrategiesPage } from "./pages/admin/Strategies";
import { RiskLimitsPage } from "./pages/admin/RiskLimits";
import { InstrumentsPage } from "./pages/admin/Instruments";
import { BrokerConnectionsPage } from "./pages/admin/BrokerConnections";
import { DataFeedsPage } from "./pages/admin/DataFeeds";
import { BlotterPage } from "./pages/admin/Blotter";

function Trade() {
  const { me } = useSession();
  return <TradePage me={me} />;
}

function Positions() {
  const { me } = useSession();
  return <PositionsPage me={me} />;
}

/** The whole app. Sign-in and the access page render without a session. */
export function App() {
  return (
    <Routes>
      <Route path="/login" element={<LoginPage home={APP_HOME} />} />
      <Route path="/access" element={<AccessPage />} />
      <Route element={<AuthGate><Shell /></AuthGate>}>
        <Route index element={<Trade />} />
        <Route path="positions" element={<Positions />} />
        <Route path="settings" element={<AccountSettings />} />
        <Route path="admin" element={<AdminPanel />}>
          <Route index element={<Navigate to="users" replace />} />
          <Route path="users" element={<UsersPage />} />
          <Route path="groups" element={<GroupsPage />} />
          <Route path="sign-in" element={<SignInPage />} />
          <Route path="api" element={<ApiPage />} />
          <Route path="portfolios" element={<PortfoliosPage />} />
          <Route path="accounts" element={<AccountsPage />} />
          <Route path="strategies" element={<StrategiesPage />} />
          <Route path="risk-limits" element={<RiskLimitsPage />} />
          <Route path="instruments" element={<InstrumentsPage />} />
          <Route path="broker-connections" element={<BrokerConnectionsPage />} />
          <Route path="data-feeds" element={<DataFeedsPage />} />
          <Route path="blotter" element={<BlotterPage />} />
          <Route path="*" element={<Navigate to="users" replace />} />
        </Route>
        <Route path="*" element={<Navigate to="/" replace />} />
      </Route>
    </Routes>
  );
}
