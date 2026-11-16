import { useState } from "react";
import {
  Stack, Group, Title, Button, Table, Modal, Drawer, TextInput, PasswordInput, Select, Checkbox, Text, Badge, Loader,
  ActionIcon, Alert, Code, CopyButton, Divider, Switch,
} from "@mantine/core";
import { api } from "../api/client";
import { useList, useApiMutation } from "../api/hooks";
import type { ApiKeyRecord, Portfolio } from "../api/types";
import { ACCOUNT_ROLE_OPTIONS, ROLE_OPTIONS, accessBody, accountRoleLabel, pendingUsers, roleLabel, type Access } from "../users";

const PATH = "/admin/users";

type UserRow = {
  principal_id: string;
  code: string;
  display_name: string | null;
  email: string | null;
  role: string | null;
  account_role: "admin" | "user" | "pending";
  portfolios: { portfolio_id: string; code: string; can_trade: boolean }[];
  sign_in: string | null;
  status: string;
  admin: boolean;
};

function who(u: UserRow) {
  return u.display_name ?? u.email ?? u.code;
}

/** Portfolio checkboxes, each with a trade/view select. Unchecked = no access. */
function PortfolioPicker({ value, onChange }: { value: Record<string, Access>; onChange: (v: Record<string, Access>) => void }) {
  const portfolios = useList<Portfolio>("/admin/portfolios");
  return (
    <Stack gap={6}>
      <Text size="sm" fw={500}>Portfolios</Text>
      {(portfolios.data ?? []).map((p) => (
        <Group key={p.id} gap="sm">
          <Checkbox
            label={p.code}
            w={200}
            checked={p.id in value}
            onChange={(e) => {
              const next = { ...value };
              if (e.currentTarget.checked) next[p.id] = "view";
              else delete next[p.id];
              onChange(next);
            }}
          />
          {p.id in value && (
            <Select
              size="xs"
              w={110}
              allowDeselect={false}
              data={[{ value: "trade", label: "Trade" }, { value: "view", label: "View" }]}
              value={value[p.id]}
              onChange={(v) => v && onChange({ ...value, [p.id]: v as Access })}
            />
          )}
        </Group>
      ))}
      {portfolios.data?.length === 0 && <Text size="sm" c="dimmed">No portfolios yet.</Text>}
    </Stack>
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

  return (
    <Stack>
      <Group justify="space-between">
        <Title order={3}>Users</Title>
        <Button onClick={() => setAdding(true)}>Add user</Button>
      </Group>

      {pending.length > 0 && (
        <Alert color="yellow" title="Waiting for approval">
          <Stack gap="xs">
            {pending.map((u) => (
              <Group key={u.principal_id} justify="space-between">
                <Text size="sm">{who(u)}{u.email && u.display_name ? ` (${u.email})` : ""}{u.sign_in ? ` — via ${u.sign_in}` : ""}</Text>
                <Group gap="xs">
                  <Button size="xs" onClick={() => setApproving(u)}>Approve</Button>
                  <Button size="xs" variant="light" color="red"
                    onClick={() => confirm(`Reject ${who(u)}? The account is disabled.`) && run(u, (x) => api.post(`${PATH}/${x.principal_id}/reject`, {}))}>
                    Reject
                  </Button>
                </Group>
              </Group>
            ))}
          </Stack>
        </Alert>
      )}

      {list.isLoading ? (
        <Loader />
      ) : (
        <Table striped highlightOnHover withTableBorder>
          <Table.Thead>
            <Table.Tr>
              <Table.Th>User</Table.Th>
              <Table.Th>Role</Table.Th>
              <Table.Th>Portfolio access</Table.Th>
              <Table.Th>Sign-in</Table.Th>
              <Table.Th>Status</Table.Th>
              <Table.Th>Admin</Table.Th>
              <Table.Th />
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {(list.data ?? []).map((u) => (
              <Table.Tr key={u.principal_id} c={u.status === "DISABLED" ? "dimmed" : undefined}>
                <Table.Td>
                  {who(u)}
                  {u.email && u.display_name && <Text size="xs" c="dimmed">{u.email}</Text>}
                </Table.Td>
                <Table.Td>{accountRoleLabel(u.account_role)}</Table.Td>
                <Table.Td>
                  {u.portfolios.length === 0
                    ? <Text c="dimmed">—</Text>
                    : `${roleLabel(u.role)}: ` + u.portfolios.map((p) => `${p.code} (${p.can_trade ? "trade" : "view"})`).join(", ")}
                </Table.Td>
                <Table.Td>{u.sign_in ?? <Text c="dimmed">—</Text>}</Table.Td>
                <Table.Td><Badge variant="light">{u.status}</Badge></Table.Td>
                <Table.Td>
                  <Switch
                    aria-label="Admin"
                    checked={u.admin}
                    disabled={u.status !== "ACTIVE"}
                    onChange={(e) => {
                      const admin = e.currentTarget.checked;
                      run(u, (x) => api.put(`${PATH}/${x.principal_id}/admin`, { admin }));
                    }}
                  />
                </Table.Td>
                <Table.Td>
                  <Group gap="xs" justify="flex-end">
                    {u.status === "ACTIVE" && <Button size="xs" variant="light" onClick={() => setEditing(u)}>Edit access</Button>}
                    {u.status === "ACTIVE" && <Button size="xs" variant="light" onClick={() => setKeysFor(u)}>API keys</Button>}
                    {u.status === "ACTIVE" && (
                      <Button size="xs" variant="light" color="red"
                        onClick={() => confirm(`Disable ${who(u)}? Their sessions and API keys stop working.`) && run(u, (x) => api.post(`${PATH}/${x.principal_id}/disable`, {}))}>
                        Disable
                      </Button>
                    )}
                    {u.status === "DISABLED" && (
                      <Button size="xs" variant="light" onClick={() => run(u, (x) => api.post(`${PATH}/${x.principal_id}/enable`, {}))}>Enable</Button>
                    )}
                  </Group>
                </Table.Td>
              </Table.Tr>
            ))}
            {(list.data ?? []).length === 0 && (
              <Table.Tr><Table.Td colSpan={7}><Text c="dimmed" ta="center">No users yet.</Text></Table.Td></Table.Tr>
            )}
          </Table.Tbody>
        </Table>
      )}

      <AddUserModal opened={adding} onClose={() => setAdding(false)} />
      {editing && <EditAccessModal user={editing} onClose={() => setEditing(null)} />}
      {approving && <ApproveModal user={approving} onClose={() => setApproving(null)} />}

      <Drawer opened={!!keysFor} onClose={() => setKeysFor(null)} position="right" size="lg" title={keysFor ? `${who(keysFor)} — API keys` : ""}>
        {keysFor && <KeysSection userId={keysFor.principal_id} />}
      </Drawer>
    </Stack>
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
    <Modal opened={opened} onClose={onClose} title="Add user" centered>
      <form onSubmit={(e) => { e.preventDefault(); create.mutate(undefined); }}>
        <Stack>
          <TextInput label="Name" required value={name} onChange={(e) => setName(e.currentTarget.value)} />
          <TextInput label="Email" required value={email} onChange={(e) => setEmail(e.currentTarget.value)} />
          <PasswordInput label="Password" description="At least 15 characters. Give it to them yourself; they can change it under Account."
            required value={password} onChange={(e) => setPassword(e.currentTarget.value)} autoComplete="new-password" />
          <Select label="Role" data={ACCOUNT_ROLE_OPTIONS} allowDeselect={false} value={role} onChange={(v) => v && setRole(v)} />
          <Group justify="flex-end">
            <Button variant="default" onClick={onClose}>Cancel</Button>
            <Button type="submit" loading={create.isPending}>Add user</Button>
          </Group>
        </Stack>
      </form>
    </Modal>
  );
}

/** Approve a pending account: it becomes a user, with a portfolio role and access. */
function ApproveModal({ user, onClose }: { user: UserRow; onClose: () => void }) {
  const [role, setRole] = useState<string>("viewer");
  const [picked, setPicked] = useState<Record<string, Access>>({});
  const approve = useApiMutation(
    () => api.post(`${PATH}/${user.principal_id}/approve`, { role, portfolios: accessBody(picked) }),
    { invalidate: [PATH], success: "Approved", onDone: onClose },
  );
  return (
    <Modal opened onClose={onClose} title={`Approve ${who(user)}`} centered>
      <form onSubmit={(e) => { e.preventDefault(); approve.mutate(undefined); }}>
        <Stack>
          <Select label="Portfolio role" data={ROLE_OPTIONS} allowDeselect={false} value={role} onChange={(v) => v && setRole(v)} />
          <PortfolioPicker value={picked} onChange={setPicked} />
          <Group justify="flex-end">
            <Button variant="default" onClick={onClose}>Cancel</Button>
            <Button type="submit" loading={approve.isPending}>Approve</Button>
          </Group>
        </Stack>
      </form>
    </Modal>
  );
}

function EditAccessModal({ user, onClose }: { user: UserRow; onClose: () => void }) {
  const [role, setRole] = useState<string>(user.role ?? "trader");
  const [picked, setPicked] = useState<Record<string, Access>>(
    Object.fromEntries(user.portfolios.map((p) => [p.portfolio_id, p.can_trade ? "trade" : "view"] as [string, Access])),
  );
  const save = useApiMutation(
    async () => {
      if (role !== user.role) await api.put(`${PATH}/${user.principal_id}/role`, { role });
      await api.put(`${PATH}/${user.principal_id}/portfolios`, accessBody(picked));
    },
    { invalidate: [PATH], success: "Access updated", onDone: onClose },
  );
  return (
    <Modal opened onClose={onClose} title={`Edit access — ${who(user)}`} centered>
      <Stack>
        <Select label="Role" data={ROLE_OPTIONS} allowDeselect={false} value={role} onChange={(v) => v && setRole(v)} />
        <PortfolioPicker value={picked} onChange={setPicked} />
        <Group justify="flex-end">
          <Button variant="default" onClick={onClose}>Cancel</Button>
          <Button onClick={() => save.mutate(undefined)} loading={save.isPending}>Save</Button>
        </Group>
      </Stack>
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
    <Stack gap="xs">
      {created?.secret && (
        <Alert color="yellow" title="Copy this secret now — it is shown only once">
          <Group gap="xs">
            <Code>{created.key_id}</Code> :
            <Code>{created.secret}</Code>
            <CopyButton value={`${created.key_id}:${created.secret}`}>
              {({ copied, copy }) => <Button size="xs" variant="light" onClick={copy}>{copied ? "Copied" : "Copy id:secret"}</Button>}
            </CopyButton>
          </Group>
        </Alert>
      )}
      <Group align="flex-end">
        <TextInput label="New key name (optional)" value={name} onChange={(e) => setName(e.currentTarget.value)} />
        <Button onClick={() => register.mutate(name)} loading={register.isPending}>Register key</Button>
      </Group>
      <Divider />
      <Table withTableBorder>
        <Table.Thead><Table.Tr><Table.Th>Key id</Table.Th><Table.Th>Name</Table.Th><Table.Th /></Table.Tr></Table.Thead>
        <Table.Tbody>
          {(keys.data ?? []).map((k) => (
            <Table.Tr key={k.key_id}>
              <Table.Td><Code>{k.key_id}</Code></Table.Td>
              <Table.Td>{k.name ?? <Text c="dimmed">—</Text>}</Table.Td>
              <Table.Td>
                <Group justify="flex-end">
                  <ActionIcon color="red" variant="light" onClick={() => confirm("Revoke key?") && revoke.mutate(k.key_id)}>✕</ActionIcon>
                </Group>
              </Table.Td>
            </Table.Tr>
          ))}
          {(keys.data ?? []).length === 0 && (
            <Table.Tr><Table.Td colSpan={3}><Text c="dimmed" ta="center">No keys.</Text></Table.Td></Table.Tr>
          )}
        </Table.Tbody>
      </Table>
    </Stack>
  );
}
