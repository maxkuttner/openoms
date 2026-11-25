import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Select, Stat } from "../../components/form";
import { Spinner } from "../../components/common";
import { api } from "../../api/client";
import { useLive } from "../../api/liveUpdates";
import { numeric } from "../../components/trading/table";
import { formatFees } from "../../fees";
import type { Me } from "../../access";
import type { MarkRow } from "../../components/trading/types";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { positionLabel } from "../../tradeTimes";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { useAlerts, useFutureRoots } from "../../api/hooks";
import { daysUntil, rollOffer, venueToday } from "../../futures";
import { RollDialog, type RollTarget } from "../../components/trading/RollDialog";
import { totalsByCurrency } from "../../positions";
import type { PositionRow } from "../../positions";

export type { PositionRow };

// mark, market_value, unrealized_pnl and mark_ts are null when no live quote
// exists — an unpriceable or expired contract, or a feed that is down. Render
// them as "—", NEVER as 0: a zero mark silently misstates P&L, and unrealized
// P&L is exactly the number someone glances at before deciding something.
const num = (v: number | null | undefined) => (v === null || v === undefined ? "—" : v);

/** Positions of one portfolio. Pass `portfolioId` to control it from outside (the picker then hides). */
export function PositionsPage({ me, portfolioId: controlled }: { me: Me; portfolioId?: string | null }) {
  const viewable = me.portfolios.filter((p) => p.can_view);
  const [own, setPortfolioId] = useState<string | null>(viewable[0]?.portfolio_id ?? null);
  const portfolioId = controlled === undefined ? own : controlled;

  const live = useLive();
  const positions = useQuery<PositionRow[]>({
    queryKey: ["/portfolios", portfolioId, "positions"],
    queryFn: () => api.get<PositionRow[]>(`/portfolios/${portfolioId}/positions`),
    enabled: portfolioId !== null,
    refetchInterval: live ? false : 5000,
  });

  const rows = positions.data ?? [];

  // Futures: a held contract near its roll date gets a badge and a Roll action.
  const roots = useFutureRoots();
  const alerts = useAlerts(live);
  const [rollTarget, setRollTarget] = useState<RollTarget | null>(null);
  const today = venueToday();
  const offerFor = (p: PositionRow) =>
    rollOffer(p, roots.data ?? [], (alerts.data ?? []).filter((a) => a.portfolio_id === p.portfolio_id), today);
  const portfolioCode = me.portfolios.find((x) => x.portfolio_id === portfolioId)?.code ?? "";

  const heldInstrumentIds = rows.map((p) => p.instrument_id);
  const marks = useQuery<MarkRow[]>({
    queryKey: ["/marks", heldInstrumentIds],
    queryFn: () => api.get<MarkRow[]>(`/marks?instrument_ids=${heldInstrumentIds.join(",")}`),
    enabled: heldInstrumentIds.length > 0,
    refetchInterval: live ? false : 3000,
  });
  const markByInstrument = new Map((marks.data ?? []).map((m) => [String(m.instrument_id), m]));

  // Totals are per currency (no FX). realized_pnl is never null; unrealized_pnl
  // and market_value are null for a position with no live mark, and Day P&L
  // needs a mark AND a previous close — those rows are excluded from their sums
  // rather than treated as zero, and the counts are surfaced below.
  const totals = totalsByCurrency(rows, markByInstrument);
  const unpriced = rows.filter((p) => p.unrealized_pnl === null).length;
  const dayPnlUnpriced = rows.filter((p) => {
    const mark = markByInstrument.get(p.instrument_id);
    return mark?.mid == null || mark?.prev_close == null;
  }).length;

  const tone = (v: number) => (v < 0 ? "neg" : v > 0 ? "pos" : undefined);
  const COLS: [string, boolean][] = [
    ["Instrument", false], ["Net qty", true], ["Avg cost", true], ["Mark", true], ["Market value", true],
    ["Unrealized P&L", true], ["Realized P&L", true], ["Other fees", true], ["Mark time", false], ["", false],
  ];

  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-end justify-between gap-3">
        <h2 className="text-sm font-semibold">Positions</h2>
        {controlled === undefined && viewable.length > 1 && (
          <Select
            className="w-52"
            label="Portfolio"
            data={viewable.map((p) => ({ value: p.portfolio_id, label: p.code }))}
            value={portfolioId}
            onChange={(v) => v && setPortfolioId(v)}
          />
        )}
      </div>

      {rows.length > 0 && (
        <div className="flex flex-col gap-1">
          {[...totals].map(([ccy, t]) => (
            <div key={ccy} className="grid grid-cols-1 gap-2 sm:grid-cols-5">
              <Stat label={`Unrealized P&L · ${ccy}`} value={t.unrealized.toFixed(2)} tone={tone(t.unrealized)} />
              <Stat label={`Realized P&L · ${ccy}`} value={t.realized.toFixed(2)} tone={tone(t.realized)} />
              <Stat label={`Market value · ${ccy}`} value={t.marketValue.toFixed(2)} />
              <Stat label={`Day P&L · ${ccy}`} value={t.day.toFixed(2)} tone={tone(t.day)} />
              <Stat label={`Gross exposure · ${ccy}`} value={t.grossExposure.toFixed(2)} />
            </div>
          ))}
          {unpriced > 0 && (
            <p className="text-xs text-muted-foreground">
              {unpriced} of {rows.length} position{rows.length === 1 ? "" : "s"} {unpriced === 1 ? "has" : "have"} no
              live mark and {unpriced === 1 ? "is" : "are"} excluded from these totals.
            </p>
          )}
          {dayPnlUnpriced > 0 && (
            <p className="text-xs text-muted-foreground">
              {dayPnlUnpriced} of {rows.length} position{rows.length === 1 ? "" : "s"} {dayPnlUnpriced === 1 ? "is" : "are"} missing
              a live mark or a previous close and {dayPnlUnpriced === 1 ? "is" : "are"} excluded from Day P&L.
            </p>
          )}
        </div>
      )}

      {portfolioId === null ? (
        <p className="text-muted-foreground">No portfolio to view.</p>
      ) : positions.isLoading ? (
        <Spinner />
      ) : (
        <div className="rounded-md border">
          <Table>
            <TableHeader>
              <TableRow>
                {COLS.map(([h, n]) => <TableHead key={h} className={n ? "text-right" : undefined}>{h}</TableHead>)}
              </TableRow>
            </TableHeader>
            <TableBody>
              {rows.map((p) => (
                <TableRow key={p.instrument_id}>
                  <TableCell>
                    {(() => {
                      const l = positionLabel(p);
                      return l.tooltip ? (
                        <Tooltip>
                          <TooltipTrigger asChild><span>{l.text}</span></TooltipTrigger>
                          <TooltipContent>{l.tooltip}</TooltipContent>
                        </Tooltip>
                      ) : (
                        <span>{l.text}</span>
                      );
                    })()}
                    {(() => {
                      const o = offerFor(p);
                      if (!o?.due) return null;
                      const d = daysUntil(today, o.rollDate);
                      return (
                        <Badge variant="outline" className="ml-2 border-warn/50 text-warn">
                          {d > 0 ? `Roll in ${d}d` : d === 0 ? "Roll today" : "Roll overdue"}
                        </Badge>
                      );
                    })()}
                  </TableCell>
                  <TableCell className={numeric}>{p.net_qty}</TableCell>
                  <TableCell className={numeric}>{p.avg_cost}</TableCell>
                  <TableCell className={numeric}>{num(p.mark)}</TableCell>
                  <TableCell className={numeric}>{num(p.market_value)}</TableCell>
                  <TableCell className={numeric}>{num(p.unrealized_pnl)}</TableCell>
                  <TableCell className={numeric}>{p.realized_pnl}</TableCell>
                  <TableCell className={numeric}>{formatFees(p.other_fees)}</TableCell>
                  <TableCell className="num">{p.mark_ts ? new Date(p.mark_ts).toLocaleString() : "—"}</TableCell>
                  <TableCell className="text-right">
                    {(() => {
                      const o = offerFor(p);
                      if (!o) return null;
                      return (
                        <Button size="xs" variant={o.due ? "default" : "secondary"} onClick={() => setRollTarget({
                          portfolioId: p.portfolio_id, portfolioCode, netQty: p.net_qty,
                          nearInstrumentId: o.nearInstrumentId, nearSymbol: o.nearSymbol,
                          farInstrumentId: o.farInstrumentId, farSymbol: o.farSymbol,
                        })}>Roll</Button>
                      );
                    })()}
                  </TableCell>
                </TableRow>
              ))}
              {rows.length === 0 && (
                <TableRow>
                  <TableCell colSpan={10} className="py-4 text-center text-muted-foreground">No positions.</TableCell>
                </TableRow>
              )}
            </TableBody>
          </Table>
        </div>
      )}
      <RollDialog target={rollTarget} onClose={() => setRollTarget(null)} />
    </div>
  );
}
