import { createContext, useContext } from "react";
import type { Me } from "../access";

export type Session = { me: Me; admin: boolean; refresh: () => void };

export const SessionContext = createContext<Session | null>(null);

/** The signed-in session. Only valid under AuthGate. */
export function useSession(): Session {
  const s = useContext(SessionContext);
  if (!s) throw new Error("useSession outside AuthGate");
  return s;
}
