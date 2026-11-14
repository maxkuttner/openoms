// Thin typed fetch wrapper over the OMS admin API.
// In dev, requests go to "/api/*" which Vite proxies to the OMS (see vite.config.ts).
//
// Authentication is the session cookie. The one exception is the deprecated
// shared admin password, accepted by the server only until the first named
// admin exists and only to create that admin: it is held in memory for this
// page (never stored) and sent as a bearer.

import { COCKPIT_HOME } from "../login/api";
import { loginHref } from "../login";

// Dev runs behind Vite's proxy, which strips this prefix (see vite.config.ts).
// A production bundle is served by the OMS itself at /cockpit/, so it is
// same-origin and calls the API with no prefix at all.
export const API_BASE = import.meta.env.DEV ? "/api" : "";

let legacyPassword: string | null = null;

/** Use the shared admin password for this page's requests (null to stop). */
export function setLegacyPassword(password: string | null): void {
  legacyPassword = password;
}

export function isLegacySession(): boolean {
  return legacyPassword !== null;
}

export class ApiError extends Error {
  constructor(public status: number, message: string) {
    super(message);
  }
}

// Opens the authenticator prompt for a 403 mfa_required; resolves true once
// the session has passed it. Installed by the app shell.
type MfaPrompt = () => Promise<boolean>;
let mfaPrompt: MfaPrompt | null = null;

export function onMfaRequired(prompt: MfaPrompt): () => void {
  mfaPrompt = prompt;
  return () => {
    if (mfaPrompt === prompt) mfaPrompt = null;
  };
}

/** Send the browser to the sign-in page, coming back here afterwards. */
export function goToLogin(): void {
  window.location.href = loginHref(COCKPIT_HOME, window.location.pathname + window.location.search);
}

function send(method: string, path: string, body?: unknown): Promise<Response> {
  return fetch(`${API_BASE}${path}`, {
    method,
    credentials: "same-origin",
    headers: {
      "Content-Type": "application/json",
      ...(legacyPassword ? { Authorization: `Bearer ${legacyPassword}` } : {}),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
}

/** Is this a 403 asking for a fresh authenticator code? */
async function isMfaRequired(res: Response): Promise<boolean> {
  if (res.status !== 403) return false;
  const text = await res.clone().text().catch(() => "");
  try {
    return JSON.parse(text)?.error === "mfa_required";
  } catch {
    return false;
  }
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  let res = await send(method, path, body);

  if (mfaPrompt && (await isMfaRequired(res)) && (await mfaPrompt())) {
    res = await send(method, path, body);
  }

  if (!res.ok) {
    if (res.status === 401) {
      setLegacyPassword(null);
      goToLogin();
      // Never resolves: the page is navigating away.
      return new Promise<T>(() => {});
    }
    const text = await res.text().catch(() => "");
    throw new ApiError(res.status, text || `${method} ${path} failed (${res.status})`);
  }
  if (res.status === 204) return undefined as T;
  const text = await res.text();
  return text ? (JSON.parse(text) as T) : (undefined as T);
}

/**
 * Who may open the console: "admin", "forbidden" (signed in, but no admin
 * capability), "signed_out", or "unavailable" (server error). Does not redirect.
 */
export async function consoleAccess(): Promise<"admin" | "forbidden" | "signed_out" | "unavailable"> {
  const res = await send("GET", "/admin/users");
  if (res.ok) return "admin";
  if (res.status === 403) return "forbidden";
  return res.status === 401 ? "signed_out" : "unavailable";
}

export const api = {
  get: <T>(path: string) => request<T>("GET", path),
  post: <T>(path: string, body: unknown) => request<T>("POST", path, body),
  put: <T>(path: string, body: unknown) => request<T>("PUT", path, body),
  patch: <T>(path: string, body: unknown) => request<T>("PATCH", path, body),
  del: (path: string) => request<void>("DELETE", path),
};
