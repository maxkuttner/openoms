import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Loader, Paper, Select, SimpleGrid, Stack, Table, Text, Title, Group } from "@mantine/core";
import { tradeApi } from "../api/client";
import { useLive } from "../api/liveUpdates";
import { numeric, columnHeader } from "../table";
import { formatFees } from "../../fees";
import type { Me } from "../App";
import type { MarkRow } from "../types";
import { totalsByCurrency } from "../../positions";
import type { PositionRow } from "../../positions";

export type { PositionRow };

// mark, market_value, unrealized_pnl and mark_ts are null when no live quote
// exists — an unpriceable or expired contract, or a feed that is down. Render
// them as "—", NEVER as 0: a zero mark silently misstates P&L, and unrealized
// P&L is exactly the number someone glances at before deciding something.
const num = (v: number | null | undefined) => (v === null || v === undefined ? "—" : v);

/// One derived total for the summary row. `pnlColor` on a P&L figure, omitted
/// for market value (a size, not a gain or loss).
function SummaryCard({ label, value, pnlColor }: { label: string; value: string; pnlColor?: boolean }) {
  const signed = pnlColor ? (value.startsWith("-") ? "offer" : value === "0.00" ? undefined : "depth") : undefined;
  return (
    <Paper withBorder p="md">
      <Text size="xs" c="dimmed" tt="uppercase" fw={600} style={{ letterSpacing: "0.05em" }}>
        {label}
      </Text>
      <Text size="xl" fw={600} c={signed} mt={4}>
        {value}
      </Text>
    </Paper>
  );
}

export function PositionsPage({ me }: { me: Me }) {
  const viewable = me.portfolios.filter((p) => p.can_view);
  const [portfolioId, setPortfolioId] = useState<string | null>(viewable[0]?.portfolio_id ?? null);

  const live = useLive();
  const positions = useQuery<PositionRow[]>({
    queryKey: ["/portfolios", portfolioId, "positions"],
    queryFn: () => tradeApi.get<PositionRow[]>(`/portfolios/${portfolioId}/positions`),
    enabled: portfolioId !== null,
    refetchInterval: live ? false : 5000,
  });

  const rows = positions.data ?? [];

  const heldInstrumentIds = rows.map((p) => p.instrument_id);
  const marks = useQuery<MarkRow[]>({
    queryKey: ["/marks", heldInstrumentIds],
    queryFn: () => tradeApi.get<MarkRow[]>(`/marks?instrument_ids=${heldInstrumentIds.join(",")}`),
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

  return (
    <Stack>
      <Group justify="space-between" align="flex-end">
        <Title order={3}>Positions</Title>
        {viewable.length > 1 && (
          <Select
            label="Portfolio"
            data={viewable.map((p) => ({ value: p.portfolio_id, label: p.code }))}
            value={portfolioId}
            onChange={setPortfolioId}
            clearable={false}
            searchable
            w={200}
          />
        )}
      </Group>

      {rows.length > 0 && (
        <Stack gap={4}>
          {[...totals].map(([ccy, t]) => (
            <SimpleGrid key={ccy} cols={{ base: 1, sm: 5 }}>
              <SummaryCard label={`Unrealized P&L · ${ccy}`} value={t.unrealized.toFixed(2)} pnlColor />
              <SummaryCard label={`Realized P&L · ${ccy}`} value={t.realized.toFixed(2)} pnlColor />
              <SummaryCard label={`Market value · ${ccy}`} value={t.marketValue.toFixed(2)} />
              <SummaryCard label={`Day P&L · ${ccy}`} value={t.day.toFixed(2)} pnlColor />
              <SummaryCard label={`Gross exposure · ${ccy}`} value={t.grossExposure.toFixed(2)} />
            </SimpleGrid>
          ))}
          {unpriced > 0 && (
            <Text size="xs" c="dimmed">
              {unpriced} of {rows.length} position{rows.length === 1 ? "" : "s"} {unpriced === 1 ? "has" : "have"} no
              live mark and {unpriced === 1 ? "is" : "are"} excluded from these totals.
            </Text>
          )}
          {dayPnlUnpriced > 0 && (
            <Text size="xs" c="dimmed">
              {dayPnlUnpriced} of {rows.length} position{rows.length === 1 ? "" : "s"} {dayPnlUnpriced === 1 ? "is" : "are"} missing
              a live mark or a previous close and {dayPnlUnpriced === 1 ? "is" : "are"} excluded from Day P&L.
            </Text>
          )}
        </Stack>
      )}

      {portfolioId === null ? (
        <Text c="dimmed">No portfolio to view.</Text>
      ) : positions.isLoading ? (
        <Loader />
      ) : (
        <Table striped highlightOnHover withTableBorder verticalSpacing={6} fz="sm">
          <Table.Thead>
            <Table.Tr>
              <Table.Th style={columnHeader}>Instrument</Table.Th>
              <Table.Th style={{ ...columnHeader, ...numeric }}>Net qty</Table.Th>
              <Table.Th style={{ ...columnHeader, ...numeric }}>Avg cost</Table.Th>
              <Table.Th style={{ ...columnHeader, ...numeric }}>Mark</Table.Th>
              <Table.Th style={{ ...columnHeader, ...numeric }}>Market value</Table.Th>
              <Table.Th style={{ ...columnHeader, ...numeric }}>Unrealized P&L</Table.Th>
              <Table.Th style={{ ...columnHeader, ...numeric }}>Realized P&L</Table.Th>
              <Table.Th style={{ ...columnHeader, ...numeric }}>Other fees</Table.Th>
              <Table.Th style={columnHeader}>Mark time</Table.Th>
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {rows.map((p) => (
              <Table.Tr key={p.instrument_id}>
                <Table.Td>{p.instrument_id}</Table.Td>
                <Table.Td style={numeric}>{p.net_qty}</Table.Td>
                <Table.Td style={numeric}>{p.avg_cost}</Table.Td>
                <Table.Td style={numeric}>{num(p.mark)}</Table.Td>
                <Table.Td style={numeric}>{num(p.market_value)}</Table.Td>
                <Table.Td style={numeric}>{num(p.unrealized_pnl)}</Table.Td>
                <Table.Td style={numeric}>{p.realized_pnl}</Table.Td>
                <Table.Td style={numeric}>{formatFees(p.other_fees)}</Table.Td>
                <Table.Td>{p.mark_ts ? new Date(p.mark_ts).toLocaleString() : "—"}</Table.Td>
              </Table.Tr>
            ))}
            {rows.length === 0 && (
              <Table.Tr>
                <Table.Td colSpan={9}>
                  <Text c="dimmed" ta="center" py="md">
                    No positions.
                  </Text>
                </Table.Td>
              </Table.Tr>
            )}
          </Table.Tbody>
        </Table>
      )}
    </Stack>
  );
}
