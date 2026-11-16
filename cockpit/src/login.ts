// Pure helpers for the shared sign-in UI (cockpit/src/login/), used by both
// the cockpit and the trader app.

/** `GET /auth/methods`. */
export type AuthMethods = {
  local: boolean;
  sso: boolean;
  sso_label: string;
  /** The sign-up form may be shown. */
  signup?: boolean;
  /** No admin exists yet: offer "Create the admin account". */
  first_run?: boolean;
};

export type Method = "sso" | "local";

/** The sign-in methods to offer, in display order. Disabled ones never appear. */
export function visibleMethods(m: AuthMethods): Method[] {
  const out: Method[] = [];
  if (m.sso) out.push("sso");
  if (m.local) out.push("local");
  return out;
}

export type Step = "totp" | "totp_enroll" | "done";

/** The step after a password login, enrolment, password change or TOTP check. */
export function nextStepFromLoginResponse(body: unknown): Step {
  const next = (body as { next?: unknown } | null | undefined)?.next;
  return next === "totp" || next === "totp_enroll" ? next : "done";
}

/** `/auth/me` says the account is waiting for an admin's approval. */
export function isPendingAccount(me: { role?: string } | null | undefined): boolean {
  return me?.role === "pending";
}

/** Is this response the 403 a pending account gets everywhere? */
export function isApprovalPending(status: number, body: string): boolean {
  if (status !== 403) return false;
  try {
    return JSON.parse(body)?.error === "approval_pending";
  } catch {
    return false;
  }
}

/** What the sign-in page offers besides signing in. */
export function loginMode(m: AuthMethods): "create_admin" | "signin" {
  return m.local && m.first_run ? "create_admin" : "signin";
}

export const PENDING_MESSAGE = "Your account is waiting for an administrator to approve it.";

/**
 * What the "Two-step sign-in" settings section offers: nothing for an SSO
 * session (the provider owns its MFA), else turning the authenticator on or off.
 */
export function twoStepSection(
  me: { auth_method?: string; totp_enrolled?: boolean } | null | undefined,
): "hidden" | "enroll" | "disable" {
  if (me?.auth_method !== "local") return "hidden";
  return me.totp_enrolled ? "disable" : "enroll";
}

/** Text for the access page a refused sign-in lands on. */
export function accessMessage(reason: string | null, _hint?: string | null): string {
  switch (reason) {
    case "no_account":
      return "There is no account for this sign-in. Ask the OMS admin to add you.";
    default:
      return "Access not granted. Ask the OMS admin to add you.";
  }
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

export type MfaStep = "totp" | "sso";

/**
 * What a 403 mfa_required asks for: a local session confirms with its
 * authenticator; any other (OIDC) signs in again with SSO. A TOTP call
 * answering totp_not_enrolled / not_a_local_session also means SSO.
 */
export function mfaStepFor(authMethod: string | null, errorCode?: string | null): MfaStep {
  if (errorCode === "totp_not_enrolled" || errorCode === "not_a_local_session") return "sso";
  return authMethod === "local" ? "totp" : "sso";
}

export function mfaPromptText(step: MfaStep): string {
  return step === "totp" ? "Confirm with your authenticator code to continue." : "Sign in again with SSO to continue.";
}

/** User-facing text for a failed sign-in call, by the server's error code. */
export function authErrorMessage(status: number, code: string | null, reason: string | null): string {
  switch (code) {
    case "invalid_credentials":
      return "Wrong email or password.";
    case "too_many_attempts":
      return "Too many attempts. Wait a few minutes and try again.";
    case "weak_password":
      return reason ?? "Choose a stronger password.";
    case "unauthenticated":
      return "Your sign-in expired. Sign in again.";
    case "invalid_code":
      return "That code didn't work. Try again.";
    case "same_password":
      return "Choose a password different from the current one.";
    case "signup_disabled":
      return "Sign-up is turned off. Ask the OMS admin to add you.";
    case "email_taken":
      return "An account with this email already exists. Sign in instead.";
    case "invalid_email":
      return "Enter a valid email address.";
    case "mfa_required_by_policy":
      return "This OMS requires two-step sign-in, so it can't be turned off.";
  }
  if (status === 429) return "Too many attempts. Wait a few minutes and try again.";
  return "Something went wrong. Try again.";
}
