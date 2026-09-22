import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Loader, Paper, Select, SimpleGrid, Stack, Table, Text, Title, Group } from "@mantine/core";
import { tradeApi } from "../api/client";
import { numeric, columnHeader } from "../table";
import type { Me } from "../App";

// Mirrors src/positions.rs Position. mark, market_value, unrealized_pnl and
// mark_ts are None when no live mark exists for the instrument — an
// unpriceable/expired contract, or a feed that's down — and MUST stay
// nullable here. Defaulting them (in the type, or with `?? 0`) would turn an
// honest "we don't know" into a lie that reads as a real, zero-valued P&L.
export interface PositionRow {
  portfolio_id: string;
  instrument_id: string;
  net_qty: number;
  avg_cost: number;
  realized_pnl: number;
  updated_at: string;
  mark: number | null;
  market_value: number | null;
  unrealized_pnl: number | null;
  mark_ts: string | null;
}

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

  const positions = useQuery<PositionRow[]>({
    queryKey: ["/portfolios", portfolioId, "positions"],
    queryFn: () => tradeApi.get<PositionRow[]>(`/portfolios/${portfolioId}/positions`),
    enabled: portfolioId !== null,
    refetchInterval: 5000,
  });

  const rows = positions.data ?? [];

  // realized_pnl is never null; unrealized_pnl and market_value are null for
  // any position with no live mark (see the PositionRow comment above) — those
  // rows are excluded from their sums rather than treated as zero, and the
  // count is surfaced so the total never reads as more complete than it is.
  const realizedTotal = rows.reduce((sum, p) => sum + p.realized_pnl, 0);
  const pricedRows = rows.filter((p) => p.unrealized_pnl !== null);
  const unrealizedTotal = pricedRows.reduce((sum, p) => sum + (p.unrealized_pnl as number), 0);
  const marketValueTotal = rows
    .filter((p) => p.market_value !== null)
    .reduce((sum, p) => sum + (p.market_value as number), 0);
  const unpriced = rows.length - pricedRows.length;

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
          <SimpleGrid cols={{ base: 1, sm: 3 }}>
            <SummaryCard label="Unrealized P&L" value={unrealizedTotal.toFixed(2)} pnlColor />
            <SummaryCard label="Realized P&L" value={realizedTotal.toFixed(2)} pnlColor />
            <SummaryCard label="Market value" value={marketValueTotal.toFixed(2)} />
          </SimpleGrid>
          {unpriced > 0 && (
            <Text size="xs" c="dimmed">
              {unpriced} of {rows.length} position{unpriced === 1 ? "" : "s"} {unpriced === 1 ? "has" : "have"} no
              live mark and {unpriced === 1 ? "is" : "are"} excluded from these totals.
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
                <Table.Td>{p.mark_ts ? new Date(p.mark_ts).toLocaleString() : "—"}</Table.Td>
              </Table.Tr>
            ))}
            {rows.length === 0 && (
              <Table.Tr>
                <Table.Td colSpan={8}>
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
