import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Area, AreaChart, CartesianGrid, XAxis, YAxis } from "recharts";
import { ChartContainer, ChartTooltip, ChartTooltipContent, type ChartConfig } from "@/components/ui/chart";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import type { Me } from "../../access";
import { api } from "../../api/client";
import type { BlotterRow } from "../../api/types";
import { Notice, Section } from "../../components/common";
import { SampleBadge } from "../../components/market/SampleBadge";
import { OrderStatus, SideBadge } from "../../components/status";
import type { PositionRow } from "../../positions";
import { fmtLocal } from "../../tradeTimes";
import { exposureShares, pickDefaultPortfolio, samplePnl } from "../../portfolioView";
import { PortfolioPicker } from "../../components/PortfolioPicker";
import { PositionsPage } from "./Positions";

const RECENT = 10;
const pnlConfig = { pnl: { label: "P&L", color: "var(--primary)" } } satisfies ChartConfig;

function Exposure({ portfolioId }: { portfolioId: string }) {
  // Same query key as the positions table below, so this is one request.
  const positions = useQuery<PositionRow[]>({
    queryKey: ["/portfolios", portfolioId, "positions"],
    queryFn: () => api.get<PositionRow[]>(`/portfolios/${portfolioId}/positions`),
  });
  const shares = exposureShares(positions.data ?? [], 5);
  return (
    <Section title="Exposure by instrument">
      {shares.length === 0 ? (
        <p className="text-sm text-muted-foreground">No priced positions.</p>
      ) : (
        <ul className="flex flex-col gap-2.5">
          {shares.map((s) => (
            <li key={s.label} className="flex flex-col gap-1 text-xs">
              <div className="flex justify-between"><span>{s.label}</span><span className="num text-muted-foreground">{s.pct}%</span></div>
              <div className="h-1.5 rounded bg-muted"><div className="h-1.5 rounded bg-primary" style={{ width: `${s.pct}%` }} /></div>
            </li>
          ))}
        </ul>
      )}
    </Section>
  );
}

function PnlChart({ portfolioId }: { portfolioId: string }) {
  const data = samplePnl(portfolioId, 30);
  return (
    <Section title={<span className="flex items-center gap-2">P&L — last 30 days <SampleBadge /></span>}>
      <ChartContainer config={pnlConfig} className="h-[200px] w-full">
        <AreaChart data={data} margin={{ left: 4, right: 8 }}>
          <CartesianGrid vertical={false} />
          <XAxis dataKey="date" tickLine={false} axisLine={false} minTickGap={32} tickFormatter={(d: string) => d.slice(5)} />
          <YAxis tickLine={false} axisLine={false} width={56} />
          <ChartTooltip content={<ChartTooltipContent />} />
          <Area dataKey="pnl" type="monotone" stroke="var(--color-pnl)" fill="var(--color-pnl)" fillOpacity={0.15} />
        </AreaChart>
      </ChartContainer>
    </Section>
  );
}

function RecentOrders({ portfolioId }: { portfolioId: string }) {
  const orders = useQuery<BlotterRow[]>({
    queryKey: ["/orders", { portfolioId, limit: RECENT }],
    queryFn: () => api.get<BlotterRow[]>(`/orders?limit=${RECENT}&portfolio_id=${encodeURIComponent(portfolioId)}`),
  });
  const rows = orders.data ?? [];
  return (
    <Section title="Recent orders">
      <Table>
        <TableHeader>
          <TableRow><TableHead>Placed</TableHead><TableHead>Instrument</TableHead><TableHead>Side</TableHead><TableHead className="text-right">Qty</TableHead><TableHead className="text-right">Avg px</TableHead><TableHead>Status</TableHead></TableRow>
        </TableHeader>
        <TableBody>
          {rows.map((o) => (
            <TableRow key={o.order_id}>
              <TableCell className="num">{fmtLocal(o.created_at)}</TableCell>
              <TableCell>{o.instrument_symbol ?? o.instrument_id}</TableCell>
              <TableCell><SideBadge side={o.side} /></TableCell>
              <TableCell className="num text-right">{o.original_qty}</TableCell>
              <TableCell className="num text-right">{o.avg_px ?? "—"}</TableCell>
              <TableCell><OrderStatus status={o.status} /></TableCell>
            </TableRow>
          ))}
          {rows.length === 0 && <TableRow><TableCell colSpan={6} className="text-center text-muted-foreground">No orders.</TableCell></TableRow>}
        </TableBody>
      </Table>
    </Section>
  );
}

/** One portfolio at a glance: totals and positions, exposure, P&L history and its latest orders. */
export function PortfoliosPage({ me }: { me: Me }) {
  const viewable = me.portfolios.filter((p) => p.can_view);
  const [picked, setPicked] = useState<string | null>(null);
  const portfolioId = picked ?? pickDefaultPortfolio(viewable);
  if (portfolioId === null) return <Notice tone="info">You have no portfolio to view.</Notice>;
  return (
    <div className="flex flex-col gap-3">
      {viewable.length > 1 && (
        <div className="flex items-center gap-2">
          <PortfolioPicker portfolios={viewable} value={portfolioId} onChange={(id) => id && setPicked(id)} className="w-72" />
        </div>
      )}
      <div className="grid grid-cols-1 gap-3 lg:grid-cols-3">
        <div className="lg:col-span-2"><PnlChart portfolioId={portfolioId} /></div>
        <Exposure portfolioId={portfolioId} />
      </div>
      <PositionsPage me={me} portfolioId={portfolioId} />
      <RecentOrders portfolioId={portfolioId} />
    </div>
  );
}
