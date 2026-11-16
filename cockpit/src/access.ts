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
