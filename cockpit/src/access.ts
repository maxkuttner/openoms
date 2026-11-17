// Where a browser session stands, from `GET /auth/me`. Pure, so it is testable
// with node --test; the fetch lives in api/client.ts (sessionAccess).

import { isApprovalPending, isPasswordChangeRequired, isPendingAccount, mustChangePassword } from "./login.ts";

/** The parts of `GET /auth/me` the shell cares about. */
export type SessionInfo = {
  auth_method: string;
  /** `admin`, `user` or `pending`. */
  role?: string;
  must_change_password?: boolean;
  totp_enrolled?: boolean;
  code?: string;
  display_name?: string | null;
};

/** One portfolio the signed-in user has been granted, from `GET /auth/me`. */
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

/** The full `GET /auth/me` answer for a ready session. */
export type Me = SessionInfo & {
  principal_id: string;
  code: string;
  display_name: string | null;
  portfolios: GrantedPortfolio[];
};

export type AccessState = "signed_out" | "pending" | "change_password" | "ready" | "unavailable";

/** `status` 0 means the request never got an answer. `body` is the raw text of a non-2xx reply. */
export function accessFromMe(status: number, me: SessionInfo | null, body = ""): AccessState {
  if (status === 401) return "signed_out";
  if (status === 403) {
    if (isApprovalPending(status, body)) return "pending";
    if (isPasswordChangeRequired(status, body)) return "change_password";
    return "unavailable";
  }
  if (status < 200 || status >= 300 || !me) return "unavailable";
  if (isPendingAccount(me)) return "pending";
  if (mustChangePassword(me)) return "change_password";
  return "ready";
}

export function isAdmin(me: SessionInfo | null | undefined): boolean {
  return me?.role === "admin";
}
