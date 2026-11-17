import { useState } from "react";
import { X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { Separator } from "@/components/ui/separator";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { cn } from "@/lib/utils";
import { ConfirmButton } from "../../components/ConfirmButton";
import { CopyText, Notice, Spinner } from "../../components/common";
import { MultiSelect, Select, TextField } from "../../components/form";
import { Drawer, Modal } from "../../components/overlay";
import { Pill } from "../../components/status";
import { api } from "../../api/client";
import { useList, useApiMutation } from "../../api/hooks";
import type { ApiKeyRecord, Portfolio } from "../../api/types";
import { ACCOUNT_ROLE_OPTIONS, accountRoleLabel, pendingUsers } from "../../users";
import { accessLabel } from "../../groups";

const PATH = "/admin/users";

type UserRow = {
  principal_id: string;
  code: string;
  display_name: string | null;
  email: string | null;
  account_role: "admin" | "user" | "pending";
  groups: { id: string; name: string }[];
  portfolios: { portfolio_id: string; code: string; access: string }[];
  sign_in: string | null;
  status: string;
  admin: boolean;
};

function who(u: UserRow) {
  return u.display_name ?? u.email ?? u.code;
}

type GroupRef = { id: string; name: string };

/** Pick groups: they decide the user's portfolio access. */
function GroupPicker({ value, onChange }: { value: string[]; onChange: (v: string[]) => void }) {
  const groups = useList<GroupRef>("/admin/groups");
  return (
    <MultiSelect label="Groups" description="Portfolio access comes from groups (Groups page)."
      data={(groups.data ?? []).map((g) => ({ value: g.id, label: g.name }))} value={value} onChange={onChange} />
  );
}

export function UsersPage() {
  const list = useList<UserRow>(PATH);
  const [adding, setAdding] = useState(false);
  const [editing, setEditing] = useState<UserRow | null>(null);
  const [approving, setApproving] = useState<UserRow | null>(null);
  const [keysFor, setKeysFor] = useState<UserRow | null>(null);

  const reload = [PATH, "/admin/setup-status"];
  const act = useApiMutation(
    ({ u, fn }: { u: UserRow; fn: (u: UserRow) => Promise<unknown> }) => fn(u),
    { invalidate: reload, success: "Done" },
  );
  const run = (u: UserRow, fn: (u: UserRow) => Promise<unknown>) => act.mutate({ u, fn });
  const pending = pendingUsers(list.data ?? []);

  const dash = <span className="text-muted-foreground">—</span>;

  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-center justify-between">
        <h2 className="text-sm font-semibold">Users</h2>
        <Button size="sm" onClick={() => setAdding(true)}>Add user</Button>
      </div>

      {pending.length > 0 && (
        <div className="flex flex-col gap-2 rounded-md border border-warn/40 bg-warn/5 p-3">
          <div className="font-semibold text-warn">Waiting for approval</div>
          {pending.map((u) => (
            <div key={u.principal_id} className="flex items-center justify-between gap-2">
              <span>{who(u)}{u.email && u.display_name ? ` (${u.email})` : ""}{u.sign_in ? ` — via ${u.sign_in}` : ""}</span>
              <div className="flex gap-1.5">
                <Button size="xs" onClick={() => setApproving(u)}>Approve</Button>
                <ConfirmButton size="xs" variant="destructive" title={`Reject ${who(u)}?`} description="The account is disabled."
                  confirmLabel="Reject" onConfirm={() => run(u, (x) => api.post(`${PATH}/${x.principal_id}/reject`, {}))}>
                  Reject
                </ConfirmButton>
              </div>
            </div>
          ))}
        </div>
      )}

      {list.isLoading ? (
        <Spinner />
      ) : (
        <div className="rounded-md border">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>User</TableHead>
                <TableHead>Role</TableHead>
                <TableHead>Groups</TableHead>
                <TableHead>Portfolio access</TableHead>
                <TableHead>Sign-in</TableHead>
                <TableHead>Status</TableHead>
                <TableHead>Admin</TableHead>
                <TableHead />
              </TableRow>
            </TableHeader>
            <TableBody>
              {(list.data ?? []).map((u) => (
                <TableRow key={u.principal_id} className={cn(u.status === "DISABLED" && "text-muted-foreground")}>
                  <TableCell>
                    {who(u)}
                    {u.email && u.display_name && <div className="text-xs text-muted-foreground">{u.email}</div>}
                  </TableCell>
                  <TableCell>{accountRoleLabel(u.account_role)}</TableCell>
                  <TableCell>{u.groups.length ? u.groups.map((g) => g.name).join(", ") : dash}</TableCell>
                  <TableCell>
                    {u.portfolios.length === 0 ? dash : u.portfolios.map((p) => `${p.code} (${accessLabel(p.access)})`).join(", ")}
                  </TableCell>
                  <TableCell>{u.sign_in ?? dash}</TableCell>
                  <TableCell><Pill tone={u.status === "ACTIVE" ? "buy" : "muted"}>{u.status}</Pill></TableCell>
                  <TableCell>
                    <Switch
                      aria-label="Admin"
                      checked={u.admin}
                      disabled={u.status !== "ACTIVE"}
                      onCheckedChange={(admin) => run(u, (x) => api.put(`${PATH}/${x.principal_id}/admin`, { admin }))}
                    />
                  </TableCell>
                  <TableCell>
                    <div className="flex justify-end gap-1.5">
                      {u.status === "ACTIVE" && <Button size="xs" variant="secondary" onClick={() => setEditing(u)}>Groups</Button>}
                      {u.status === "ACTIVE" && <Button size="xs" variant="secondary" onClick={() => setKeysFor(u)}>API keys</Button>}
                      {u.status === "ACTIVE" && (
                        <ConfirmButton size="xs" variant="destructive" title={`Disable ${who(u)}?`}
                          description="Their sessions and API keys stop working." confirmLabel="Disable"
                          onConfirm={() => run(u, (x) => api.post(`${PATH}/${x.principal_id}/disable`, {}))}>
                          Disable
                        </ConfirmButton>
                      )}
                      {u.status === "DISABLED" && (
                        <Button size="xs" variant="secondary" onClick={() => run(u, (x) => api.post(`${PATH}/${x.principal_id}/enable`, {}))}>Enable</Button>
                      )}
                    </div>
                  </TableCell>
                </TableRow>
              ))}
              {(list.data ?? []).length === 0 && (
                <TableRow><TableCell colSpan={8} className="text-center text-muted-foreground">No users yet.</TableCell></TableRow>
              )}
            </TableBody>
          </Table>
        </div>
      )}

      <AddUserModal opened={adding} onClose={() => setAdding(false)} />
      {editing && <EditAccessModal user={editing} onClose={() => setEditing(null)} />}
      {approving && <ApproveModal user={approving} onClose={() => setApproving(null)} />}

      <Drawer open={!!keysFor} onClose={() => setKeysFor(null)} title={keysFor ? `${who(keysFor)} — API keys` : ""}>
        {keysFor && <KeysSection userId={keysFor.principal_id} />}
      </Drawer>
    </div>
  );
}

