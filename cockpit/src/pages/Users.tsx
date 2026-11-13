import { useState } from "react";
import {
  Stack, Group, Title, Button, Table, Modal, Drawer, TextInput, Select, Checkbox, Text, Badge, Loader,
  ActionIcon, Alert, Code, CopyButton, Divider, Anchor, Switch,
} from "@mantine/core";
import { Link } from "react-router-dom";
import { api, isLegacySession } from "../api/client";
import { useList, useApiMutation } from "../api/hooks";
import type { ApiKeyRecord, Portfolio } from "../api/types";
import { useSetupStatus } from "../components/SetupChecklist";
import { INVITE_METHOD_OPTIONS, ROLE_OPTIONS, SIGN_IN_WARNING, accessBody, inviteNotice, roleLabel, type Access } from "../users";

const PATH = "/admin/users";

type UserRow = {
  principal_id: string;
  code: string;
  display_name: string | null;
  email: string | null;
  role: string | null;
  portfolios: { portfolio_id: string; code: string; can_trade: boolean }[];
  sign_in: string | null;
  status: string;
  invite: { invite_id: string; expires_at: string } | null;
  admin: boolean;
};

type PendingBinding = {
  binding_id: string;
  user_code: string;
  invited_email: string;
  issuer: string;
  subject: string;
  untrusted_email: string | null;
  display_name: string | null;
  created_at: string;
};

type InviteResult = { link: string; expires_at: string | null };

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

function LinkBox({ email, result }: { email: string; result: InviteResult }) {
  return (
    <Stack gap="xs">
      <Text size="sm">{result.expires_at ? inviteNotice(email, result.expires_at) : `Send this link to ${email}.`}</Text>
      <Group gap="xs" wrap="nowrap">
        <Code style={{ overflowWrap: "anywhere" }}>{result.link}</Code>
        <CopyButton value={result.link}>
          {({ copied, copy }) => <Button size="xs" variant="light" onClick={copy}>{copied ? "Copied" : "Copy"}</Button>}
        </CopyButton>
      </Group>
    </Stack>
  );
}

