// The sign-in screens' own fetch: session cookie only, never an admin bearer,
// so the same module is safe in both the cockpit and the trader app.

import { authErrorMessage, type AuthMethods } from "../login";

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

async function call<T>(method: "GET" | "POST" | "DELETE", path: string, body?: unknown): Promise<T> {
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
  login: (email: string, password: string) => call<unknown>("POST", "/auth/local/login", { email, password }),
  signup: (name: string, email: string, password: string) =>
    call<{ role: string; next: string }>("POST", "/auth/local/signup", { name, email, password }),
  totp: (code: string) => call<unknown>("POST", "/auth/local/totp", { code }),
  totpEnroll: () => call<{ otpauth_uri: string }>("POST", "/auth/local/totp/enroll", {}),
  totpConfirm: (code: string) => call<{ recovery_codes: string[] }>("POST", "/auth/local/totp/confirm", { code }),
  totpDisable: (code: string) => call<void>("POST", "/auth/local/totp/disable", { code }),
  changePassword: (current_password: string, new_password: string) =>
    call<unknown>("POST", "/auth/local/password", { current_password, new_password }),
  me: () => call<SessionInfo>("GET", "/auth/me"),
  myKeys: () => call<OwnKey[]>("GET", "/auth/keys"),
  createKey: (name: string) => call<OwnKey>("POST", "/auth/keys", { name: name || null }),
  revokeKey: (keyId: string) => call<void>("DELETE", `/auth/keys/${encodeURIComponent(keyId)}`),
  logout: () => call<void>("POST", "/auth/logout", {}),
};

/** One of the signed-in user's API keys; `secret` only right after creation. */
export type OwnKey = { key_id: string; name: string | null; created_at: string; secret?: string | null };

/** The parts of `GET /auth/me` the sign-in screens care about. */
export type SessionInfo = {
  auth_method: string;
  /** `admin`, `user` or `pending`. */
  role?: string;
  must_change_password?: boolean;
  totp_enrolled?: boolean;
};


/** User-facing text for a failed sign-in call. */
export function authErrorText(err: unknown): string {
  if (!(err instanceof AuthError)) return "Can't reach the OMS. Try again.";
  return authErrorMessage(err.status, err.code, err.reason);
}

/** The signed-in session's sign-in method ("local", "oidc", …), or null. */
export async function sessionAuthMethod(): Promise<string | null> {
  return call<{ auth_method?: string }>("GET", "/auth/me").then((m) => m?.auth_method ?? null, () => null);
}

/** Full-page navigation to the OIDC flow (a 303 to the provider, which fetch can't follow). */
export function startSso(returnTo: string) {
  window.location.href = `/auth/oidc/login?return_to=${encodeURIComponent(returnTo)}`;
}
