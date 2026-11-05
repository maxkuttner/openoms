import { useState } from "react";
import { Badge, Button, Code, CopyButton, Group, Modal, MultiSelect, Stack, Text } from "@mantine/core";
import { CrudResource } from "../components/CrudResource";
import { api } from "../api/client";
import { notifyError, notifyOk, useList } from "../api/hooks";
import { useQueryClient } from "@tanstack/react-query";
import { connectionEntries, killLabel, reportSummary, type ControlReport } from "../strategies";

const PATH = "/admin/strategies";

type Strategy = {
  code: string;
  name: string;
  portfolio_id: string;
  state: string;
  rest_enabled: boolean;
  session_live: boolean;
  has_fix_password: boolean;
  last_logon_error: string | null;
};

type AccountRow = { id: string; code: string; broker_connection_code: string; portfolio_id: string | null };
type ConnectionRow = { code: string; broker_code: string };

function Confirm({ title, body, onConfirm, onClose }: { title: string; body: string; onConfirm: () => void; onClose: () => void }) {
  return (
    <Modal opened onClose={onClose} title={title} centered>
      <Stack>
        <Text>{body}</Text>
        <Group justify="flex-end">
          <Button variant="default" onClick={onClose}>Cancel</Button>
          <Button color="red" onClick={() => { onConfirm(); onClose(); }}>{title}</Button>
        </Group>
      </Stack>
    </Modal>
  );
}

function ConnectionsEditor({ strategy, onClose }: { strategy: Strategy; onClose: () => void }) {
  const accounts = (useList<AccountRow>("/admin/accounts").data ?? []).filter((a) => a.portfolio_id === strategy.portfolio_id);
  const connections = useList<ConnectionRow>("/admin/broker-connections").data ?? [];
  const [picked, setPicked] = useState<string[]>([]);
  const save = () =>
    api
      .put(`${PATH}/${strategy.code}/connections`, connectionEntries(picked, accounts, connections))
      .then(() => { notifyOk("Connections saved"); onClose(); })
      .catch(notifyError);
  return (
    <Modal opened onClose={onClose} title={`Connections — ${strategy.code}`} centered>
      <Stack>
        <MultiSelect
          label="Accounts"
          data={accounts.map((a) => ({ value: a.id, label: `${a.code} (${a.broker_connection_code})` }))}
          value={picked}
          onChange={setPicked}
        />
        <Group justify="flex-end">
          <Button onClick={save}>Save</Button>
        </Group>
      </Stack>
    </Modal>
  );
}

