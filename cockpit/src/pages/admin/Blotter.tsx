import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from "@/components/ui/sheet";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { Spinner } from "../../components/common";
import { Select } from "../../components/form";
import { OrderStatus, SideBadge } from "../../components/status";
import { useList } from "../../api/hooks";
import { api } from "../../api/client";
import { InstrumentSelect } from "../../components/InstrumentSelect";
import { OrderTimeline } from "../../components/OrderTimeline";
import type { BlotterRow, Portfolio } from "../../api/types";

const STATUSES = ["submitted", "routed", "partially_filled", "filled", "canceled", "rejected", "expired"].map(
  (s) => ({ value: s, label: s }),
);

export function BlotterPage() {
  const [status, setStatus] = useState<string | null>(null);
  const [portfolioId, setPortfolioId] = useState<string | null>(null);
  const [principalId, setPrincipalId] = useState<string | null>(null);
  const [instrument, setInstrument] = useState<string | null>(null);
  const [selected, setSelected] = useState<BlotterRow | null>(null);

  const portfolios = useList<Portfolio>("/admin/portfolios");
  const users = useList<{ principal_id: string; code: string }>("/admin/users");

  const qs = new URLSearchParams();
  if (status) qs.set("status", status);
  if (portfolioId) qs.set("portfolio_id", portfolioId);
  if (principalId) qs.set("principal_id", principalId);
  if (instrument) qs.set("instrument_id", instrument);
  const q = qs.toString();
  const orders = useList<BlotterRow>(`/admin/orders${q ? `?${q}` : ""}`);

  const clear = () => {
    setStatus(null);
    setPortfolioId(null);
    setPrincipalId(null);
    setInstrument(null);
  };

  const num = "num text-right";

  return (
    <div className="flex flex-col gap-3">
      <div>
        <h2 className="text-sm font-semibold">Blotter — who is trading what</h2>
        <p className="text-muted-foreground">Click an order for its full audit trail.</p>
      </div>
      <div className="flex flex-wrap items-end gap-2">
        <Select className="w-40" label="Status" data={STATUSES} value={status} onChange={setStatus} clearable placeholder="Any" />
        <Select
          className="w-44"
          label="Portfolio"
          placeholder="Any"
          data={(portfolios.data ?? []).map((p) => ({ value: p.id, label: p.code }))}
          value={portfolioId}
          onChange={setPortfolioId}
          clearable
        />
        <Select
          className="w-44"
          label="User"
          placeholder="Any"
          data={(users.data ?? []).map((u) => ({ value: u.principal_id, label: u.code }))}
          value={principalId}
          onChange={setPrincipalId}
          clearable
        />
        <div className="w-60">
          <InstrumentSelect label="Instrument" value={instrument} onChange={setInstrument} apiGet={api.get} />
        </div>
        <Button variant="outline" onClick={clear}>Clear</Button>
      </div>

      <Sheet open={selected !== null} onOpenChange={(o) => { if (!o) setSelected(null); }}>
        <SheetContent className="w-full overflow-y-auto sm:max-w-xl">
          {selected && (
            <>
              <SheetHeader>
                <SheetTitle>
                  {selected.instrument_symbol ?? selected.instrument_id} · {selected.side} <span className="num">{selected.original_qty}</span>
                </SheetTitle>
                <SheetDescription className="num text-xs">
                  {selected.order_id} · {selected.portfolio_code} · {selected.principal_code}
                </SheetDescription>
              </SheetHeader>
              <div className="px-4 pb-4">
                <OrderTimeline orderId={selected.order_id} apiGet={api.get} />
              </div>
            </>
          )}
        </SheetContent>
      </Sheet>

      {orders.isLoading ? (
        <Spinner />
      ) : (
        <div className="rounded-md border">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Time</TableHead>
                <TableHead>User</TableHead>
                <TableHead>Portfolio</TableHead>
                <TableHead>Instrument</TableHead>
                <TableHead>Side</TableHead>
                <TableHead>Type</TableHead>
                <TableHead>Status</TableHead>
                <TableHead className="text-right">Qty</TableHead>
                <TableHead className="text-right">Cum</TableHead>
                <TableHead className="text-right">Leaves</TableHead>
                <TableHead className="text-right">Avg px</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {(orders.data ?? []).map((o) => (
                <TableRow key={o.order_id} onClick={() => setSelected(o)} className="cursor-pointer">
                  <TableCell className="num">{new Date(o.created_at).toLocaleString()}</TableCell>
                  <TableCell>{o.principal_code}</TableCell>
                  <TableCell>{o.portfolio_code}</TableCell>
                  <TableCell>
                    {o.instrument_name ? (
                      <Tooltip>
                        <TooltipTrigger asChild><span>{o.instrument_symbol ?? o.instrument_id}</span></TooltipTrigger>
                        <TooltipContent>{o.instrument_name}</TooltipContent>
                      </Tooltip>
                    ) : (
                      <span>{o.instrument_symbol ?? o.instrument_id}</span>
                    )}
                  </TableCell>
                  <TableCell><SideBadge side={o.side} /></TableCell>
                  <TableCell>{o.order_type}</TableCell>
                  <TableCell><OrderStatus status={o.status} /></TableCell>
                  <TableCell className={num}>{o.original_qty}</TableCell>
                  <TableCell className={num}>{o.cum_qty}</TableCell>
                  <TableCell className={num}>{o.leaves_qty}</TableCell>
                  <TableCell className={num}>{o.avg_px ?? "—"}</TableCell>
                </TableRow>
              ))}
              {(orders.data ?? []).length === 0 && (
                <TableRow>
                  <TableCell colSpan={11} className="py-4 text-center text-muted-foreground">No orders match.</TableCell>
                </TableRow>
              )}
            </TableBody>
          </Table>
        </div>
      )}
    </div>
  );
}
