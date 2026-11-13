// The trade app authenticates by session cookie and NOTHING else.
//
// It deliberately does not reuse cockpit/src/api/client.ts, which can carry
// the deprecated shared admin password as a bearer.

import { loginHref } from "../../login";
import { TRADE_HOME } from "../../login/api";

export const API_BASE = import.meta.env.DEV ? "/api" : "";

export class ApiError extends Error {
  constructor(public status: number, message: string) {
    super(message);
  }
}

// Where an expired or missing session sends the browser: the shared sign-in
// page, coming back here afterwards.
let navigatingToLogin = false;

function goToLogin(): void {
  if (navigatingToLogin) return;
  navigatingToLogin = true;
  window.location.href = loginHref(TRADE_HOME, window.location.pathname + window.location.search);
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(`${API_BASE}${path}`, {
    method,
    credentials: "same-origin",
    headers: body === undefined ? {} : { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });

  if (!res.ok) {
    // 401 means the session is gone.
    if (res.status === 401) {
      goToLogin();
      // Never resolves: the navigation is underway. Resolving or rejecting
      // here would flash an error on screen first.
      return new Promise<T>(() => {});
    }
    const text = await res.text().catch(() => "");
    throw new ApiError(res.status, text || `${method} ${path} failed (${res.status})`);
  }
  if (res.status === 204) return undefined as T;
  const text = await res.text();
  return text ? (JSON.parse(text) as T) : (undefined as T);
}

export const tradeApi = {
  get: <T>(path: string) => request<T>("GET", path),
  post: <T>(path: string, body: unknown) => request<T>("POST", path, body),
  del: (path: string) => request<void>("DELETE", path),
};