export function StrategiesPage() {
  const qc = useQueryClient();
  const rows = useList<Strategy>(PATH).data ?? [];
  const [password, setPassword] = useState<{ code: string; value: string } | null>(null);
  const [confirm, setConfirm] = useState<{ title: string; body: string; run: () => void } | null>(null);
  const [editing, setEditing] = useState<Strategy | null>(null);

  const refresh = () => qc.invalidateQueries({ queryKey: [PATH] });
  const control = (code: string, action: "kill" | "flatten") =>
    api
      .post<ControlReport>(`${PATH}/${code}/${action}`, {})
      .then((r) => notifyOk(`${code}: ${reportSummary(r)}`))
      .catch(notifyError)
      .finally(refresh);
  const enable = (code: string) =>
    api.post(`${PATH}/${code}/enable`, {}).then(() => notifyOk(`${code} enabled`)).catch(notifyError).finally(refresh);
  const resetPassword = (code: string) =>
    api
      .post<{ password: string }>(`${PATH}/${code}/password`, {})
      .then((r) => setPassword({ code, value: r.password }))
      .catch(notifyError)
      .finally(refresh);
  const restart = () =>
    api
      .post<{ logged_out: number }>("/admin/fix-acceptor/restart", {})
      .then((r) => notifyOk(`FIX acceptor restarted; ${r.logged_out} session(s) logged out`))
      .catch(notifyError)
      .finally(refresh);
  const live = rows.filter((r) => r.session_live).length;

  return (
    <Stack gap="lg">
      <Group justify="flex-end">
        <Button
          variant="light"
          onClick={() =>
            setConfirm({ title: "Restart FIX acceptor", body: `Logs out ${live} session(s).`, run: restart })
          }
        >
          Restart FIX acceptor
        </Button>
      </Group>
      <CrudResource
        title="Strategies"
        path={PATH}
        idKey="code"
        editable
        deletable
        rowAction={(row: Strategy) => (
          <Group gap={4} wrap="nowrap">
            {killLabel(row.state) === "Kill" ? (
              <Button
                size="xs"
                color="red"
                onClick={() =>
                  setConfirm({ title: "Kill", body: `Halt ${row.code}, cancel its open orders and log it out.`, run: () => control(row.code, "kill") })
                }
              >
                Kill
              </Button>
            ) : (
              <Button size="xs" variant="light" onClick={() => enable(row.code)}>Enable</Button>
            )}
            <Button
              size="xs"
              variant="light"
              color="orange"
              onClick={() =>
                setConfirm({ title: "Flatten", body: `Kill ${row.code} and close every position it holds.`, run: () => control(row.code, "flatten") })
              }
            >
              Flatten
            </Button>
            <Button size="xs" variant="default" onClick={() => resetPassword(row.code)}>Reset password</Button>
            <Button size="xs" variant="default" onClick={() => setEditing(row)}>Connections</Button>
          </Group>
        )}
        columns={[
          { key: "code", label: "Code" },
          { key: "name", label: "Name" },
          { key: "state", label: "State" },
          { key: "rest_enabled", label: "REST", render: (r: Strategy) => (r.rest_enabled ? "on" : "off") },
          {
            key: "session_live",
            label: "FIX session",
            render: (r: Strategy) => <Badge color={r.session_live ? "green" : "gray"}>{r.session_live ? "logged on" : "down"}</Badge>,
          },
          { key: "max_order_notional", label: "Max order notional" },
          { key: "price_band_pct", label: "Price band %" },
          { key: "last_logon_error", label: "Last logon error" },
        ]}
        fields={[
          { name: "code", label: "Code", required: true, inEdit: false },
          { name: "name", label: "Name", required: true },
          { name: "portfolio_id", label: "Portfolio", type: "select", required: true, inEdit: false, optionsPath: "/admin/portfolios", optionValue: "id", optionLabel: "code" },
          { name: "owner_principal_id", label: "Owner", type: "select", required: true, inEdit: false, optionsPath: "/admin/principals", optionValue: "id", optionLabel: "code" },
          { name: "principal_id", label: "Trades as", type: "select", required: true, inEdit: false, optionsPath: "/admin/principals", optionValue: "id", optionLabel: "code" },
          { name: "sender_comp_id", label: "SenderCompID", required: true },
          { name: "target_comp_id", label: "TargetCompID", required: true, default: "OMS" },
          { name: "allowed_addresses", label: "Allowed IP addresses" },
          { name: "max_order_quantity", label: "Max order quantity", type: "number" },
          { name: "max_order_notional", label: "Max order notional", type: "number" },
          { name: "max_position_quantity", label: "Max position quantity", type: "number" },
          { name: "max_position_notional", label: "Max position notional", type: "number" },
          { name: "price_band_pct", label: "Price band %", type: "number" },
        ]}
      />

      {confirm && <Confirm title={confirm.title} body={confirm.body} onConfirm={confirm.run} onClose={() => setConfirm(null)} />}
      {editing && <ConnectionsEditor strategy={editing} onClose={() => setEditing(null)} />}
      {password && (
        <Modal opened onClose={() => setPassword(null)} title={`FIX password — ${password.code}`} centered>
          <Stack>
            <Code block>{password.value}</Code>
            <Group justify="flex-end">
              <CopyButton value={password.value}>
                {({ copied, copy }) => <Button onClick={copy}>{copied ? "Copied" : "Copy"}</Button>}
              </CopyButton>
            </Group>
          </Stack>
        </Modal>
      )}
    </Stack>
  );
}
