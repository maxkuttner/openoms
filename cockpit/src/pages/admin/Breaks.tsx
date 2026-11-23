import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { api } from "../../api/client";
import { useApiMutation, useList } from "../../api/hooks";
import { Notice, Spinner } from "../../components/common";
import { ConfirmButton } from "../../components/ConfirmButton";
import { Select } from "../../components/form";
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

export function BreaksPage() {
  const breaks = useOpenBreaks();
  const portfolios = useList<Portfolio>("/admin/portfolios").data ?? [];
  const accounts = useList<Account>("/admin/accounts").data ?? [];
  const accountCode = (id: string) => accounts.find((a) => a.id === id)?.code ?? id.slice(0, 8);

  return (
    <div className="flex flex-col gap-3">
      <p className="text-sm text-muted-foreground">
        Venue orders the OMS did not place, held for review ({openBreakCount(breaks)} open). Adopt one into a book to book its fills, or dismiss it.
      </p>
      {breaks.length === 0 ? (
        <Notice tone="info">No open breaks.</Notice>
      ) : (
        <Table>
          <TableHeader>
            <TableRow>
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
  const adopt = useApiMutation(() => adoptBreak(api.post, row.id, portfolio!), { ...done, success: "Break adopted" });
  const dismiss = useApiMutation(() => dismissBreak(api.post, row.id), { ...done, success: "Break dismissed" });
  const fail = { onError: (e: unknown) => setError(errorText(e)) };

  return (
    <TableRow>
      <TableCell>{row.broker_connection_code}</TableCell>
      <TableCell>{account}</TableCell>
      <TableCell className="font-mono text-xs">{row.venue_order_id}</TableCell>
      <TableCell>{row.kind}</TableCell>
      <TableCell className="num">{eventCount(row.events)}</TableCell>
      <TableCell>{new Date(row.created_at).toLocaleString()}</TableCell>
      <TableCell>
        <div className="flex items-center gap-2">
          <div className="w-44">
            <Select
              data={portfolios.map((p) => ({ value: p.id, label: p.code }))}
              value={portfolio}
              onChange={(v) => { setPortfolio(v); setError(null); }}
              placeholder="Adopt into book"
            />
          </div>
          <Button
            size="xs"
            disabled={!portfolio || adopt.isPending}
            onClick={() => { setError(null); adopt.mutate(undefined, fail); }}
          >
            {adopt.isPending ? <Spinner /> : "Adopt"}
          </Button>
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
