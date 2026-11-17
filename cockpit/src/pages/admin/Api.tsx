import { useMemo, useState } from "react";
import { Link } from "react-router-dom";
import { useList } from "../../api/hooks";
import { ApiKeysBox } from "../../components/ApiKeysBox";
import { Select } from "../../components/form";
import { userKeys } from "../../components/userKeys";

type UserRef = { principal_id: string; code: string; display_name: string | null; email: string | null; status: string };

const label = (u: UserRef) => (u.display_name ? `${u.display_name} · ${u.email ?? u.code}` : (u.email ?? u.code));

/**
 * Any user's API keys, for admins: pick the user, then the same box as in
 * Settings. A key acts as that user, with the portfolios their groups give them.
 */
export function ApiPage() {
  const users = useList<UserRef>("/admin/users");
  const active = (users.data ?? []).filter((u) => u.status === "ACTIVE");
  const [principalId, setPrincipalId] = useState<string | null>(null);
  const who = active.find((u) => u.principal_id === principalId);
  const source = useMemo(() => (principalId ? userKeys(principalId) : null), [principalId]);

  return (
    <div className="flex max-w-3xl flex-col gap-4">
      <div>
        <h2 className="text-sm font-semibold">API keys</h2>
        <p className="mt-1 text-muted-foreground">
          Manage any user's keys. A key acts as its user: it can trade and view what that user's{" "}
          <Link to="/admin/groups" className="text-primary hover:underline">groups</Link> allow.
        </p>
      </div>
      <Select
        className="max-w-sm"
        label="User"
        placeholder="Choose a user"
        data={active.map((u) => ({ value: u.principal_id, label: label(u) }))}
        value={principalId}
        onChange={setPrincipalId}
      />
      {source && who && (
        <ApiKeysBox
          key={principalId}
          source={source}
          idPrefix="admin-key"
          intro={<>Keys that act as <b>{label(who)}</b>. Use one as <code className="num">Authorization: Bearer key_id.secret</code>.</>}
        />
      )}
    </div>
  );
}
