import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { CopyText } from "../../components/common";
import { MultiSelect, Select } from "../../components/form";
import { Modal } from "../../components/overlay";
import { Pill } from "../../components/status";
import { CrudResource } from "../../components/CrudResource";
import { api } from "../../api/client";
import { notifyError, notifyOk, useList } from "../../api/hooks";
import { useQueryClient } from "@tanstack/react-query";
import { connectionEntries, killLabel, reportSummary, savedAccounts, type ControlReport } from "../../strategies";

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
  responsible: { principal_id: string; code: string; display_name: string | null; status: string } | null;
  needs_responsible: boolean;
};

type User = { principal_id: string; code: string; display_name: string | null; status: string };

const userLabel = (u: { code: string; display_name: string | null }) => u.display_name || u.code;

/// Who can be responsible for a strategy: active users only.
function activeUserOptions(users: User[]) {
  return users.filter((u) => u.status === "ACTIVE").map((u) => ({ value: u.principal_id, label: userLabel(u) }));
}

function HandoverForm({ strategy, users, onClose }: { strategy: Strategy; users: User[]; onClose: () => void }) {
  const qc = useQueryClient();
  const [to, setTo] = useState<string | null>(null);
  const save = () =>
    api
      .post(`${PATH}/${strategy.code}/handover`, { principal_id: to })
      .then(() => { notifyOk(`${strategy.code} handed over`); onClose(); })
      .catch(notifyError)
      .finally(() => qc.invalidateQueries({ queryKey: [PATH] }));
  return (
    <Modal open onClose={onClose} title={`Hand over — ${strategy.code}`} footer={<Button onClick={save} disabled={!to}>Hand over</Button>}>
      <p>Responsible now: {strategy.responsible ? userLabel(strategy.responsible) : "nobody"}</p>
      <Select label="New responsible user" data={activeUserOptions(users)} value={to} onChange={setTo} />
    </Modal>
  );
}

type AccountRow = { id: string; code: string; broker_connection_code: string; portfolio_id: string | null };
type ConnectionRow = { code: string; broker_code: string };

function Confirm({ title, body, onConfirm, onClose }: { title: string; body: string; onConfirm: () => void; onClose: () => void }) {
  return (
    <Modal open onClose={onClose} title={title} footer={<>
      <Button variant="outline" onClick={onClose}>Cancel</Button>
      <Button variant="destructive" onClick={() => { onConfirm(); onClose(); }}>{title}</Button>
    </>}>
      <p>{body}</p>
    </Modal>
  );
}

function ConnectionsEditor({ strategy, onClose }: { strategy: Strategy; onClose: () => void }) {
  const accounts = (useList<AccountRow>("/admin/accounts").data ?? []).filter((a) => a.portfolio_id === strategy.portfolio_id);
  const connections = useList<ConnectionRow>("/admin/broker-connections").data ?? [];
  const saved = useList<{ broker_code: string; account_id: string }>(`${PATH}/${strategy.code}/connections`).data;
  const [picked, setPicked] = useState<string[]>([]);
  useEffect(() => setPicked(savedAccounts(saved)), [saved]);
  const save = () =>
    api
      .put(`${PATH}/${strategy.code}/connections`, connectionEntries(picked, accounts, connections))
      .then(() => { notifyOk("Connections saved"); onClose(); })
      .catch(notifyError);
  return (
    <Modal open onClose={onClose} title={`Connections — ${strategy.code}`} footer={<Button onClick={save}>Save</Button>}>
      <MultiSelect
        label="Accounts"
        data={accounts.map((a) => ({ value: a.id, label: `${a.code} (${a.broker_connection_code})` }))}
        value={picked}
        onChange={setPicked}
      />
    </Modal>
  );
}

