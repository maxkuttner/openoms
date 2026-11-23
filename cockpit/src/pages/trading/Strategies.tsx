import { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { Sheet, SheetContent, SheetHeader, SheetTitle, SheetTrigger } from "@/components/ui/sheet";
import { cn } from "@/lib/utils";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { api } from "../../api/client";
import { notifyError, notifyOk } from "../../api/hooks";
import { CopyText, Notice, Section } from "../../components/common";
import { ConfirmButton } from "../../components/ConfirmButton";
import { TextField } from "../../components/form";
import { Modal } from "../../components/overlay";
import { Pill } from "../../components/status";
import {
  BASE,
  createReported,
  gapSummary,
  getBatches,
  getFills,
  getStatus,
  archiveReported,
  listReported,
  patchReported,
  revokeToken,
  rotateToken,
  staleLabel,
  tokenActions,
  visibleStrategies,
  type ReportedFill,
  type ReportedStrategy,
} from "../../reported";

/** Shows a freshly issued token; it cannot be fetched again. */
function TokenModal({ code, token, onClose }: { code: string; token: string; onClose: () => void }) {
  return (
    <Modal open onClose={onClose} title={`Report token — ${code}`} footer={<Button onClick={onClose}>Done</Button>}>
      <Notice tone="info">This token is shown once. Copy it now; rotate it to get a new one.</Notice>
      <CopyText value={token} label="Bearer token" />
    </Modal>
  );
}

function CreateForm({ onCreated }: { onCreated: (code: string, token: string) => void }) {
  const qc = useQueryClient();
  const [code, setCode] = useState("");
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const submit = () => {
    setBusy(true);
    createReported(api.post, { code: code.trim(), name: name.trim() })
      .then((r) => { setCode(""); setName(""); onCreated(r.code, r.token); })
      .catch(notifyError)
      .finally(() => { setBusy(false); qc.invalidateQueries({ queryKey: [BASE] }); });
  };
  return (
    <form className="flex items-end gap-2" onSubmit={(e) => { e.preventDefault(); submit(); }}>
      <TextField label="Code" required value={code} onChange={setCode} />
      <TextField label="Name" required value={name} onChange={setName} />
      <Button type="submit" disabled={busy || !code.trim() || !name.trim()}>Create</Button>
    </form>
  );
}

function FillsTable({ code }: { code: string }) {
  const [cursors, setCursors] = useState<(string | null)[]>([null]);
  const before = cursors[cursors.length - 1];
  const page = useQuery({ queryKey: [BASE, code, "fills", before], queryFn: () => getFills(api.get, code, before) });
  const fills: ReportedFill[] = page.data?.fills ?? [];
  return (
    <div className="flex flex-col gap-2">
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>Venue</TableHead><TableHead>Account</TableHead><TableHead>Exec id</TableHead><TableHead>Symbol</TableHead>
            <TableHead>Side</TableHead><TableHead>Qty</TableHead><TableHead>Price</TableHead><TableHead>Fee</TableHead><TableHead>Trade time</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {fills.map((f) => (
            <TableRow key={f.id}>
              <TableCell>{f.venue}</TableCell>
              <TableCell>{f.account_ref}</TableCell>
              <TableCell className="font-mono text-xs">{f.exec_id}</TableCell>
              <TableCell>{f.symbol}</TableCell>
              <TableCell>{f.side}</TableCell>
              <TableCell className="num">{f.qty}</TableCell>
              <TableCell className="num">{f.price}</TableCell>
              <TableCell className="num">{f.fee == null ? "—" : `${f.fee}${f.fee_asset ? ` ${f.fee_asset}` : ""}`}</TableCell>
              <TableCell>{new Date(f.trade_time).toLocaleString()}</TableCell>
            </TableRow>
          ))}
          {fills.length === 0 && (
            <TableRow><TableCell colSpan={9} className="text-center text-muted-foreground">No fills.</TableCell></TableRow>
          )}
        </TableBody>
      </Table>
      <div className="flex gap-2">
        <Button size="xs" variant="secondary" disabled={cursors.length === 1} onClick={() => setCursors(cursors.slice(0, -1))}>Newer</Button>
        <Button size="xs" variant="secondary" disabled={!page.data?.next_before}
          onClick={() => setCursors([...cursors, page.data!.next_before])}>Older</Button>
      </div>
    </div>
  );
}

function StaleAfter({ row }: { row: ReportedStrategy }) {
  const qc = useQueryClient();
  const [value, setValue] = useState(String(row.stale_after_secs));
  const secs = Number.parseInt(value, 10);
  const save = () =>
    patchReported(api.patch, row.code, secs)
      .then(() => notifyOk("Saved"))
      .catch(notifyError)
      .finally(() => qc.invalidateQueries({ queryKey: [BASE] }));
  return (
    <div className="flex items-end gap-2">
      <TextField label="Stale after (seconds)" value={value} onChange={setValue} />
      <Button size="xs" variant="secondary" disabled={!Number.isInteger(secs) || secs <= 0 || secs === row.stale_after_secs} onClick={save}>Save</Button>
    </div>
  );
}

function BatchesTable({ code }: { code: string }) {
  const batches = useQuery({ queryKey: [BASE, code, "batches"], queryFn: () => getBatches(api.get, code) });
  if ((batches.data ?? []).length === 0) return <p className="text-sm text-muted-foreground">No batches.</p>;
  return (
    <Table>
      <TableHeader>
        <TableRow><TableHead>Batch</TableHead><TableHead>Sequence</TableHead><TableHead>Received</TableHead><TableHead>Booked</TableHead><TableHead>Duplicate</TableHead><TableHead>Broken</TableHead></TableRow>
      </TableHeader>
      <TableBody>
        {batches.data!.map((b) => (
          <TableRow key={b.batch_id}>
            <TableCell className="font-mono text-xs">{b.batch_id}</TableCell>
            <TableCell>{b.sequence ?? "—"}{b.sequence_gap && <Pill tone="sell" className="ml-1">gap</Pill>}</TableCell>
            <TableCell>{new Date(b.received_at).toLocaleString()}</TableCell>
            <TableCell className="num">{b.booked}</TableCell>
            <TableCell className="num">{b.duplicate}</TableCell>
            <TableCell className="num">{b.broken}</TableCell>
          </TableRow>
        ))}
      </TableBody>
    </Table>
  );
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex flex-col gap-0.5">
      <span className="text-xs text-muted-foreground">{label}</span>
      <span className="text-sm">{children}</span>
    </div>
  );
}

