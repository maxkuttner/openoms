import { ApiKeysBox, type ApiKeysSource } from "../../components/ApiKeysBox";
import { authApi, authErrorText } from "../../login/api";

/** Your own keys, through the session's `/auth/keys`. */
const ownKeys: ApiKeysSource = {
  list: authApi.myKeys,
  create: authApi.createKey,
  revoke: authApi.revokeKey,
  errorText: authErrorText,
};

/** Your own API keys: a key acts as you, with your portfolio access. */
export function ApiKeysPage() {
  return (
    <div className="flex max-w-3xl flex-col gap-4">
      <ApiKeysBox
        source={ownKeys}
        intro={<>A key acts as you, with your portfolio access. Use it as <code className="num">Authorization: Bearer key_id.secret</code>.</>}
      />
    </div>
  );
}