export function StrategiesPage() {
  const qc = useQueryClient();
  const rows = useList<Strategy>(PATH).data ?? [];
  const [password, setPassword] = useState<{ code: string; value: string } | null>(null);
  const [confirm, setConfirm] = useState<{ title: string; body: string; run: () => void } | null>(null);
  const [editing, setEditing] = useState<Strategy | null>(null);
  const [handover, setHandover] = useState<Strategy | null>(null);
  const users = useList<User>("/admin/users").data ?? [];

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
    <div className="flex flex-col gap-3">
      <div className="flex justify-end">
        <Button
          size="sm"
          variant="secondary"
          onClick={() =>
            setConfirm({ title: "Restart FIX acceptor", body: `Logs out ${live} session(s).`, run: restart })
          }
        >
          Restart FIX acceptor
        </Button>
      </div>
      <CrudResource
        title="Strategies"
        path={PATH}
        idKey="code"
        editable
        deletable
        rowAction={(row: Strategy) => (
          <div className="flex flex-nowrap gap-1">
            {killLabel(row.state) === "Kill" ? (
              <Button
                size="xs"
                variant="destructive"
                onClick={() =>
                  setConfirm({ title: "Kill", body: `Halt ${row.code}, cancel its open orders and log it out.`, run: () => control(row.code, "kill") })
                }
              >
                Kill
              </Button>
            ) : (
              <Button size="xs" variant="secondary" onClick={() => enable(row.code)}>Enable</Button>
            )}
            <Button
              size="xs"
              variant="secondary"
              className="text-warn"
              onClick={() =>
                setConfirm({ title: "Flatten", body: `Kill ${row.code} and close every position it holds.`, run: () => control(row.code, "flatten") })
              }
            >
              Flatten
            </Button>
            <Button size="xs" variant="outline" onClick={() => resetPassword(row.code)}>Reset password</Button>
            <Button size="xs" variant="outline" onClick={() => setEditing(row)}>Connections</Button>
            <Button size="xs" variant="outline" onClick={() => setHandover(row)}>Hand over</Button>
          </div>
        )}
        columns={[
          { key: "code", label: "Code" },
          { key: "name", label: "Name" },
          { key: "state", label: "State" },
          {
            key: "responsible",
            label: "Responsible",
            render: (r: Strategy) => (
              <span className="flex items-center gap-1">
                {r.responsible ? userLabel(r.responsible) : "—"}
                {r.needs_responsible ? <Pill tone="warn">needs a new responsible user</Pill> : null}
              </span>
            ),
          },
          { key: "rest_enabled", label: "REST", render: (r: Strategy) => (r.rest_enabled ? "on" : "off") },
          {
            key: "session_live",
            label: "FIX session",
            render: (r: Strategy) => <Pill tone={r.session_live ? "buy" : "muted"}>{r.session_live ? "logged on" : "down"}</Pill>,
          },
          { key: "max_order_notional", label: "Max order notional" },
          { key: "price_band_pct", label: "Price band %" },
          { key: "last_logon_error", label: "Last logon error" },
        ]}
        fields={[
          { name: "code", label: "Code", required: true, inEdit: false },
          { name: "name", label: "Name", required: true },
          { name: "portfolio_id", label: "Portfolio", type: "select", required: true, inEdit: false, optionsPath: "/admin/portfolios", optionValue: "id", optionLabel: "code" },
          { name: "owner_principal_id", label: "Responsible user", type: "select", required: true, inEdit: false, options: activeUserOptions(users) },
          { name: "sender_comp_id", label: "SenderCompID", required: true },
          { name: "target_comp_id", label: "TargetCompID", required: true, default: "OMS" },
          { name: "allowed_addresses", label: "Allowed IP addresses" },
          { name: "max_order_quantity", label: "Max order quantity", type: "number" },
          { name: "max_order_notional", label: "Max order notional", type: "number" },
          { name: "max_position_quantity", label: "Max position quantity", type: "number" },
          { name: "max_position_notional", label: "Max position notional", type: "number" },
          { name: "price_band_pct", label: "Price band %", type: "number" },
          { name: "max_orders_per_sec", label: "Max orders per second", type: "number" },
        ]}
      />

      {confirm && <Confirm title={confirm.title} body={confirm.body} onConfirm={confirm.run} onClose={() => setConfirm(null)} />}
      {editing && <ConnectionsEditor strategy={editing} onClose={() => setEditing(null)} />}
      {handover && <HandoverForm strategy={handover} users={users} onClose={() => setHandover(null)} />}
      {password && (
        <Modal open onClose={() => setPassword(null)} title={`FIX password — ${password.code}`}>
          <CopyText value={password.value} />
        </Modal>
      )}
    </div>
  );
}
