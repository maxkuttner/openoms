import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { api } from "../../api/client";
import { useApiMutation, useList } from "../../api/hooks";
import type { Portfolio } from "../../api/types";
import { ACCESS_OPTIONS, accessLabel, accessTableBody, groupSummary, type Access } from "../../groups";
import { ConfirmButton } from "../../components/ConfirmButton";
import { MultiSelect, Select, TextField } from "../../components/form";
import { Modal } from "../../components/overlay";

const PATH = "/admin/groups";

type GroupRow = {
  id: string;
  name: string;
  description: string | null;
  members: string[];
  portfolios: { portfolio_id: string; code: string; access: Access }[];
};

type UserRef = { principal_id: string; code: string; display_name: string | null; email: string | null; status: string };

const label = (u: UserRef) => u.display_name ?? u.email ?? u.code;

/** Per-portfolio access level: unchecked = none. */
function AccessTable({ value, onChange }: { value: Record<string, Access>; onChange: (v: Record<string, Access>) => void }) {
  const portfolios = useList<Portfolio>("/admin/portfolios");
  return (
    <div className="flex flex-col gap-1.5">
      <span className="text-xs text-muted-foreground">Portfolio access</span>
      {(portfolios.data ?? []).map((p) => (
        <div key={p.id} className="flex h-8 items-center gap-3">
          <label className="flex w-48 items-center gap-2">
            <Checkbox checked={p.id in value}
              onCheckedChange={(on) => {
                const next = { ...value };
                if (on === true) next[p.id] = "view";
                else delete next[p.id];
                onChange(next);
              }} />
            {p.code}
          </label>
          {p.id in value && (
            <Select className="w-40" data={ACCESS_OPTIONS} value={value[p.id]}
              onChange={(v) => v && onChange({ ...value, [p.id]: v as Access })} />
          )}
        </div>
      ))}
      {portfolios.data?.length === 0 && <p className="text-muted-foreground">No portfolios yet.</p>}
    </div>
  );
}

/** Create or edit a group: name, members and its portfolio access. */
function GroupModal({ group, onClose }: { group: GroupRow | null; onClose: () => void }) {
  const users = useList<UserRef>("/admin/users");
  const [name, setName] = useState(group?.name ?? "");
  const [description, setDescription] = useState(group?.description ?? "");
  const [members, setMembers] = useState<string[]>(group?.members ?? []);
  const [access, setAccess] = useState<Record<string, Access>>(
    Object.fromEntries((group?.portfolios ?? []).map((p) => [p.portfolio_id, p.access])),
  );
  const save = useApiMutation(
    async () => {
      const body = { name: name.trim(), description: description.trim() || null };
      let id = group?.id;
      if (id) await api.put(`${PATH}/${id}`, body);
      else id = (await api.post<{ id: string }>(PATH, body)).id;
      await api.put(`${PATH}/${id}/members`, members);
      await api.put(`${PATH}/${id}/portfolios`, accessTableBody(access));
    },
    { invalidate: [PATH, "/admin/users"], success: "Group saved", onDone: onClose },
  );
  return (
    <Modal open onClose={onClose} title={group ? `Edit ${group.name}` : "New group"} className="sm:max-w-lg">
      <form onSubmit={(e) => { e.preventDefault(); save.mutate(undefined); }} className="flex flex-col gap-3">
        <TextField label="Name" required value={name} onChange={setName} />
        <TextField label="Description" value={description} onChange={setDescription} />
        <MultiSelect label="Members" value={members} onChange={setMembers}
          data={(users.data ?? []).filter((u) => u.status !== "DISABLED").map((u) => ({ value: u.principal_id, label: label(u) }))} />
        <AccessTable value={access} onChange={setAccess} />
        <div className="flex justify-end gap-2">
          <Button type="button" variant="outline" onClick={onClose}>Cancel</Button>
          <Button type="submit" disabled={save.isPending}>Save</Button>
        </div>
      </form>
    </Modal>
  );
}

/** Groups: who gets which access to which portfolios (a person's highest across groups applies). */
export function GroupsPage() {
  const list = useList<GroupRow>(PATH);
  const [editing, setEditing] = useState<GroupRow | null | "new">(null);
  const remove = useApiMutation((id: string) => api.del(`${PATH}/${id}`), { invalidate: [PATH, "/admin/users"], success: "Group deleted" });
  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-center justify-between">
        <h2 className="text-sm font-semibold">Groups</h2>
        <Button size="sm" onClick={() => setEditing("new")}>New group</Button>
      </div>
      <p className="text-muted-foreground">
        People get portfolio rights through groups: view (viewer), trade (trader) or manage (PM). Someone in several groups
        gets the highest access on each portfolio.
      </p>
      <div className="rounded-md border">
        <Table>
          <TableHeader>
            <TableRow><TableHead>Group</TableHead><TableHead>Portfolio access</TableHead><TableHead>Size</TableHead><TableHead /></TableRow>
          </TableHeader>
          <TableBody>
            {(list.data ?? []).map((g) => (
              <TableRow key={g.id}>
                <TableCell>{g.name}{g.description && <div className="text-xs text-muted-foreground">{g.description}</div>}</TableCell>
                <TableCell>{g.portfolios.length ? g.portfolios.map((p) => `${p.code}: ${accessLabel(p.access)}`).join(", ") : <span className="text-muted-foreground">—</span>}</TableCell>
                <TableCell>{groupSummary(g)}</TableCell>
                <TableCell>
                  <div className="flex justify-end gap-1.5">
                    <Button size="xs" variant="secondary" onClick={() => setEditing(g)}>Edit</Button>
                    <ConfirmButton size="xs" variant="destructive" title={`Delete ${g.name}?`} description="Its members lose the access it gives."
                      confirmLabel="Delete" onConfirm={() => remove.mutate(g.id)}>Delete</ConfirmButton>
                  </div>
                </TableCell>
              </TableRow>
            ))}
            {(list.data ?? []).length === 0 && (
              <TableRow><TableCell colSpan={4} className="text-center text-muted-foreground">No groups yet.</TableCell></TableRow>
            )}
          </TableBody>
        </Table>
      </div>
      {editing && <GroupModal group={editing === "new" ? null : editing} onClose={() => setEditing(null)} />}
    </div>
  );
}
