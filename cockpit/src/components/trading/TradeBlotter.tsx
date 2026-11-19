import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from "@/components/ui/sheet";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { Select } from "../form";
import { Notice, Spinner } from "../common";
import { OrderStatus, Pill, SideBadge, ORDER_STATUS_TONE } from "../status";
import { cn } from "@/lib/utils";
import { api, ApiError } from "../../api/client";
import { useLive } from "../../api/liveUpdates";
import { numeric } from "./table";
import { OrderTimeline } from "../OrderTimeline";
import type { Cell } from "../../tradeTimes";
import { placedCell, sortBlotter, tradeTimeCell } from "../../tradeTimes";
import { formatFees } from "../../fees";
import type { BlotterRow } from "../../api/types";
import type { GrantedPortfolio } from "../../access";

// Statuses the OMS will never move on from. Mirrors the terminal check in
// POST /orders/cancel (src/handlers.rs) — an order in one of these states is
// done, one way or another.
const TERMINAL = new Set(["filled", "canceled", "rejected", "expired"]);

// How many orders to ask for. The server defaults to 100 and clamps to 1..500
// (list_orders in src/handlers.rs); asking explicitly means the window is a
// decision made here rather than a default that silently truncates the blotter.
const BLOTTER_LIMIT = 200;

// GET /orders/{id}, of which only the status is used here. Mirrors
// OrderAggregateState in src/domain/orders/state.rs.
type OrderState = { order_id: string; status: string };

function notifyError(err: unknown) {
  const message = err instanceof ApiError ? `${err.status}: ${err.message}` : String(err);
  toast.error(message);
}

function TipText({ cell }: { cell: Cell }) {
  if (!cell.tooltip) return <>{cell.text}</>;
  return (
    <Tooltip>
      <TooltipTrigger asChild><span>{cell.text}</span></TooltipTrigger>
      <TooltipContent>{cell.tooltip}</TooltipContent>
    </Tooltip>
  );
}

