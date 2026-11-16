import { useCallback, useEffect, useState, type ReactNode } from "react";
import { goToLogin, onMfaRequired, sessionAccess } from "../api/client";
import { isAdmin, type AccessState, type Me } from "../access";
import { Frame } from "../login/Frame";
import { MfaPrompt } from "../login/MfaPrompt";
import { ForcedPasswordChange, PendingScreen } from "../login/ChangePassword";
import { LiveUpdatesProvider } from "../trade/api/liveUpdates";
import { Spinner } from "../components/common";
import { SessionContext } from "./session";

/**
 * The one sign-in gate: checking → signed out (to the login page) → pending
 * → forced password change → ready. Being signed in is enough; the admin
 * capability only decides which pages exist (see nav.ts).
 */
export function AuthGate({ children }: { children: ReactNode }) {
  const [state, setState] = useState<AccessState | "checking">("checking");
  const [me, setMe] = useState<Me | null>(null);

  const load = useCallback(() => {
    void sessionAccess().then(({ state, me }) => {
      if (state === "signed_out") return goToLogin();
      setMe(me as Me | null);
      setState(state);
    });
  }, []);
  useEffect(load, [load]);

  if (state === "checking" || state === "signed_out") {
    return <div className="grid h-screen place-items-center bg-background"><Spinner className="size-6" /></div>;
  }
  if (state === "pending") return <PendingScreen />;
  if (state === "change_password") return <ForcedPasswordChange onDone={load} />;
  if (state === "unavailable" || !me) {
    return <Frame title="openOMS"><p>The OMS isn't responding. Try again.</p></Frame>;
  }

  return (
    <SessionContext.Provider value={{ me, admin: isAdmin(me), refresh: load }}>
      <MfaPrompt register={onMfaRequired} />
      <LiveUpdatesProvider>{children}</LiveUpdatesProvider>
    </SessionContext.Provider>
  );
}
