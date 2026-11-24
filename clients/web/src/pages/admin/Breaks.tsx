import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { api } from "../../api/client";
import { useApiMutation, useList } from "../../api/hooks";
import { Notice, Spinner } from "../../components/common";
import { ConfirmButton } from "../../components/ConfirmButton";
import { Select } from "../../components/form";
import { InstrumentSelect } from "../../components/InstrumentSelect";
import { Checkbox } from "@/components/ui/checkbox";
import { Pill } from "../../components/status";
import { adoptReportedBreak, isReportedBreak, reportedBreakLabel } from "../../reported";
import { adoptBreak, dismissBreak, errorText, eventCount, openBreakCount, type ReconBreak } from "../../recon";
import type { Account, Portfolio } from "../../api/types";

export const OPEN_BREAKS = "/admin/recon-breaks?status=open";

/** Open breaks, refetched every 30s so the nav badge stays current. */
export function useOpenBreaks(enabled = true): ReconBreak[] {
  const q = useQuery<ReconBreak[]>({
    queryKey: [OPEN_BREAKS],
    queryFn: () => api.get<ReconBreak[]>(OPEN_BREAKS),
    enabled,
    refetchInterval: 30_000,
  });
  return q.data ?? [];
}

const SOURCES = [
  { value: "ALL", label: "All sources" },
  { value: "RECONCILED", label: "Reconciled (venue)" },
  { value: "REPORTED", label: "Reported by strategies" },
];

export function BreaksPage() {
  const all = useOpenBreaks();
  const [source, setSource] = useState<string>("ALL");
  const breaks = source === "ALL" ? all : all.filter((b) => b.source === source);
  const portfolios = useList<Portfolio>("/admin/portfolios").data ?? [];
  const accounts = useList<Account>("/admin/accounts").data ?? [];
  const accountCode = (id: string) => accounts.find((a) => a.id === id)?.code ?? id.slice(0, 8);

  return (
    <div className="flex flex-col gap-3">
      <p className="text-sm text-muted-foreground">
        Venue orders the OMS did not place, and strategy-reported fills it could not book, held for review ({openBreakCount(breaks)} open). Adopt one into a book to book its fills, or dismiss it.
      </p>
      <div className="w-56">
        <Select data={SOURCES} value={source} onChange={(v) => setSource(v ?? "ALL")} />
      </div>
      {breaks.length === 0 ? (
        <Notice tone="info">No open breaks.</Notice>
      ) : (
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Source</TableHead>
              <TableHead>Connection</TableHead>
              <TableHead>Account</TableHead>
              <TableHead>Venue order</TableHead>
              <TableHead>Kind</TableHead>
              <TableHead>Events</TableHead>
              <TableHead>Created</TableHead>
              <TableHead>Action</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {breaks.map((b) => (
              <BreakRow key={b.id} row={b} account={accountCode(b.account_id ?? "")} portfolios={portfolios} />
            ))}
          </TableBody>
        </Table>
      )}
    </div>
  );
}

function BreakRow({ row, account, portfolios }: { row: ReconBreak; account: string; portfolios: Portfolio[] }) {
  const [portfolio, setPortfolio] = useState<string | null>(null);
  // 422 (fix the cause) and 409 (it changed or closed) say why in the body.
  const [error, setError] = useState<string | null>(null);
  const done = { invalidate: [OPEN_BREAKS] };
  const reported = isReportedBreak(row);
  const [instrument, setInstrument] = useState<string | null>(null);
  const [mapSymbol, setMapSymbol] = useState(true);
  const adopt = useApiMutation(
    () =>
      reported
        ? adoptReportedBreak(api.post, row.id, { instrument_id: instrument!, map_symbol: mapSymbol, ...(portfolio ? { portfolio_id: portfolio } : {}) })
        : adoptBreak(api.post, row.id, portfolio!),
    { ...done, success: "Break adopted" },
  );
  const fill = (row.reported_fill ?? {}) as Record<string, unknown>;
  // Unparseable or conflicting fills cannot be adopted (422); dismiss only.
  const dismissOnly = reported && (row.kind === "INVALID_FILL" || row.kind === "CONFLICTING_DUPLICATE");
  const dismiss = useApiMutation(() => dismissBreak(api.post, row.id), { ...done, success: "Break dismissed" });
  const fail = { onError: (e: unknown) => setError(errorText(e)) };

  return (
    <TableRow>
      <TableCell>
        {reported ? <Pill tone="warn">{reportedBreakLabel}</Pill> : <Pill>Reconciled</Pill>}
        {reported && row.strategy_code && <div className="text-xs text-muted-foreground">{row.strategy_code}</div>}
      </TableCell>
      <TableCell>{reported ? String(fill.venue ?? "—") : row.broker_connection_code}</TableCell>
      <TableCell>{reported ? String(fill.account_ref ?? "—") : account}</TableCell>
      <TableCell className="font-mono text-xs">
        {reported ? `${String(fill.symbol ?? "?")} ${String(fill.side ?? "")} ${String(fill.qty ?? "")} @ ${String(fill.price ?? "")} (${String(fill.exec_id ?? "")})` : row.venue_order_id}
      </TableCell>
      <TableCell>{row.kind}</TableCell>
      <TableCell className="num">{eventCount(row.events)}</TableCell>
      <TableCell>{new Date(row.created_at).toLocaleString()}</TableCell>
      <TableCell>
        <div className="flex items-center gap-2">
          {reported && !dismissOnly && (
            <>
              <div className="w-52">
                <InstrumentSelect value={instrument} onChange={(v) => { setInstrument(v); setError(null); }} placeholder="Instrument" apiGet={api.get} />
              </div>
              <label className="flex items-center gap-1 text-xs">
                <Checkbox checked={mapSymbol} onCheckedChange={(on) => setMapSymbol(on === true)} /> map symbol
              </label>
            </>
          )}
          {!dismissOnly && <div className="w-44">
            <Select
              data={portfolios.map((p) => ({ value: p.id, label: p.code }))}
              value={portfolio}
              onChange={(v) => { setPortfolio(v); setError(null); }}
              placeholder={reported ? "Book (default: strategy's)" : "Adopt into book"}
            />
          </div>}
          {!dismissOnly && <Button
            size="xs"
            disabled={(reported ? !instrument : !portfolio) || adopt.isPending}
            onClick={() => { setError(null); adopt.mutate(undefined, fail); }}
          >
            {adopt.isPending ? <Spinner /> : "Adopt"}
          </Button>}
          <ConfirmButton
            size="xs"
            variant="secondary"
            title="Dismiss this break?"
            description="Its fills will not be booked. This cannot be undone."
            confirmLabel="Dismiss"
            disabled={dismiss.isPending}
            onConfirm={() => { setError(null); dismiss.mutate(undefined, fail); }}
          >
            Dismiss
          </ConfirmButton>
        </div>
        {error && <p role="alert" className="mt-1 text-xs text-sell">{error}</p>}
      </TableCell>
    </TableRow>
  );
}