/** Open WebUI's "Add user": a local account with a password you pass on yourself. */
function AddUserModal({ opened, onClose }: { opened: boolean; onClose: () => void }) {
  const [name, setName] = useState("");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [role, setRole] = useState<string>("user");
  const create = useApiMutation(
    () => api.post(PATH, { name: name.trim(), email: email.trim(), password, role }),
    {
      invalidate: [PATH, "/admin/setup-status"],
      success: "User added",
      onDone: () => { setName(""); setEmail(""); setPassword(""); onClose(); },
    },
  );
  return (
    <Modal open={opened} onClose={onClose} title="Add user">
      <form onSubmit={(e) => { e.preventDefault(); create.mutate(undefined); }} className="flex flex-col gap-3">
        <TextField label="Name" required value={name} onChange={setName} />
        <TextField label="Email" required value={email} onChange={setEmail} />
        <TextField label="Password" type="password" description="At least 15 characters. Give it to them yourself; they can change it under Settings."
          required value={password} onChange={setPassword} autoComplete="new-password" />
        <Select label="Role" data={ACCOUNT_ROLE_OPTIONS} value={role} onChange={(v) => v && setRole(v)} />
        <div className="flex justify-end gap-2">
          <Button type="button" variant="outline" onClick={onClose}>Cancel</Button>
          <Button type="submit" disabled={create.isPending}>Add user</Button>
        </div>
      </form>
    </Modal>
  );
}

