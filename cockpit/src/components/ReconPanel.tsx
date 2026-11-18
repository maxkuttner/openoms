import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { api } from "../api/client";
import { useApiMutation, useList } from "../api/hooks";
import { lookbackOptions, policyOptions, reconPatch, showAccounts, sweepSummary, errorText, type SweepReport } from "../recon";
import type { Venue } from "../venues";
import type { Account, BrokerConnection, Portfolio } from "../api/types";
import { Notice, Section } from "./common";
import { ConfirmButton } from "./ConfirmButton";
import { NumberInput, Select, TextField } from "./form";

type Rule = {
  id: string;
  account_id: string | null;
  client_id_prefix: string | null;
  portfolio_id: string;
  priority: number;
};

type Settings = {
  default_portfolio_id: string | null;
  external_policy: string;
  eod_lookback: string;
  intraday_poll_secs: number | null;
};

/**
 * Reconciliation for one broker connection: where venue orders that did not
 * come from the OMS are booked, how far back the nightly sweep reads, and the
 * rules that pick the portfolio.
 */
export function ReconPanel({ connection, venues }: { connection: BrokerConnection; venues: (Venue & { multi_account?: boolean })[] }) {
  return (
    <div className="flex flex-col gap-4">
      <SettingsSection code={connection.code} />
      <SweepSection code={connection.code} />
      <RulesSection connection={connection} multiAccount={showAccounts(connection, venues)} />
    </div>
  );
}

function SettingsSection({ code }: { code: string }) {
  const portfolios = useList<Portfolio>("/admin/portfolios").data ?? [];
  const [portfolio, setPortfolio] = useState<string | null>(null);
  const [policy, setPolicy] = useState<string | null>(null);
  const [lookback, setLookback] = useState<string | null>(null);
  const [secs, setSecs] = useState<number | string>("");
  const [saved, setSaved] = useState<Settings | null>(null);

  const patch = reconPatch({ default_portfolio_id: portfolio, external_policy: policy, eod_lookback: lookback, intraday_poll_secs: String(secs) });
  const save = useApiMutation(() => api.patch<Settings>(`/admin/broker-connections/${code}/recon`, patch), {
    invalidate: [],
    success: "Reconciliation settings saved",
    onDone: (r) => setSaved(r as Settings),
  });

  return (
    <Section title="Settings">
      <Select
        label="Default book"
        description="Where adopted external orders book when no rule matches."
        data={portfolios.map((p) => ({ value: p.id, label: p.code }))}
        value={portfolio}
        onChange={setPortfolio}
        placeholder="Leave unchanged"
      />
      <Select
        label="External orders"
        description="What to do with venue orders that did not come from the OMS."
        data={policyOptions()}
        value={policy}
        onChange={setPolicy}
        placeholder="Leave unchanged"
      />
      <Select
        label="EOD lookback"
        description="How far back the nightly sweep reads. The venue may cap its history; a sweep reports what it obtained."
        data={lookbackOptions()}
        value={lookback}
        onChange={setLookback}
        placeholder="Leave unchanged"
      />
      <NumberInput
        label="Intraday poll (seconds)"
        description="Blank leaves the setting as it is."
        min={1}
        step={1}
        value={secs}
        onChange={setSecs}
      />
      <p className="text-xs text-muted-foreground">Only the fields you fill in are changed; a setting cannot be cleared once set.</p>
      <div>
        <Button size="sm" disabled={Object.keys(patch).length === 0 || save.isPending} onClick={() => save.mutate(undefined)}>
          Save
        </Button>
      </div>
      {saved && (
        <Notice tone="success" title="Now in effect">
          {saved.external_policy} · lookback {saved.eod_lookback} · poll {saved.intraday_poll_secs ?? "adapter default"} · book{" "}
          {portfolios.find((p) => p.id === saved.default_portfolio_id)?.code ?? "none"}
        </Notice>
      )}
    </Section>
  );
}

function SweepSection({ code }: { code: string }) {
  const [report, setReport] = useState<SweepReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const sweep = useApiMutation(() => api.post<SweepReport>(`/admin/broker-connections/${code}/sweep`, {}), {
    invalidate: ["/admin/recon-breaks?status=open"],
    success: "Sweep finished",
    onDone: (r) => {
      setError(null);
      setReport(r as SweepReport);
    },
  });
  return (
    <Section title="Sweep">
      <p className="text-xs text-muted-foreground">Reads the venue's history for this connection now, as the nightly job does, over the EOD lookback.</p>
      <div>
        <Button
          size="sm"
          variant="secondary"
          disabled={sweep.isPending}
          onClick={() => sweep.mutate(undefined, { onError: (e) => setError(errorText(e)) })}
        >
          {sweep.isPending ? "Sweeping…" : "Sweep now"}
        </Button>
      </div>
      {error && <Notice title="Sweep failed">{error}</Notice>}
      {report && (
        <Notice tone={report.failed > 0 || report.errors.length > 0 ? "error" : "success"} title="Sweep report">
          {sweepSummary(report)}
          {report.errors.length > 0 && (
            <ul className="mt-1 list-disc pl-4">
              {report.errors.map((e, i) => (
                <li key={i}>{e}</li>
              ))}
            </ul>
          )}
        </Notice>
      )}
    </Section>
  );
}

