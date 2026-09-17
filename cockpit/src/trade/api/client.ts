// The trade app authenticates by session cookie and NOTHING else.
//
// It deliberately does not reuse cockpit/src/api/client.ts: that one attaches
// `Authorization: Bearer <admin token>` from localStorage. On a browser where an
// operator has signed into the cockpit, reusing it would put the admin token on
// every trader request — handing any bug on this surface the full admin API.
// fetch sends same-origin cookies by default, so the session needs no header.

export const API_BASE = import.meta.env.DEV ? "/api" : "";

export class ApiError extends Error {
  constructor(public status: number, message: string) {
    super(message);
  }
}

// Where an expired or missing session sends the browser. A full-page navigation
// is required because /auth/login answers 303 to the identity provider, which
// fetch cannot usefully follow.
export const LOGIN_PATH = `/auth/login?return_to=${encodeURIComponent("/trade/")}`;

// /trade/ and /auth/* are mounted under the same OIDC condition, but they can
// still come apart: a provider-discovery failure at boot leaves the app served
// and the auth routes absent. Navigating to /auth/login then lands the trader
// on a bare 404 with no explanation and no way back — so the app is told
// instead, and says what happened.
type LoginUnavailableListener = () => void;
let loginUnavailableListener: LoginUnavailableListener | null = null;

export function onLoginUnavailable(listener: LoginUnavailableListener): () => void {
  loginUnavailableListener = listener;
  return () => {
    if (loginUnavailableListener === listener) loginUnavailableListener = null;
  };
}

let navigatingToLogin = false;

async function goToLogin(): Promise<void> {
  if (navigatingToLogin) return;
  navigatingToLogin = true;

  try {
    // `redirect: "manual"` keeps the 303 from being followed into the identity
    // provider: a live endpoint answers with an opaque redirect (status 0), an
    // unmounted one answers 404. Only the 404 changes what we do.
    const probe = await fetch(LOGIN_PATH, { redirect: "manual" });
    if (probe.status === 404) {
      navigatingToLogin = false;
      loginUnavailableListener?.();
      return;
    }
  } catch {
    // The probe itself failed (offline, proxy). Fall through: a real navigation
    // gives the browser's own error page, which is at least explicable.
  }

  window.location.href = LOGIN_PATH;
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(`${API_BASE}${path}`, {
    method,
    headers: body === undefined ? {} : { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });

  if (!res.ok) {
    // 401 means the session is gone. A full-page navigation is required because
    // /auth/login answers 303 to the identity provider, which fetch cannot follow
    // usefully.
    if (res.status === 401) {
      void goToLogin();
      // Never resolves: either the navigation is underway, or the app has been
      // told sign-in is unavailable and is showing that instead. Resolving or
      // rejecting here would put a second, less informative error on screen.
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
