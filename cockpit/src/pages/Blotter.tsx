import { useState } from "react";
import { Group, Title, Table, Select, Loader, Text, Badge, Stack, Button, Tooltip } from "@mantine/core";
import { useList } from "../api/hooks";
import { InstrumentSelect } from "../components/InstrumentSelect";
import type { BlotterRow, Portfolio, Principal } from "../api/types";

const STATUSES = ["submitted", "routed", "partially_filled", "filled", "canceled", "rejected", "expired"].map(
  (s) => ({ value: s, label: s }),
);

const STATUS_COLOR: Record<string, string> = {
  filled: "green",
  partially_filled: "teal",
  routed: "blue",
  submitted: "gray",
  canceled: "orange",
  rejected: "red",
  expired: "yellow",
};

export function BlotterPage() {
  const [status, setStatus] = useState<string | null>(null);
  const [portfolioId, setPortfolioId] = useState<string | null>(null);
  const [principalId, setPrincipalId] = useState<string | null>(null);
  const [instrument, setInstrument] = useState<string | null>(null);

  const portfolios = useList<Portfolio>("/admin/portfolios");
  const principals = useList<Principal>("/admin/principals");

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

  return (
    <Stack>
      <Title order={3}>Blotter — who is trading what</Title>
      <Group align="flex-end">
        <Select label="Status" data={STATUSES} value={status} onChange={setStatus} clearable w={160} />
        <Select
          label="Portfolio"
          data={(portfolios.data ?? []).map((p) => ({ value: p.id, label: p.code }))}
          value={portfolioId}
          onChange={setPortfolioId}
          clearable
          searchable
          w={180}
        />
        <Select
          label="Principal"
          data={(principals.data ?? []).map((p) => ({ value: p.id, label: p.code }))}
          value={principalId}
          onChange={setPrincipalId}
          clearable
          searchable
          w={180}
        />
        <div style={{ width: 240 }}>
          <InstrumentSelect label="Instrument" value={instrument} onChange={setInstrument} />
        </div>
        <Button variant="default" onClick={clear}>Clear</Button>
      </Group>

      {orders.isLoading ? (
        <Loader />
      ) : (
        <Table striped highlightOnHover withTableBorder>
          <Table.Thead>
            <Table.Tr>
              <Table.Th>Time</Table.Th>
              <Table.Th>Principal</Table.Th>
              <Table.Th>Portfolio</Table.Th>
              <Table.Th>Instrument</Table.Th>
              <Table.Th>Side</Table.Th>
              <Table.Th>Type</Table.Th>
              <Table.Th>Status</Table.Th>
              <Table.Th ta="right">Qty</Table.Th>
              <Table.Th ta="right">Cum</Table.Th>
              <Table.Th ta="right">Leaves</Table.Th>
              <Table.Th ta="right">Avg px</Table.Th>
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {(orders.data ?? []).map((o) => (
              <Table.Tr key={o.order_id}>
                <Table.Td>{new Date(o.created_at).toLocaleString()}</Table.Td>
                <Table.Td>{o.principal_code}</Table.Td>
                <Table.Td>{o.portfolio_code}</Table.Td>
                <Table.Td>
                  <Tooltip label={o.instrument_name ?? `id ${o.instrument_id}`} disabled={!o.instrument_name}>
                    <Text size="sm">{o.instrument_symbol ?? o.instrument_id}</Text>
                  </Tooltip>
                </Table.Td>
                <Table.Td>
                  <Badge color={o.side === "buy" ? "green" : "red"} variant="light">{o.side}</Badge>
                </Table.Td>
                <Table.Td>{o.order_type}</Table.Td>
                <Table.Td>
                  <Badge color={STATUS_COLOR[o.status] ?? "gray"}>{o.status}</Badge>
                </Table.Td>
                <Table.Td ta="right">{o.original_qty}</Table.Td>
                <Table.Td ta="right">{o.cum_qty}</Table.Td>
                <Table.Td ta="right">{o.leaves_qty}</Table.Td>
                <Table.Td ta="right">{o.avg_px ?? "—"}</Table.Td>
              </Table.Tr>
            ))}
            {(orders.data ?? []).length === 0 && (
              <Table.Tr>
                <Table.Td colSpan={11}>
                  <Text c="dimmed" ta="center" py="md">No orders match.</Text>
                </Table.Td>
              </Table.Tr>
            )}
          </Table.Tbody>
        </Table>
      )}
    </Stack>
  );
}
