import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Loader, Select, Stack, Table, Text, Title, Group } from "@mantine/core";
import { tradeApi } from "../api/client";
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

      {portfolioId === null ? (
        <Text c="dimmed">No portfolio to view.</Text>
      ) : positions.isLoading ? (
        <Loader />
      ) : (
        <Table striped highlightOnHover withTableBorder>
          <Table.Thead>
            <Table.Tr>
              <Table.Th>Instrument</Table.Th>
              <Table.Th ta="right">Net qty</Table.Th>
              <Table.Th ta="right">Avg cost</Table.Th>
              <Table.Th ta="right">Mark</Table.Th>
              <Table.Th ta="right">Market value</Table.Th>
              <Table.Th ta="right">Unrealized P&L</Table.Th>
              <Table.Th ta="right">Realized P&L</Table.Th>
              <Table.Th>Mark time</Table.Th>
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {rows.map((p) => (
              <Table.Tr key={p.instrument_id}>
                <Table.Td>{p.instrument_id}</Table.Td>
                <Table.Td ta="right">{p.net_qty}</Table.Td>
                <Table.Td ta="right">{p.avg_cost}</Table.Td>
                <Table.Td ta="right">{num(p.mark)}</Table.Td>
                <Table.Td ta="right">{num(p.market_value)}</Table.Td>
                <Table.Td ta="right">{num(p.unrealized_pnl)}</Table.Td>
                <Table.Td ta="right">{p.realized_pnl}</Table.Td>
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