/** One strategy: health, token state and actions; fills, batches and settings open in a sheet. */
function StrategyCard({ row, onToken }: { row: ReportedStrategy; onToken: (code: string, token: string) => void }) {
  const qc = useQueryClient();
  const status = useQuery({ queryKey: [BASE, row.code, "status"], queryFn: () => getStatus(api.get, row.code), refetchInterval: 30_000 });
  const s = status.data;
  const label = s ? staleLabel(s) : "…";
  const actions = tokenActions(s);
  const refresh = () => qc.invalidateQueries({ queryKey: [BASE, row.code, "status"] });
  const rotate = () => rotateToken(api.post, row.code).then((r) => { refresh(); onToken(row.code, r.token); }).catch(notifyError);
  const revoke = () => revokeToken(api.del, row.code).then(() => { refresh(); notifyOk(`${row.code}: token revoked`); }).catch(notifyError);
  const archive = () => archiveReported(api.post, row.code)
    .then(() => { qc.invalidateQueries({ queryKey: [BASE] }); notifyOk(`${row.code}: archived`); })
    .catch(notifyError);
  const archived = !!row.archived_at;
  return (
    <Card className="gap-3 py-4">
      <CardHeader className="flex flex-row flex-wrap items-center gap-2 px-4">
        <CardTitle className="font-mono">{row.code}</CardTitle>
        <span className="flex-1 truncate text-sm text-muted-foreground">{row.name}</span>
        {archived ? <Pill tone="muted">archived</Pill> : <Pill tone={label === "ok" ? "buy" : "warn"}>{label}</Pill>}
      </CardHeader>
      <CardContent className="flex flex-col gap-3 px-4">
        <div className="grid grid-cols-2 gap-x-4 gap-y-2">
          <Field label="Last report">{s?.last_report_at ? new Date(s.last_report_at).toLocaleString() : "never"}</Field>
          <Field label="Last sequence"><span className="num">{s?.last_sequence ?? "—"}</span></Field>
          <Field label="Open breaks"><span className={cn("num", s && s.open_breaks > 0 && "text-sell")}>{s?.open_breaks ?? "—"}</span></Field>
          <Field label="Stale after"><span className="num">{row.stale_after_secs}s</span></Field>
        </div>
        {s && s.gaps.length > 0 && <Notice tone="error" title="Sequence gaps">{gapSummary(s.gaps)}</Notice>}
        <div className="flex flex-wrap items-center gap-1.5 border-t pt-3">
          {s && !archived && <Pill tone={s.token_active ? "buy" : "sell"}>{s.token_active ? "token active" : "no token"}</Pill>}
          <Pill tone="warn">Self-managed / unverified</Pill>
          {archived ? (
            <span className="ml-auto text-xs text-muted-foreground">Archived {new Date(row.archived_at!).toLocaleDateString()}</span>
          ) : (
          <div className="ml-auto flex gap-1.5">
            <ConfirmButton size="xs" variant="secondary" title={`${actions.rotateLabel} for ${row.code}?`} description="Any current token stops working."
              confirmLabel={actions.rotateLabel} onConfirm={rotate}>{actions.rotateLabel}</ConfirmButton>
            {actions.canRevoke && (
              <ConfirmButton size="xs" variant="destructive" title={`Revoke ${row.code}'s token?`} description="Reports are refused until you issue a new token."
                confirmLabel="Revoke" onConfirm={revoke}>Revoke token</ConfirmButton>
            )}
            <ConfirmButton size="xs" variant="destructive" title={`Archive ${row.code}?`}
              description="Revokes its tokens and hides it from this list. Fills, batches and its portfolio are kept. This cannot be undone here."
              confirmLabel="Archive" onConfirm={archive}>Archive</ConfirmButton>
          </div>
          )}
        </div>
        <Sheet>
          <SheetTrigger asChild>
            <Button size="xs" variant="link" className="self-start px-0">Fills, batches and settings →</Button>
          </SheetTrigger>
          <SheetContent className="w-full overflow-y-auto sm:max-w-3xl">
            <SheetHeader><SheetTitle>{row.code} — {row.name}</SheetTitle></SheetHeader>
            <div className="flex flex-col gap-4 px-4 pb-4">
              {s && s.stale && <Notice tone="error">{s.last_report_at ? `No report for longer than ${row.stale_after_secs}s.` : "This strategy has never reported."}</Notice>}
              <StaleAfter row={row} />
              <h3 className="text-sm font-semibold">Fills</h3>
              <FillsTable code={row.code} />
              <h3 className="text-sm font-semibold">Batches</h3>
              <BatchesTable code={row.code} />
            </div>
          </SheetContent>
        </Sheet>
      </CardContent>
    </Card>
  );
}

