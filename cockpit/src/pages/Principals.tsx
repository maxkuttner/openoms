import { useState } from "react";
import {
  Stack, Group, Title, Button, Table, Modal, Drawer, TextInput, Select, Checkbox,
  Text, Badge, Loader, ActionIcon, Alert, Code, CopyButton, Divider,
} from "@mantine/core";
import { useForm } from "@mantine/form";
import { api } from "../api/client";
import { useList, useApiMutation } from "../api/hooks";
import type { Principal, ApiKeyRecord, Grant, Portfolio } from "../api/types";

const PRINCIPAL_TYPES = ["HUMAN", "SERVICE", "STRATEGY", "DESK"].map((v) => ({ value: v, label: v }));
const STATUS = ["ACTIVE", "INACTIVE"].map((v) => ({ value: v, label: v }));

export function PrincipalsPage() {
  const list = useList<Principal>("/admin/principals");
  const [editing, setEditing] = useState<Principal | {} | null>(null);
  const [manage, setManage] = useState<Principal | null>(null);
  const isCreate = editing != null && !("id" in editing);

  const form = useForm({
    initialValues: { code: "", principal_type: "HUMAN", display_name: "", external_subject: "", status: "ACTIVE" },
  });

  const openForm = (p: Principal | null) => {
    form.setValues({
      code: p?.code ?? "",
      principal_type: p?.principal_type ?? "HUMAN",
      display_name: p?.display_name ?? "",
      external_subject: p?.external_subject ?? "",
      status: p?.status ?? "ACTIVE",
    });
    setEditing(p ?? {});
  };

  const save = useApiMutation(
    (v: any) => {
      const body: any = { ...v };
      if (!body.display_name) delete body.display_name;
      if (!body.external_subject) delete body.external_subject;
      return isCreate ? api.post("/admin/principals", body) : api.patch(`/admin/principals/${(editing as Principal).id}`, body);
    },
    { invalidate: ["/admin/principals"], success: isCreate ? "Principal created" : "Principal updated", onDone: () => setEditing(null) },
  );

  return (
    <Stack>
      <Group justify="space-between">
        <Title order={3}>Principals</Title>
        <Button onClick={() => openForm(null)}>New</Button>
      </Group>

      {list.isLoading ? (
        <Loader />
      ) : (
        <Table striped highlightOnHover withTableBorder>
          <Table.Thead>
            <Table.Tr>
              <Table.Th>Code</Table.Th>
              <Table.Th>Display name</Table.Th>
              <Table.Th>Type</Table.Th>
              <Table.Th>Status</Table.Th>
              <Table.Th />
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {(list.data ?? []).map((p) => (
              <Table.Tr key={p.id}>
                <Table.Td>{p.code}</Table.Td>
                <Table.Td>{p.display_name ?? <Text c="dimmed">—</Text>}</Table.Td>
                <Table.Td>{p.principal_type}</Table.Td>
                <Table.Td><Badge variant="light">{p.status}</Badge></Table.Td>
                <Table.Td>
                  <Group gap="xs" justify="flex-end">
                    <Button size="xs" variant="light" onClick={() => openForm(p)}>Edit</Button>
                    <Button size="xs" onClick={() => setManage(p)}>Keys &amp; grants</Button>
                  </Group>
                </Table.Td>
              </Table.Tr>
            ))}
          </Table.Tbody>
        </Table>
      )}

      <Modal opened={!!editing} onClose={() => setEditing(null)} title={isCreate ? "New principal" : "Edit principal"} centered>
        <form onSubmit={form.onSubmit((v) => save.mutate(v))}>
          <Stack>
            <TextInput label="Code" required disabled={!isCreate} {...form.getInputProps("code")} />
            <Select label="Type" data={PRINCIPAL_TYPES} {...form.getInputProps("principal_type")} />
            <TextInput label="Display name" {...form.getInputProps("display_name")} />
            <TextInput label="External subject (OIDC sub, optional)" {...form.getInputProps("external_subject")} />
            <Select label="Status" data={STATUS} {...form.getInputProps("status")} />
            <Group justify="flex-end">
              <Button variant="default" onClick={() => setEditing(null)}>Cancel</Button>
              <Button type="submit" loading={save.isPending}>Save</Button>
            </Group>
          </Stack>
        </form>
      </Modal>

      <Drawer opened={!!manage} onClose={() => setManage(null)} position="right" size="lg"
        title={manage ? `${manage.code} — keys & grants` : ""}>
        {manage && (
          <Stack>
            <KeysSection principalId={manage.id} />
            <Divider />
            <GrantsSection principalId={manage.id} />
          </Stack>
        )}
      </Drawer>
    </Stack>
  );
}

