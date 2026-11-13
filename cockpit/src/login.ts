// Pure helpers for the shared sign-in UI (cockpit/src/login/), used by both
// the cockpit and the trader app.

/** `GET /auth/methods`. */
export type AuthMethods = {
  local: boolean;
  sso: boolean;
  sso_label: string;
  legacy_admin: boolean;
};

export type Method = "sso" | "local" | "legacy_admin";

/** The sign-in methods to offer, in display order. Disabled ones never appear. */
export function visibleMethods(m: AuthMethods): Method[] {
  const out: Method[] = [];
  if (m.sso) out.push("sso");
  if (m.local) out.push("local");
  if (m.legacy_admin) out.push("legacy_admin");
  return out;
}

export type Step = "totp" | "totp_enroll" | "done";

/** The step after a password login, enrolment or TOTP check. */
export function nextStepFromLoginResponse(body: unknown): Step {
  const next = (body as { next?: unknown } | null | undefined)?.next;
  return next === "totp" || next === "totp_enroll" ? next : "done";
}

/** Text for the access page a refused sign-in lands on; unknown reasons read as no invite. */
export function accessMessage(reason: string | null, hint: string | null): string {
  switch (reason) {
    case "expired":
      return "This invite expired. Ask for a new one.";
    case "revoked":
      return "This invite was revoked.";
    case "email_mismatch":
      return `This invite is for ${hint ?? "another address"}. Sign in with that address, or ask for a new invite.`;
    // No longer sent by the server; kept for old bookmarked access URLs.
    case "email_unverified":
      return "Your sign-in provider didn't confirm your email address. Use another sign-in method or ask for a new invite.";
    case "pending_approval":
      return "Your sign-in is waiting for an administrator to approve it.";
    default:
      return "Access not granted. Ask the OMS admin for an invite.";
  }
}

const ROLE_LABELS: Record<string, string> = { pm: "PM", trader: "Trader", viewer: "Viewer" };

/** "You're invited as {Role} on {portfolios}." An invite with no role is a named admin's. */
export function inviteSummary(p: { role: string | null; portfolios: string[] }): string {
  const role = p.role === null ? "Admin" : (ROLE_LABELS[p.role] ?? p.role);
  return p.portfolios.length ? `You're invited as ${role} on ${p.portfolios.join(", ")}.` : `You're invited as ${role}.`;
}

/** The base32 secret inside an `otpauth://` URI, for typing in by hand. */
export function secretFromOtpauth(uri: string): string | null {
  const q = uri.indexOf("?");
  if (!uri.startsWith("otpauth://") || q < 0) return null;
  return new URLSearchParams(uri.slice(q + 1)).get("secret");
}

/** A same-site path to go back to after sign-in, or null for anything else. */
export function safeReturnTo(raw: string | null): string | null {
  if (!raw || !raw.startsWith("/") || raw.startsWith("//") || raw.startsWith("/\\")) return null;
  return raw;
}

/** The login page of an app whose router lives under `appBase` (e.g. "/cockpit/"). */
export function loginHref(appBase: string, returnTo: string): string {
  return `${appBase}login?return_to=${encodeURIComponent(returnTo)}`;
}

/** Invite links carry the token in the fragment, so it never reaches a server log or referrer. */
export function inviteTokenFromHash(hash: string): string | null {
  const t = hash.replace(/^#/, "");
  return t || null;
}