/** Strategies that trade outside the OMS and report their fills to it. */
export function StrategiesPage() {
  const list = useQuery({ queryKey: [BASE], queryFn: () => listReported(api.get) });
  const [token, setToken] = useState<{ code: string; token: string } | null>(null);
  const show = (code: string, t: string) => setToken({ code, token: t });
  const [showArchived, setShowArchived] = useState(false);
  const all = list.data ?? [];
  const rows = visibleStrategies(all, showArchived);
  const archivedCount = all.length - visibleStrategies(all, false).length;
  return (
    <div className="flex flex-col gap-4">
      <p className="text-sm text-muted-foreground">
        These strategies trade on their own and report fills here. The OMS books what they report but does not see the venue:
        positions are self-managed and unverified.
      </p>
      <Section title="New strategy">
        <CreateForm onCreated={show} />
        <p className="mt-2 text-xs text-muted-foreground">Creates the strategy, a private portfolio and a reporting token (shown once).</p>
      </Section>
      {archivedCount > 0 && (
        <div className="flex items-center gap-2">
          <Switch id="show-archived" checked={showArchived} onCheckedChange={setShowArchived} />
          <Label htmlFor="show-archived" className="text-sm">Show archived ({archivedCount})</Label>
        </div>
      )}
      <div className="grid grid-cols-1 gap-3 md:grid-cols-2 2xl:grid-cols-3">
        {rows.map((r) => <StrategyCard key={r.code} row={r} onToken={show} />)}
      </div>
      {list.data && rows.length === 0 && <Notice tone="info">No reported strategies yet.</Notice>}
      {token && <TokenModal code={token.code} token={token.token} onClose={() => setToken(null)} />}
    </div>
  );
}