function RulesSection({ connection, multiAccount }: { connection: BrokerConnection; multiAccount: boolean }) {
  const base = `/admin/broker-connections/${connection.code}/attribution-rules`;
  const rules = useList<Rule>(base).data ?? [];
  const portfolios = useList<Portfolio>("/admin/portfolios").data ?? [];
  const accounts = (useList<Account>("/admin/accounts", multiAccount).data ?? []).filter((a) => a.broker_connection_code === connection.code);
  const portfolioCode = (id: string) => portfolios.find((p) => p.id === id)?.code ?? id;
  const accountCode = (id: string | null) => (id ? (accounts.find((a) => a.id === id)?.code ?? id) : "—");

  const [editing, setEditing] = useState<string | null>(null); // rule id, or "" for new
  const [account, setAccount] = useState<string | null>(null);
  const [prefix, setPrefix] = useState("");
  const [portfolio, setPortfolio] = useState<string | null>(null);
  const [priority, setPriority] = useState<number | string>(100);
  const [error, setError] = useState<string | null>(null);

  const open = (r: Rule | null) => {
    setEditing(r?.id ?? "");
    setAccount(r?.account_id ?? null);
    setPrefix(r?.client_id_prefix ?? "");
    setPortfolio(r?.portfolio_id ?? null);
    setPriority(r?.priority ?? 100);
    setError(null);
  };
  const body = { account_id: account, client_id_prefix: prefix || null, portfolio_id: portfolio, priority: Number(priority) };
  const save = useApiMutation(() => (editing ? api.put(`${base}/${editing}`, body) : api.post(base, body)), {
    invalidate: [base],
    success: "Rule saved",
    onDone: () => setEditing(null),
  });
  const remove = useApiMutation((id: string) => api.del(`${base}/${id}`), { invalidate: [base], success: "Rule deleted" });

  return (
    <Section title="Attribution rules">
      <p className="text-xs text-muted-foreground">
        Pick the portfolio an adopted order books into. Lowest priority number wins; no match falls back to the default book.
      </p>
      {rules.length > 0 && (
        <Table>
          <TableHeader>
            <TableRow>
              {multiAccount && <TableHead>Account</TableHead>}
              <TableHead>Client id prefix</TableHead>
              <TableHead>Portfolio</TableHead>
              <TableHead>Priority</TableHead>
              <TableHead />
            </TableRow>
          </TableHeader>
          <TableBody>
            {[...rules].sort((a, b) => a.priority - b.priority).map((r) => (
              <TableRow key={r.id}>
                {multiAccount && <TableCell>{accountCode(r.account_id)}</TableCell>}
                <TableCell>{r.client_id_prefix ?? "—"}</TableCell>
                <TableCell>{portfolioCode(r.portfolio_id)}</TableCell>
                <TableCell className="num">{r.priority}</TableCell>
                <TableCell className="flex justify-end gap-1">
                  <Button size="xs" variant="secondary" onClick={() => open(r)}>
                    Edit
                  </Button>
                  <ConfirmButton size="xs" variant="secondary" title="Delete this rule?" confirmLabel="Delete" onConfirm={() => remove.mutate(r.id)}>
                    Delete
                  </ConfirmButton>
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      )}
      {editing === null ? (
        <div>
          <Button size="sm" variant="secondary" onClick={() => open(null)}>
            Add rule
          </Button>
        </div>
      ) : (
        <div className="flex flex-col gap-3 rounded-md border p-3">
          {multiAccount && (
            <Select
              label="Account"
              data={accounts.map((a) => ({ value: a.id, label: a.code }))}
              value={account}
              onChange={setAccount}
              clearable
              placeholder="Any account"
            />
          )}
          <TextField label="Client id prefix" description="A rule needs an account or a prefix." value={prefix} onChange={setPrefix} />
          <Select label="Portfolio" required data={portfolios.map((p) => ({ value: p.id, label: p.code }))} value={portfolio} onChange={setPortfolio} />
          <NumberInput label="Priority" min={0} step={1} value={priority} onChange={setPriority} />
          {error && <Notice title="Not saved">{error}</Notice>}
          <div className="flex gap-2">
            <Button
              size="sm"
              disabled={!portfolio || save.isPending}
              onClick={() => save.mutate(undefined, { onError: (e) => setError(errorText(e)) })}
            >
              Save rule
            </Button>
            <Button size="sm" variant="ghost" onClick={() => setEditing(null)}>
              Cancel
            </Button>
          </div>
        </div>
      )}
    </Section>
  );
}