export function TradeBlotter({
  portfolios,
  followOrderId = null,
}: {
  portfolios: GrantedPortfolio[];
  // The order the ticket just submitted, if any. GET /orders is scoped
  // server-side to can_view grants, so the same permission that decides which
  // rows come back decides which portfolios can be picked — Positions uses
  // can_view for exactly this reason, and the two must not disagree.
  followOrderId?: string | null;
}) {
  const viewable = portfolios.filter((p) => p.can_view);
  const [portfolioId, setPortfolioId] = useState<string | null>(null);
  const [selected, setSelected] = useState<BlotterRow | null>(null);

  // POST /orders/cancel answers 202 when the cancel was FORWARDED to the broker
  // and is still working, and 204 only when it is already done. The execution
  // stream finalises OrderCanceled on broker confirmation — that asymmetry is
  // what fixed the fill-versus-cancel race, so the UI must not flatten it back
  // out by claiming the order is cancelled the moment the request returns.
  // A row stays "cancelling…" until the polled order's OWN status reaches a
  // terminal state (which may be `filled`, not `canceled` — the cancel lost
  // the race, and that is exactly the case this exists for).
  const [cancelling, setCancelling] = useState<Set<string>>(new Set());

  // Filtering and limiting happen SERVER-side. Fetching an unparameterised
  // /orders and filtering in the browser meant the server's own default window
  // (LIMIT 100 ORDER BY created_at DESC) silently hid everything older, and
  // picking a portfolio could show "No orders" while that portfolio had plenty
  // — just none inside the newest hundred rows across all portfolios.
  const live = useLive();
  const orders = useQuery<BlotterRow[]>({
    queryKey: ["/orders", { portfolioId, limit: BLOTTER_LIMIT }],
    queryFn: () => {
      const params = new URLSearchParams({ limit: String(BLOTTER_LIMIT) });
      if (portfolioId) params.set("portfolio_id", portfolioId);
      return api.get<BlotterRow[]>(`/orders?${params.toString()}`);
    },
    refetchInterval: live ? false : 2000,
  });

  // The order the ticket just sent, watched on its own until it leaves
  // `submitted` — the spec's "after submission" behaviour. The submit response
  // is empty (204), so the only way to learn what became of the order is to
  // fetch it; an order that never leaves `submitted` is precisely the
  // recorded-but-not-routed case the 502/503 toasts warn about, and this is
  // where a trader sees it rather than being told.
  const followed = useQuery<OrderState>({
    queryKey: ["/orders", followOrderId, "state"],
    queryFn: () => api.get<OrderState>(`/orders/${followOrderId}`),
    enabled: followOrderId !== null,
    // Stop polling once it has moved: there is nothing further to learn. While
    // live, the orders notice refreshes it instead.
    refetchInterval: (query) =>
      live || (query.state.data && query.state.data.status !== "submitted") ? false : 2000,
  });
  const followedStatus = followOrderId ? followed.data?.status ?? null : null;

  // Drop an order from `cancelling` once its own polled status says it's done —
  // never on the strength of the POST response alone.
  useEffect(() => {
    if (!orders.data) return;
    setCancelling((current) => {
      if (current.size === 0) return current;
      let next: Set<string> | null = null;
      for (const row of orders.data!) {
        if (current.has(row.order_id) && TERMINAL.has(row.status)) {
          if (!next) next = new Set(current);
          next.delete(row.order_id);
        }
      }
      return next ?? current;
    });
  }, [orders.data]);

  async function cancel(orderId: string) {
    setCancelling((s) => new Set(s).add(orderId));
    try {
      await api.post("/orders/cancel", { order_id: orderId });
      // Either 202 or 204: in both cases the row keeps polling until the
      // order's own status changes. We never render "cancelled" on our own
      // authority.
      toast("Cancel sent");
    } catch (err) {
      setCancelling((s) => {
        const n = new Set(s);
        n.delete(orderId);
        return n;
      });
      notifyError(err);
    }
  }

  // No client-side filtering: what came back IS the answer to the query asked.
  const rows = sortBlotter(orders.data ?? []);
  const windowFull = rows.length >= BLOTTER_LIMIT;

  const HEAD: [string, boolean][] = [
    ["Placed", false], ["Trade time", false], ["Booked", false], ["Updated", false], ["Instrument", false], ["Side", false], ["Type", false], ["Status", false],
    ["Qty", true], ["Cum", true], ["Leaves", true], ["Avg px", true], ["Fees", true], ["", false],
  ];
  const followTone = followedStatus === null || followedStatus === "submitted" ? "muted" : ORDER_STATUS_TONE[followedStatus] ?? "muted";

  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-end justify-between gap-3">
        <h2 className="text-sm font-semibold">Blotter</h2>
        {viewable.length > 1 && (
          <Select
            className="w-52"
            label="Portfolio"
            placeholder="All portfolios"
            data={viewable.map((p) => ({ value: p.portfolio_id, label: p.code }))}
            value={portfolioId}
            onChange={setPortfolioId}
            clearable
          />
        )}
      </div>

      {followOrderId !== null && (
        <Notice
          tone={followTone === "sell" ? "error" : followTone === "buy" ? "success" : "info"}
          title={
            followedStatus === null
              ? "Watching your order…"
              : followedStatus === "submitted"
                ? "Your order is recorded, not yet routed"
                : `Your order is ${followedStatus}`
          }
        >
          <span className="num">{followOrderId}</span>
          {followedStatus === "submitted" &&
            " — it has not reached a broker yet. If it stays here, it was never routed: cancel it below."}
          {followed.isError && " — could not read this order back; check the rows below."}
        </Notice>
      )}

      <Sheet open={selected !== null} onOpenChange={(open) => { if (!open) setSelected(null); }}>
        <SheetContent className="w-full overflow-y-auto sm:max-w-xl">
          {selected && (
            <>
              <SheetHeader>
                <SheetTitle>
                  {selected.instrument_symbol ?? selected.instrument_id} · {selected.side} <span className="num">{selected.original_qty}</span>
                </SheetTitle>
                <SheetDescription className="num text-xs">{selected.order_id} · {selected.portfolio_code}</SheetDescription>
              </SheetHeader>
              <div className="px-4 pb-4">
                <OrderTimeline orderId={selected.order_id} eventsPath="/orders" apiGet={api.get} />
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
                {HEAD.map(([h, n], i) => <TableHead key={i} className={n ? "text-right" : undefined}>{h}</TableHead>)}
              </TableRow>
            </TableHeader>
            <TableBody>
              {rows.map((o) => {
                const isCancelling = cancelling.has(o.order_id) && !TERMINAL.has(o.status);
                const canCancel = !TERMINAL.has(o.status) && !isCancelling;
                return (
                  <TableRow
                    key={o.order_id}
                    onClick={() => setSelected(o)}
                    className={cn("cursor-pointer", o.order_id === followOrderId && "bg-primary/10")}
                  >
                    <TableCell className="num"><TipText cell={placedCell(o)} /></TableCell>
                    <TableCell className="num"><TipText cell={tradeTimeCell(o)} /></TableCell>
                    <TableCell className="num">{new Date(o.booked_at).toLocaleString()}</TableCell>
                    <TableCell className="num">{new Date(o.updated_at).toLocaleString()}</TableCell>
                    <TableCell>
                      <div className="flex flex-col items-start">
                        {o.instrument_name ? (
                          <Tooltip>
                            <TooltipTrigger asChild><span>{o.instrument_symbol ?? o.instrument_id}</span></TooltipTrigger>
                            <TooltipContent>{o.instrument_name}</TooltipContent>
                          </Tooltip>
                        ) : (
                          <span>{o.instrument_symbol ?? o.instrument_id}</span>
                        )}
                        {o.client_order_id.startsWith("combo-") && <Pill className="num">{o.client_order_id}</Pill>}
                      </div>
                    </TableCell>
                    <TableCell><SideBadge side={o.side} /></TableCell>
                    <TableCell>{o.order_type}</TableCell>
                    <TableCell>{isCancelling ? <Pill>cancelling…</Pill> : <OrderStatus status={o.status} />}</TableCell>
                    <TableCell className={numeric}>{o.original_qty}</TableCell>
                    <TableCell className={numeric}>{o.cum_qty}</TableCell>
                    <TableCell className={numeric}>{o.leaves_qty}</TableCell>
                    <TableCell className={numeric}>{o.avg_px ?? "—"}</TableCell>
                    <TableCell className={numeric}>{formatFees(o.fees)}</TableCell>
                    <TableCell className="text-right">
                      <Button
                        size="xs"
                        variant="destructive"
                        disabled={!canCancel}
                        onClick={(e) => {
                          e.stopPropagation();
                          cancel(o.order_id);
                        }}
                      >
                        {isCancelling ? "Cancelling…" : "Cancel"}
                      </Button>
                    </TableCell>
                  </TableRow>
                );
              })}
              {windowFull && (
                <TableRow>
                  <TableCell colSpan={14} className="text-center text-xs text-muted-foreground">
                    Showing the {BLOTTER_LIMIT} most recent orders; older ones are not listed.
                  </TableCell>
                </TableRow>
              )}
              {rows.length === 0 && (
                <TableRow>
                  <TableCell colSpan={14} className="py-4 text-center text-muted-foreground">No orders.</TableCell>
                </TableRow>
              )}
            </TableBody>
          </Table>
        </div>
      )}
    </div>
  );
}
