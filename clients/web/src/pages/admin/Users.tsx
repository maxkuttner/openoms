import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { Badge } from "@/components/ui/badge";
import { Input } from "@/components/ui/input";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { cn } from "@/lib/utils";
import { ConfirmButton } from "../../components/ConfirmButton";
import { ApiKeysBox } from "../../components/ApiKeysBox";
import { userKeys } from "../../components/userKeys";
import { Spinner } from "../../components/common";
import { MultiSelect, Select, TextField } from "../../components/form";
import { Modal } from "../../components/overlay";
import { Pill } from "../../components/status";
import { api } from "../../api/client";
import { useList, useApiMutation } from "../../api/hooks";
import type { Portfolio } from "../../api/types";
import { ACCOUNT_ROLE_OPTIONS, accountRoleLabel, filterUsers, groupBadges, pendingUsers, portfolioCount } from "../../users";
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
  const [approving, setApproving] = useState<UserRow | null>(null);
  const [query, setQuery] = useState("");
  const [selId, setSelId] = useState<string | null>(null);

  const act = useApiMutation(
    ({ u, fn }: { u: UserRow; fn: (u: UserRow) => Promise<unknown> }) => fn(u),
    { invalidate: [PATH], success: "Done" },
  );
  const run = (u: UserRow, fn: (u: UserRow) => Promise<unknown>) => act.mutate({ u, fn });
  const all = list.data ?? [];
  const pending = pendingUsers(all);
  const rows = filterUsers(all.filter((u) => u.account_role !== "pending"), query);
  const selected = all.find((u) => u.principal_id === selId) ?? rows[0] ?? null;
  const dash = <span className="text-muted-foreground">—</span>;

  return (
    <div className="flex flex-col gap-3">
      {pending.length > 0 && (
        <div className="flex flex-col gap-2 rounded-md border border-warn/40 bg-warn/5 p-3">
          <div className="font-semibold text-warn">{pending.length} waiting for approval</div>
          {pending.map((u) => (
            <div key={u.principal_id} className="flex flex-wrap items-center justify-between gap-2">
              <span className="min-w-0 truncate">{who(u)}{u.email && u.display_name ? ` (${u.email})` : ""}{u.sign_in ? ` — via ${u.sign_in}` : ""}</span>
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

      <div className="flex flex-col gap-4 lg:flex-row lg:items-start">
        <section aria-label="Users" className="flex min-w-0 flex-col gap-2 rounded-md border bg-card p-3 lg:flex-[1_1_0]">
          <div className="flex items-center gap-2">
            <Input aria-label="Search users" placeholder="Search name or email…" value={query}
              onChange={(e) => setQuery(e.currentTarget.value)} className="h-8 flex-1" />
            <Button size="sm" onClick={() => setAdding(true)}>Add user</Button>
          </div>
          {list.isLoading ? <Spinner /> : (
            <Table className="table-fixed">
              <TableHeader>
                <TableRow>
                  <TableHead className="w-[38%]">User</TableHead>
                  <TableHead className="w-[14%]">Role</TableHead>
                  <TableHead>Groups</TableHead>
                  <TableHead className="w-[18%]">Access</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {rows.map((u) => {
                  const badges = groupBadges(u.groups.map((g) => g.name));
                  return (
                    <TableRow key={u.principal_id} onClick={() => setSelId(u.principal_id)}
                      data-state={selected?.principal_id === u.principal_id ? "selected" : undefined}
                      className={cn("cursor-pointer", u.status === "DISABLED" && "text-muted-foreground")}>
                      <TableCell className="overflow-hidden">
                        <div className="truncate">{who(u)}</div>
                        {u.email && u.display_name && <div className="truncate text-xs text-muted-foreground">{u.email}</div>}
                      </TableCell>
                      <TableCell>{u.status === "DISABLED" ? "Disabled" : accountRoleLabel(u.account_role)}</TableCell>
                      <TableCell className="overflow-hidden">
                        <div className="flex gap-1 overflow-hidden">
                          {badges.shown.length === 0 && dash}
                          {badges.shown.map((n) => <Badge key={n} variant="outline" className="max-w-32 truncate">{n}</Badge>)}
                          {badges.more && <Badge variant="outline" className="border-dashed text-muted-foreground">{badges.more}</Badge>}
                        </div>
                      </TableCell>
                      <TableCell className="whitespace-nowrap text-muted-foreground"
                        title={u.portfolios.map((p) => `${p.code} (${accessLabel(p.access)})`).join(", ")}>
                        {portfolioCount(u.portfolios.length)}
                      </TableCell>
                    </TableRow>
                  );
                })}
                {rows.length === 0 && (
                  <TableRow><TableCell colSpan={4} className="text-center text-muted-foreground">{query ? "No match." : "No users yet."}</TableCell></TableRow>
                )}
              </TableBody>
            </Table>
          )}
        </section>
        {selected && <UserEditor key={selected.principal_id} user={selected} run={run} />}
      </div>

      <AddUserModal opened={adding} onClose={() => setAdding(false)} />
      {approving && <ApproveModal user={approving} onClose={() => setApproving(null)} />}
    </div>
  );
}

/** One user: groups, the access they add up to, API keys, account. */
function UserEditor({ user, run }: { user: UserRow; run: (u: UserRow, fn: (u: UserRow) => Promise<unknown>) => void }) {
  const active = user.status === "ACTIVE";
  return (
    <section aria-label="User editor" className="flex min-w-0 flex-col gap-3 rounded-md border bg-card p-4 lg:flex-[1_1_0]">
      <div className="flex items-center gap-3">
        <span className="grid size-9 shrink-0 place-items-center rounded-full bg-accent font-semibold uppercase">{who(user).slice(0, 1)}</span>
        <div className="min-w-0 flex-1">
          <div className="truncate font-semibold">{who(user)}</div>
          <div className="truncate text-xs text-muted-foreground">
            {[user.email, user.sign_in && `via ${user.sign_in}`].filter(Boolean).join(" · ") || user.code}
          </div>
        </div>
        <Pill tone={active ? "buy" : "muted"}>{user.status}</Pill>
      </div>
      <Tabs defaultValue={active ? "groups" : "account"}>
        <TabsList>
          {active && <TabsTrigger value="groups">Groups</TabsTrigger>}
          <TabsTrigger value="access">Access</TabsTrigger>
          {active && <TabsTrigger value="keys">API keys</TabsTrigger>}
          <TabsTrigger value="account">Account</TabsTrigger>
        </TabsList>
        {active && <TabsContent value="groups" className="pt-2"><GroupsTab user={user} /></TabsContent>}
        <TabsContent value="access" className="pt-2">
          <p className="mb-2 text-xs text-muted-foreground">What this user can reach. Change it on the group or the portfolio.</p>
          {user.portfolios.length === 0 ? (
            <p className="text-sm text-muted-foreground">No portfolio access.</p>
          ) : (
            <Table>
              <TableHeader><TableRow><TableHead>Portfolio</TableHead><TableHead>Level</TableHead></TableRow></TableHeader>
              <TableBody>
                {user.portfolios.map((p) => (
                  <TableRow key={p.portfolio_id}>
                    <TableCell className="font-mono">{p.code}</TableCell>
                    <TableCell><Badge variant="outline">{accessLabel(p.access)}</Badge></TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </TabsContent>
        {active && (
          <TabsContent value="keys" className="pt-2">
            <ApiKeysBox source={userKeys(user.principal_id)} idPrefix="user-key"
              intro={<>Keys that act as {who(user)}, with the portfolios their groups give them.</>} />
          </TabsContent>
        )}
        <TabsContent value="account" className="flex flex-col pt-2">
          <div className="flex items-center justify-between gap-3 border-t py-3">
            <div>Admin<div className="text-xs text-muted-foreground">Admins manage everything here; trading still needs portfolio access.</div></div>
            <Switch aria-label="Admin" checked={user.admin} disabled={!active}
              onCheckedChange={(admin) => run(user, (x) => api.put(`${PATH}/${x.principal_id}/admin`, { admin }))} />
          </div>
          <div className="flex items-center justify-between gap-3 border-t py-3">
            {active ? (
              <>
                <div>Disable account<div className="text-xs text-muted-foreground">Their sessions and API keys stop working.</div></div>
                <ConfirmButton size="sm" variant="destructive" title={`Disable ${who(user)}?`}
                  description="Their sessions and API keys stop working." confirmLabel="Disable"
                  onConfirm={() => run(user, (x) => api.post(`${PATH}/${x.principal_id}/disable`, {}))}>
                  Disable
                </ConfirmButton>
              </>
            ) : (
              <>
                <div>Enable account<div className="text-xs text-muted-foreground">They can sign in again.</div></div>
                <Button size="sm" variant="secondary" onClick={() => run(user, (x) => api.post(`${PATH}/${x.principal_id}/enable`, {}))}>Enable</Button>
              </>
            )}
          </div>
        </TabsContent>
      </Tabs>
    </section>
  );
}

/** Which groups a user is in; saved as a whole. */
function GroupsTab({ user }: { user: UserRow }) {
  const [groups, setGroups] = useState<string[]>(user.groups.map((g) => g.id));
  const dirty = groups.join() !== user.groups.map((g) => g.id).join();
  const save = useApiMutation(
    () => api.put(`${PATH}/${user.principal_id}/groups`, groups),
    { invalidate: [PATH, "/admin/groups"], success: "Groups updated" },
  );
  return (
    <div className="flex flex-col gap-3">
      <GroupPicker value={groups} onChange={setGroups} />
      <div className="flex justify-end gap-2">
        <Button size="sm" variant="outline" disabled={!dirty} onClick={() => setGroups(user.groups.map((g) => g.id))}>Reset</Button>
        <Button size="sm" disabled={!dirty || save.isPending} onClick={() => save.mutate(undefined)}>Save</Button>
      </div>
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
      invalidate: [PATH],
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