function KeysSection({ principalId }: { principalId: string }) {
  const base = `/admin/principals/${principalId}/keys`;
  const keys = useList<ApiKeyRecord>(base);
  const [name, setName] = useState("");
  const [newSecret, setNewSecret] = useState<ApiKeyRecord | null>(null);

  const register = useApiMutation((n: string) => api.post<ApiKeyRecord>(base, { name: n || null }), {
    invalidate: [base],
    success: "Key created",
    onDone: () => setName(""),
  });
  const revoke = useApiMutation((keyId: string) => api.del(`${base}/${keyId}`), { invalidate: [base], success: "Key revoked" });

  return (
    <Stack gap="xs">
      <Title order={5}>API keys</Title>
      {newSecret?.secret && (
        <Alert color="yellow" title="Copy this secret now — it is shown only once">
          <Group gap="xs">
            <Code>{newSecret.key_id}</Code> :
            <Code>{newSecret.secret}</Code>
            <CopyButton value={`${newSecret.key_id}:${newSecret.secret}`}>
              {({ copied, copy }) => (
                <Button size="xs" variant="light" onClick={copy}>{copied ? "Copied" : "Copy id:secret"}</Button>
              )}
            </CopyButton>
          </Group>
        </Alert>
      )}
      <Group align="flex-end">
        <TextInput label="New key name (optional)" value={name} onChange={(e) => setName(e.currentTarget.value)} />
        <Button onClick={() => register.mutate(name, { onSuccess: (k: any) => setNewSecret(k) })} loading={register.isPending}>
          Register key
        </Button>
      </Group>
      <Table withTableBorder>
        <Table.Thead>
          <Table.Tr><Table.Th>Key id</Table.Th><Table.Th>Name</Table.Th><Table.Th /></Table.Tr>
        </Table.Thead>
        <Table.Tbody>
          {(keys.data ?? []).map((k) => (
            <Table.Tr key={k.id}>
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

function GrantsSection({ principalId }: { principalId: string }) {
  const base = `/admin/principals/${principalId}/grants`;
  const grants = useList<Grant>(base);
  const portfolios = useList<Portfolio>("/admin/portfolios");
  const codeOf = (id: string) => portfolios.data?.find((p) => p.id === id)?.code ?? id;

  const form = useForm({ initialValues: { portfolio_id: "", can_trade: true, can_view: true, can_allocate: false } });
  const [adding, setAdding] = useState(false);

  const create = useApiMutation((v: any) => api.post(base, v), {
    invalidate: [base], success: "Grant added", onDone: () => { setAdding(false); form.reset(); },
  });
  const update = useApiMutation(
    ({ id, patch }: { id: string; patch: any }) => api.patch(`${base}/${id}`, patch),
    { invalidate: [base], success: "Grant updated" },
  );
  const remove = useApiMutation((id: string) => api.del(`${base}/${id}`), { invalidate: [base], success: "Grant removed" });

  const toggle = (g: Grant, key: "can_trade" | "can_view" | "can_allocate") =>
    update.mutate({ id: g.id, patch: { [key]: !g[key] } });

  return (
    <Stack gap="xs">
      <Group justify="space-between">
        <Title order={5}>Portfolio grants</Title>
        <Button size="xs" onClick={() => setAdding((a) => !a)}>{adding ? "Close" : "Add grant"}</Button>
      </Group>
      {adding && (
        <form onSubmit={form.onSubmit((v) => create.mutate(v))}>
          <Group align="flex-end">
            <Select label="Portfolio" required searchable
              data={(portfolios.data ?? []).map((p) => ({ value: p.id, label: p.code }))}
              {...form.getInputProps("portfolio_id")} />
            <Checkbox label="trade" {...form.getInputProps("can_trade", { type: "checkbox" })} />
            <Checkbox label="view" {...form.getInputProps("can_view", { type: "checkbox" })} />
            <Checkbox label="allocate" {...form.getInputProps("can_allocate", { type: "checkbox" })} />
            <Button type="submit" loading={create.isPending}>Add</Button>
          </Group>
        </form>
      )}
      <Table withTableBorder>
        <Table.Thead>
          <Table.Tr>
            <Table.Th>Portfolio</Table.Th><Table.Th>Trade</Table.Th><Table.Th>View</Table.Th>
            <Table.Th>Allocate</Table.Th><Table.Th />
          </Table.Tr>
        </Table.Thead>
        <Table.Tbody>
          {(grants.data ?? []).map((g) => (
            <Table.Tr key={g.id}>
              <Table.Td>{codeOf(g.portfolio_id)}</Table.Td>
              <Table.Td><Checkbox checked={g.can_trade} onChange={() => toggle(g, "can_trade")} /></Table.Td>
              <Table.Td><Checkbox checked={g.can_view} onChange={() => toggle(g, "can_view")} /></Table.Td>
              <Table.Td><Checkbox checked={g.can_allocate} onChange={() => toggle(g, "can_allocate")} /></Table.Td>
              <Table.Td>
                <Group justify="flex-end">
                  <ActionIcon color="red" variant="light" onClick={() => confirm("Remove grant?") && remove.mutate(g.id)}>✕</ActionIcon>
                </Group>
              </Table.Td>
            </Table.Tr>
          ))}
          {(grants.data ?? []).length === 0 && (
            <Table.Tr><Table.Td colSpan={5}><Text c="dimmed" ta="center">No grants.</Text></Table.Td></Table.Tr>
          )}
        </Table.Tbody>
      </Table>
    </Stack>
  );
}
