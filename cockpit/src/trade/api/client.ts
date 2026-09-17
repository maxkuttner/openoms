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
      window.location.href = `/auth/login?return_to=${encodeURIComponent("/trade/")}`;
      // Never resolves; the navigation is already underway.
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