export function UsersPage() {
  const list = useList<UserRow>(PATH);
  const setup = useSetupStatus();
  const [inviting, setInviting] = useState(false);
  const [keyOnly, setKeyOnly] = useState(false);
  const [editing, setEditing] = useState<UserRow | null>(null);
  const [keysFor, setKeysFor] = useState<UserRow | null>(null);
  const [shown, setShown] = useState<{ email: string; result: InviteResult } | null>(null);
  const [bootstrapping, setBootstrapping] = useState(false);
  const legacy = isLegacySession();

  const reload = [PATH, "/admin/setup-status"];
  const linkAction = (path: (u: UserRow) => string) => (u: UserRow) =>
    api.post<InviteResult>(path(u), {}).then((result) => setShown({ email: u.email ?? who(u), result }));
  const act = useApiMutation(
    ({ u, fn }: { u: UserRow; fn: (u: UserRow) => Promise<unknown> }) => fn(u),
    { invalidate: reload, success: "Done" },
  );
  const run = (u: UserRow, fn: (u: UserRow) => Promise<unknown>) => act.mutate({ u, fn });

  return (
    <Stack>
      <Group justify="space-between">
        <Title order={3}>Users</Title>
        {legacy ? (
          <Button onClick={() => setBootstrapping(true)}>Bootstrap named admin</Button>
        ) : (
          <Group>
            <Button variant="default" onClick={() => setKeyOnly(true)}>New API-key user</Button>
            <Button onClick={() => setInviting(true)}>Invite user</Button>
          </Group>
        )}
      </Group>

      {legacy && (
        <Alert color="yellow">
          You're using the shared admin password. It can only create a named admin: do that, open the link,
          set a password and authenticator, then sign in as that admin.
        </Alert>
      )}

      {!legacy && <PendingBindings />}

      {setup.data && !setup.data.sign_in_ok && (
        <Alert color="yellow">
          {SIGN_IN_WARNING}{" "}
          <Anchor component={Link} to="/sign-in" inherit>Sign-in settings</Anchor>
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
              <Table.Th>Portfolios</Table.Th>
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
                <Table.Td>{roleLabel(u.role)}</Table.Td>
                <Table.Td>
                  {u.portfolios.length === 0
                    ? <Text c="dimmed">—</Text>
                    : u.portfolios.map((p) => `${p.code} (${p.can_trade ? "trade" : "view"})`).join(", ")}
                </Table.Td>
                <Table.Td>
                  {u.invite
                    ? <Text size="sm">Invite expires {new Date(u.invite.expires_at).toLocaleDateString()}</Text>
                    : u.sign_in ?? <Text c="dimmed">—</Text>}
                </Table.Td>
                <Table.Td><Badge variant="light">{u.status}</Badge></Table.Td>
                <Table.Td>
                  <Switch
                    aria-label="Admin"
                    checked={u.admin}
                    disabled={legacy || u.status === "DISABLED"}
                    onChange={(e) => {
                      const admin = e.currentTarget.checked;
                      run(u, (x) => api.put(`${PATH}/${x.principal_id}/admin`, { admin }));
                    }}
                  />
                </Table.Td>
                <Table.Td>
                  <Group gap="xs" justify="flex-end">
                    {u.invite && (
                      <>
                        <Button size="xs" variant="light" onClick={() => run(u, linkAction((x) => `${PATH}/invites/${x.invite!.invite_id}/reissue`))}>New link</Button>
                        <Button size="xs" variant="light" color="red" onClick={() => run(u, (x) => api.post(`${PATH}/invites/${x.invite!.invite_id}/revoke`, {}))}>Revoke</Button>
                      </>
                    )}
                    {u.status === "ACTIVE" && !u.sign_in && !u.invite && (
                      <Button size="xs" variant="light" onClick={() => run(u, linkAction((x) => `${PATH}/${x.principal_id}/link-invite`))}>Sign-in link</Button>
                    )}
                    {u.status !== "DISABLED" && <Button size="xs" variant="light" onClick={() => setEditing(u)}>Edit access</Button>}
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

      <InviteModal opened={inviting} onClose={() => setInviting(false)} onDone={(email, result) => { setInviting(false); setShown({ email, result }); }} />
      <BootstrapAdminModal opened={bootstrapping} onClose={() => setBootstrapping(false)} onDone={(email, result) => { setBootstrapping(false); setShown({ email, result }); }} />
      <KeyOnlyModal opened={keyOnly} onClose={() => setKeyOnly(false)} />
      {editing && <EditAccessModal user={editing} onClose={() => setEditing(null)} />}

      <Modal opened={!!shown} onClose={() => setShown(null)} title="Invite link" centered size="lg">
        {shown && <LinkBox email={shown.email} result={shown.result} />}
      </Modal>

      <Drawer opened={!!keysFor} onClose={() => setKeysFor(null)} position="right" size="lg" title={keysFor ? `${who(keysFor)} — API keys` : ""}>
        {keysFor && <KeysSection userId={keysFor.principal_id} />}
      </Drawer>
    </Stack>
  );
}

function InviteModal({ opened, onClose, onDone }: { opened: boolean; onClose: () => void; onDone: (email: string, r: InviteResult) => void }) {
  const [email, setEmail] = useState("");
  const [role, setRole] = useState<string>("trader");
  const [method, setMethod] = useState<string>("any");
  const [picked, setPicked] = useState<Record<string, Access>>({});
  const send = useApiMutation(
    () => api.post<InviteResult>(`${PATH}/invite`, { email: email.trim(), role, method, portfolios: accessBody(picked) }),
    {
      invalidate: [PATH, "/admin/setup-status"],
      success: "Invite created",
      onDone: (r) => { onDone(email.trim(), r as InviteResult); setEmail(""); setPicked({}); },
    },
  );
  return (
    <Modal opened={opened} onClose={onClose} title="Invite user" centered>
      <form onSubmit={(e) => { e.preventDefault(); send.mutate(undefined); }}>
        <Stack>
          <TextInput label="Email" type="email" required value={email} onChange={(e) => setEmail(e.currentTarget.value)} />
          <Select label="Role" data={ROLE_OPTIONS} allowDeselect={false} value={role} onChange={(v) => v && setRole(v)} />
          <Select label="Sign-in method" data={INVITE_METHOD_OPTIONS} allowDeselect={false} value={method} onChange={(v) => v && setMethod(v)} />
          <PortfolioPicker value={picked} onChange={setPicked} />
          <Group justify="flex-end">
            <Button variant="default" onClick={onClose}>Cancel</Button>
            <Button type="submit" loading={send.isPending}>Create invite</Button>
          </Group>
        </Stack>
      </form>
    </Modal>
  );
}

/** Sign-in identities waiting for an admin to compare and approve them. */
function PendingBindings() {
  const path = `${PATH}/bindings`;
  const list = useList<PendingBinding>(path);
  const decide = useApiMutation(
    ({ id, verdict }: { id: string; verdict: "approve" | "reject" }) => api.post(`${path}/${id}/${verdict}`, {}),
    { invalidate: [path, PATH], success: "Done" },
  );
  if (!list.data?.length) return null;
  return (
    <Stack gap="xs">
      <Title order={5}>Waiting for approval</Title>
      <Text size="sm" c="dimmed">
        Someone opened an invite and signed in with SSO. Check with the invited person, by a channel you trust,
        that this sign-in is theirs before approving. The provider's email is not verified.
      </Text>
      <Table withTableBorder>
        <Table.Thead>
          <Table.Tr><Table.Th>Invite</Table.Th><Table.Th>Signed in as</Table.Th><Table.Th>Provider</Table.Th><Table.Th /></Table.Tr>
        </Table.Thead>
        <Table.Tbody>
          {list.data.map((b) => (
            <Table.Tr key={b.binding_id}>
              <Table.Td>{b.invited_email}</Table.Td>
              <Table.Td>
                {b.display_name ?? "—"}
                {b.untrusted_email && <Text size="xs" c="dimmed">{b.untrusted_email} (unverified)</Text>}
              </Table.Td>
              <Table.Td>
                <Text size="xs">{b.issuer}</Text>
                <Text size="xs" c="dimmed">{b.subject}</Text>
              </Table.Td>
              <Table.Td>
                <Group gap="xs" justify="flex-end">
                  <Button size="xs" variant="light" onClick={() => decide.mutate({ id: b.binding_id, verdict: "approve" })}>Approve</Button>
                  <Button size="xs" variant="light" color="red" onClick={() => decide.mutate({ id: b.binding_id, verdict: "reject" })}>Reject</Button>
                </Group>
              </Table.Td>
            </Table.Tr>
          ))}
        </Table.Tbody>
      </Table>
    </Stack>
  );
}

/** Create the first named admin with the shared admin password. */
function BootstrapAdminModal({ opened, onClose, onDone }: { opened: boolean; onClose: () => void; onDone: (email: string, r: InviteResult) => void }) {
  const [email, setEmail] = useState("");
  const [name, setName] = useState("");
  const create = useApiMutation(
    () => api.post<{ user_id: string; link: string }>(`${PATH}/bootstrap-admin`, { email: email.trim(), display_name: name.trim() || null }),
    {
      invalidate: [PATH],
      success: "Admin created",
      onDone: (r) => onDone(email.trim(), { link: (r as { link: string }).link, expires_at: null }),
    },
  );
  return (
    <Modal opened={opened} onClose={onClose} title="Bootstrap named admin" centered>
      <form onSubmit={(e) => { e.preventDefault(); create.mutate(undefined); }}>
        <Stack>
          <TextInput label="Email" type="email" required value={email} onChange={(e) => setEmail(e.currentTarget.value)} />
          <TextInput label="Name" value={name} onChange={(e) => setName(e.currentTarget.value)} />
          <Group justify="flex-end">
            <Button variant="default" onClick={onClose}>Cancel</Button>
            <Button type="submit" loading={create.isPending}>Create</Button>
          </Group>
        </Stack>
      </form>
    </Modal>
  );
}

function KeyOnlyModal({ opened, onClose }: { opened: boolean; onClose: () => void }) {
  const [code, setCode] = useState("");
  const [name, setName] = useState("");
  const [role, setRole] = useState<string>("trader");
  const create = useApiMutation(
    () => api.post(PATH, { code: code.trim(), display_name: name.trim(), role }),
    { invalidate: [PATH], success: "User created", onDone: () => { setCode(""); setName(""); onClose(); } },
  );
  return (
    <Modal opened={opened} onClose={onClose} title="New API-key user" centered>
      <form onSubmit={(e) => { e.preventDefault(); create.mutate(undefined); }}>
        <Stack>
          <Text size="sm" c="dimmed">For a bot or script. It can't sign in; it connects with API keys.</Text>
          <TextInput label="Code" required value={code} onChange={(e) => setCode(e.currentTarget.value)} />
          <TextInput label="Name" required value={name} onChange={(e) => setName(e.currentTarget.value)} />
          <Select label="Role" data={ROLE_OPTIONS} allowDeselect={false} value={role} onChange={(v) => v && setRole(v)} />
          <Group justify="flex-end">
            <Button variant="default" onClick={onClose}>Cancel</Button>
            <Button type="submit" loading={create.isPending}>Create</Button>
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