/** Approve a pending account: it becomes a user, in the chosen groups. */
function ApproveModal({ user, onClose }: { user: UserRow; onClose: () => void }) {
  const [groups, setGroups] = useState<string[]>([]);
  const approve = useApiMutation(
    () => api.post(`${PATH}/${user.principal_id}/approve`, { groups }),
    { invalidate: [PATH], success: "Approved", onDone: onClose },
  );
  return (
    <Modal open onClose={onClose} title={`Approve ${who(user)}`}>
      <form onSubmit={(e) => { e.preventDefault(); approve.mutate(undefined); }} className="flex flex-col gap-3">
        <GroupPicker value={groups} onChange={setGroups} />
        <div className="flex justify-end gap-2">
          <Button type="button" variant="outline" onClick={onClose}>Cancel</Button>
          <Button type="submit" disabled={approve.isPending}>Approve</Button>
        </div>
      </form>
    </Modal>
  );
}

/** Which groups a user is in. */
function EditAccessModal({ user, onClose }: { user: UserRow; onClose: () => void }) {
  const [groups, setGroups] = useState<string[]>(user.groups.map((g) => g.id));
  const save = useApiMutation(
    () => api.put(`${PATH}/${user.principal_id}/groups`, groups),
    { invalidate: [PATH, "/admin/groups"], success: "Groups updated", onDone: onClose },
  );
  return (
    <Modal open onClose={onClose} title={`Groups — ${who(user)}`}
      footer={<>
        <Button variant="outline" onClick={onClose}>Cancel</Button>
        <Button onClick={() => save.mutate(undefined)} disabled={save.isPending}>Save</Button>
      </>}>
      <GroupPicker value={groups} onChange={setGroups} />
    </Modal>
  );
}

function KeysSection({ userId }: { userId: string }) {
  const base = `${PATH}/${userId}/keys`;
  const keys = useList<ApiKeyRecord>(base);
  const [name, setName] = useState("");
  const [created, setCreated] = useState<ApiKeyRecord | null>(null);
  const register = useApiMutation((n: string) => api.post<ApiKeyRecord>(base, { name: n || null }), {
    invalidate: [base], success: "Key created", onDone: (k) => { setName(""); setCreated(k as ApiKeyRecord); },
  });
  const revoke = useApiMutation((keyId: string) => api.del(`${base}/${keyId}`), { invalidate: [base], success: "Key revoked" });
  return (
    <div className="flex flex-col gap-3">
      {created?.secret && (
        <Notice tone="info" title="Copy this secret now — it is shown only once">
          <CopyText value={`${created.key_id}:${created.secret}`} />
        </Notice>
      )}
      <div className="flex items-end gap-2">
        <TextField label="New key name (optional)" value={name} onChange={setName} />
        <Button onClick={() => register.mutate(name)} disabled={register.isPending}>Register key</Button>
      </div>
      <Separator />
      <div className="rounded-md border">
        <Table>
          <TableHeader><TableRow><TableHead>Key id</TableHead><TableHead>Name</TableHead><TableHead /></TableRow></TableHeader>
          <TableBody>
            {(keys.data ?? []).map((k) => (
              <TableRow key={k.key_id}>
                <TableCell className="num">{k.key_id}</TableCell>
                <TableCell>{k.name ?? <span className="text-muted-foreground">—</span>}</TableCell>
                <TableCell className="text-right">
                  <ConfirmButton size="icon-xs" variant="ghost" className="text-sell" aria-label="Revoke key"
                    title="Revoke key?" confirmLabel="Revoke" onConfirm={() => revoke.mutate(k.key_id)}>
                    <X />
                  </ConfirmButton>
                </TableCell>
              </TableRow>
            ))}
            {(keys.data ?? []).length === 0 && (
              <TableRow><TableCell colSpan={3} className="text-center text-muted-foreground">No keys.</TableCell></TableRow>
            )}
          </TableBody>
        </Table>
      </div>
    </div>
  );
}
