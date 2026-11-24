import { api, ApiError } from "../api/client";
import type { ApiKeysSource } from "./ApiKeysBox";

/** One user's keys, through the admin API (`/admin/users/{id}/keys`). */
export function userKeys(principalId: string): ApiKeysSource {
  const base = `/admin/users/${encodeURIComponent(principalId)}/keys`;
  return {
    list: () => api.get(base),
    create: (name) => api.post(base, { name: name || null }),
    revoke: (keyId) => api.del(`${base}/${encodeURIComponent(keyId)}`),
    errorText: (e) => (e instanceof ApiError ? `${e.status}: ${e.message}` : String(e)),
  };
}
