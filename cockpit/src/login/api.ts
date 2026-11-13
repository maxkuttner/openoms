// The sign-in screens' own fetch: session cookie only, never an admin bearer,
// so the same module is safe in both the cockpit and the trader app.

import type { AuthMethods } from "../login";

export const API_BASE = import.meta.env.DEV ? "/api" : "";

// Each app's router base. In dev both run on vite with no path split (see
// main.tsx / trade/main.tsx).
export const COCKPIT_HOME = import.meta.env.DEV ? "/" : "/cockpit/";
export const TRADE_HOME = import.meta.env.DEV ? "/trade.html/" : "/trade/";

export class AuthError extends Error {
  constructor(public status: number, public code: string | null, public reason: string | null) {
    super(code ?? `request failed (${status})`);
  }
}

async function call<T>(method: "GET" | "POST", path: string, body?: unknown): Promise<T> {
  const res = await fetch(`${API_BASE}${path}`, {
    method,
    credentials: "same-origin",
    headers: body === undefined ? {} : { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text().catch(() => "");
  let json: unknown = undefined;
  try {
    json = text ? JSON.parse(text) : undefined;
  } catch {
    json = undefined;
  }
  if (!res.ok) {
    const j = json as { error?: string; reason?: string } | undefined;
    throw new AuthError(res.status, j?.error ?? null, j?.reason ?? null);
  }
  return json as T;
}

export const authApi = {
  methods: () => call<AuthMethods>("GET", "/auth/methods"),
  preview: (token: string) => call<InvitePreview>("GET", `/auth/invite/${encodeURIComponent(token)}/preview`),
  login: (email: string, password: string) => call<unknown>("POST", "/auth/local/login", { email, password }),
  enroll: (token: string, password: string) => call<unknown>("POST", "/auth/local/enroll", { token, password }),
  totp: (code: string) => call<unknown>("POST", "/auth/local/totp", { code }),
  totpEnroll: () => call<{ otpauth_uri: string }>("POST", "/auth/local/totp/enroll", {}),
  totpConfirm: (code: string) => call<{ recovery_codes: string[] }>("POST", "/auth/local/totp/confirm", { code }),
  logout: () => call<void>("POST", "/auth/logout", {}),
};

export type InvitePreview = {
  valid: boolean;
  email_hint: string | null;
  role: string | null;
  portfolios: string[];
  methods: string[];
  expires_at: string | null;
};

/** User-facing text for a failed sign-in call. */
export function authErrorText(err: unknown): string {
  if (!(err instanceof AuthError)) return "Can't reach the OMS. Try again.";
  switch (err.code) {
    case "invalid_credentials":
      return "Wrong email or password.";
    case "too_many_attempts":
      return "Too many attempts. Wait a few minutes and try again.";
    case "weak_password":
      return err.reason ?? "Choose a stronger password.";
    default:
      if (err.status === 401) return "That code didn't work. Try again.";
      if (err.status === 429) return "Too many attempts. Wait a few minutes and try again.";
      return "Something went wrong. Try again.";
  }
}

/** Full-page navigation to the OIDC flow (a 303 to the provider, which fetch can't follow). */
export function startSso(returnTo: string) {
  window.location.href = `/auth/oidc/login?return_to=${encodeURIComponent(returnTo)}`;
}
