import { useState } from "react";
import { Link } from "react-router-dom";
import { useQueryClient } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { api } from "../../api/client";
import { useList, notifyError, notifyOk } from "../../api/hooks";
import { CopyText, Section, Spinner } from "../../components/common";
import { Select, TextField } from "../../components/form";
import type { Portfolio, TradingTokenCreated, TradingTokenRow } from "../../api/types";

const PATH = "/admin/trading-tokens";

function ago(iso: string): string {
  const s = Math.max(0, Math.round((Date.now() - new Date(iso).getTime()) / 1000));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.round(s / 60)}m`;
  if (s < 86400) return `${Math.round(s / 3600)}h`;
  return `${Math.round(s / 86400)}d`;
}

export function ApiPage() {
  const qc = useQueryClient();
  const { data: tokens, isLoading } = useList<TradingTokenRow>(PATH);
  const { data: users } = useList<{ principal_id: string; code: string; display_name: string | null; status: string }>("/admin/users");
  const { data: portfolios } = useList<Portfolio>("/admin/portfolios");

  const [principalId, setPrincipalId] = useState<string | null>(null);
  const [portfolioId, setPortfolioId] = useState<string | null>(null);
  const [label, setLabel] = useState("");
  const [creating, setCreating] = useState(false);
  const [created, setCreated] = useState<TradingTokenCreated | null>(null);

  const principalOpts = (users ?? []).filter((p) => p.status === "ACTIVE").map((p) => ({
    value: p.principal_id,
    label: p.display_name ? `${p.display_name} · ${p.code}` : p.code,
  }));
  const portfolioOpts = (portfolios ?? []).map((p) => ({
    value: p.id,
    label: p.name ? `${p.name} · ${p.code}` : p.code,
  }));

  const canGenerate = !!principalId;

  async function generate() {
    setCreating(true);
    try {
      const res = await api.post<TradingTokenCreated>(PATH, {
        principal_id: principalId,
        portfolio_id: portfolioId,
        label: label.trim() || null,
      });
      setCreated(res);
      setLabel("");
      qc.invalidateQueries({ queryKey: [PATH] });
      notifyOk("Token created");
    } catch (e) {
      notifyError(e);
    } finally {
      setCreating(false);
    }
  }

  async function revoke(keyId: string) {
    try {
      await api.del(`${PATH}/${keyId}`);
      qc.invalidateQueries({ queryKey: [PATH] });
      if (created?.key_id === keyId) setCreated(null);
      notifyOk("Token revoked");
    } catch (e) {
      notifyError(e);
    }
  }

  const curl = created
    ? `curl "$OMS_URL/orders/submit" -H "Authorization: Bearer ${created.token}" -H "Content-Type: application/json" -d @order.json`
    : "";

  return (
    <div className="flex max-w-4xl flex-col gap-4">
      <div>
        <h2 className="text-sm font-semibold">API tokens</h2>
        <p className="mt-1 max-w-2xl text-muted-foreground">
          A token is a single bearer credential for a bot or strategy. It belongs to a
          user; what it can trade follows that user's portfolio access. Point
          clients at your OMS host and send <code className="num text-buy">Authorization: Bearer &lt;token&gt;</code>.
        </p>
      </div>

      <Section title="Create token">
        {principalOpts.length === 0 ? (
          <p className="text-muted-foreground">
            No users yet. Create one on the <Link to="/admin/users" className="text-primary hover:underline">Users</Link> tab,
            then mint its tokens here.
          </p>
        ) : (
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            <Select label="User" placeholder="trader / bot" data={principalOpts} value={principalId} onChange={setPrincipalId} />
            <Select label="Grant portfolio (optional)" placeholder="no grant" data={portfolioOpts} value={portfolioId}
              onChange={setPortfolioId} clearable />
          </div>
        )}
        <div className="flex items-end gap-2">
          <TextField className="flex-1" label="Label (optional)" placeholder="e.g. prod-key-1" value={label} onChange={setLabel} />
          <Button onClick={generate} disabled={!canGenerate || creating}>Create token</Button>
        </div>
      </Section>

      {created && (
        <Section title={<span className="flex items-center justify-between">New credential <span className="text-xs font-normal text-warn">shown once — copy now</span></span>}
          className="border-l-2 border-l-buy">
          <CopyText label="OMS_TOKEN" value={created.token} />
          <CopyText label="Example" value={curl} />
        </Section>
      )}

      <div className="flex flex-col gap-2">
        <div className="flex items-center justify-between">
          <h3 className="font-semibold">Active tokens</h3>
          {tokens && <span className="num text-xs text-muted-foreground">{tokens.length}</span>}
        </div>
        {isLoading ? (
          <Spinner />
        ) : (tokens ?? []).length === 0 ? (
          <p className="text-muted-foreground">No tokens yet. Create one above.</p>
        ) : (
          <div className="rounded-md border">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Label</TableHead>
                  <TableHead>User</TableHead>
                  <TableHead>Key id</TableHead>
                  <TableHead className="text-right">Age</TableHead>
                  <TableHead />
                </TableRow>
              </TableHeader>
              <TableBody>
                {(tokens ?? []).map((t) => (
                  <TableRow key={t.key_id}>
                    <TableCell>{t.label ?? <span className="text-muted-foreground">unlabeled</span>}</TableCell>
                    <TableCell className="text-foreground-2">{t.principal_name ?? t.principal_code}</TableCell>
                    <TableCell className="num text-xs text-muted-foreground">{t.key_id}</TableCell>
                    <TableCell className="num text-right text-muted-foreground">{ago(t.created_at)}</TableCell>
                    <TableCell className="text-right">
                      <Button size="xs" variant="ghost" className="text-sell" onClick={() => revoke(t.key_id)}>Revoke</Button>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </div>
        )}
      </div>
    </div>
  );
}
