import { useState } from "react";
import { Button, Checkbox, Group as Row, Modal, MultiSelect, Select, Stack, Table, Text, TextInput, Title } from "@mantine/core";
import { api } from "../api/client";
import { useApiMutation, useList } from "../api/hooks";
import type { Portfolio } from "../api/types";
import { ACCESS_OPTIONS, accessLabel, accessTableBody, groupSummary, type Access } from "../groups";

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
    <Stack gap={6}>
      <Text size="sm" fw={500}>Portfolio access</Text>
      {(portfolios.data ?? []).map((p) => (
        <Row key={p.id} gap="sm">
          <Checkbox label={p.code} w={200} checked={p.id in value}
            onChange={(e) => {
              const next = { ...value };
              if (e.currentTarget.checked) next[p.id] = "view";
              else delete next[p.id];
              onChange(next);
            }} />
          {p.id in value && (
            <Select size="xs" w={160} allowDeselect={false} data={ACCESS_OPTIONS} value={value[p.id]}
              onChange={(v) => v && onChange({ ...value, [p.id]: v as Access })} />
          )}
        </Row>
      ))}
      {portfolios.data?.length === 0 && <Text size="sm" c="dimmed">No portfolios yet.</Text>}
    </Stack>
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
    <Modal opened onClose={onClose} title={group ? `Edit ${group.name}` : "New group"} centered size="lg">
      <form onSubmit={(e) => { e.preventDefault(); save.mutate(undefined); }}>
        <Stack>
          <TextInput label="Name" required value={name} onChange={(e) => setName(e.currentTarget.value)} />
          <TextInput label="Description" value={description} onChange={(e) => setDescription(e.currentTarget.value)} />
          <MultiSelect label="Members" searchable value={members} onChange={setMembers}
            data={(users.data ?? []).filter((u) => u.status !== "DISABLED").map((u) => ({ value: u.principal_id, label: label(u) }))} />
          <AccessTable value={access} onChange={setAccess} />
          <Row justify="flex-end">
            <Button variant="default" onClick={onClose}>Cancel</Button>
            <Button type="submit" loading={save.isPending}>Save</Button>
          </Row>
        </Stack>
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
    <Stack>
      <Row justify="space-between">
        <Title order={3}>Groups</Title>
        <Button onClick={() => setEditing("new")}>New group</Button>
      </Row>
      <Text size="sm" c="dimmed">
        People get portfolio rights through groups: view (viewer), trade (trader) or manage (PM). Someone in several groups
        gets the highest access on each portfolio.
      </Text>
      <Table striped withTableBorder>
        <Table.Thead>
          <Table.Tr><Table.Th>Group</Table.Th><Table.Th>Portfolio access</Table.Th><Table.Th>Size</Table.Th><Table.Th /></Table.Tr>
        </Table.Thead>
        <Table.Tbody>
          {(list.data ?? []).map((g) => (
            <Table.Tr key={g.id}>
              <Table.Td>{g.name}{g.description && <Text size="xs" c="dimmed">{g.description}</Text>}</Table.Td>
              <Table.Td>{g.portfolios.length ? g.portfolios.map((p) => `${p.code}: ${accessLabel(p.access)}`).join(", ") : <Text c="dimmed">—</Text>}</Table.Td>
              <Table.Td>{groupSummary(g)}</Table.Td>
              <Table.Td>
                <Row gap="xs" justify="flex-end">
                  <Button size="xs" variant="light" onClick={() => setEditing(g)}>Edit</Button>
                  <Button size="xs" variant="light" color="red"
                    onClick={() => confirm(`Delete ${g.name}? Its members lose the access it gives.`) && remove.mutate(g.id)}>Delete</Button>
                </Row>
              </Table.Td>
            </Table.Tr>
          ))}
          {(list.data ?? []).length === 0 && (
            <Table.Tr><Table.Td colSpan={4}><Text c="dimmed" ta="center">No groups yet.</Text></Table.Td></Table.Tr>
          )}
        </Table.Tbody>
      </Table>
      {editing && <GroupModal group={editing === "new" ? null : editing} onClose={() => setEditing(null)} />}
    </Stack>
  );
}
