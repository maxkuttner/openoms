import { useState } from "react";
import { Stack, Group, Title, Select, Button, Table, Badge, Text, Card } from "@mantine/core";
import { api } from "../api/client";
import { useList, useApiMutation } from "../api/hooks";
import type { BrokerConnection, ReconSummary } from "../api/types";

const KIND_COLOR: Record<string, string> = {
  qty_mismatch: "orange",
  missing_in_custodian: "red",
  missing_in_oms: "red",
  unresolved_custodian_security: "yellow",
};

export function ReconciliationPage() {
  const connections = useList<BrokerConnection>("/admin/broker-connections");
  const [conn, setConn] = useState<string | null>(null);
  const [result, setResult] = useState<ReconSummary | null>(null);

  const run = useApiMutation(
    (code: string) => api.post<ReconSummary>("/admin/recon/run", { broker_connection_code: code }),
    { invalidate: ["/admin/recon/runs"], success: "Reconciliation complete" },
  );

  return (
    <Stack>
      <Title order={3}>Reconciliation — custodian vs OMS</Title>
      <Text size="sm" c="dimmed">
        Read the custodian's holdings and compare to our positions. Breaks are discrepancies to investigate.
      </Text>

      <Group align="flex-end">
        <Select
          label="Broker connection (custodian)"
          placeholder="pick a connection"
          data={(connections.data ?? []).map((c) => ({ value: c.code, label: `${c.code} (${c.broker_code}/${c.environment})` }))}
          value={conn}
          onChange={setConn}
          w={320}
        />
        <Button
          disabled={!conn}
          loading={run.isPending}
          onClick={() => conn && run.mutate(conn, { onSuccess: (d: any) => setResult(d) })}
        >
          Run reconciliation
        </Button>
      </Group>

      {result && (
        <Stack gap="sm">
          <Group gap="md">
            <Card withBorder padding="xs"><Text size="sm">OMS instruments: <b>{result.oms_count}</b></Text></Card>
            <Card withBorder padding="xs"><Text size="sm">Custodian holdings: <b>{result.custodian_count}</b></Text></Card>
            <Card withBorder padding="xs">
              <Text size="sm">Breaks: <Badge color={result.break_count ? "red" : "green"}>{result.break_count}</Badge></Text>
            </Card>
          </Group>

          <Table striped withTableBorder>
            <Table.Thead>
              <Table.Tr>
                <Table.Th>Instrument</Table.Th>
                <Table.Th>Symbol</Table.Th>
                <Table.Th ta="right">OMS qty</Table.Th>
                <Table.Th ta="right">Custodian qty</Table.Th>
                <Table.Th ta="right">Diff</Table.Th>
                <Table.Th>Break</Table.Th>
              </Table.Tr>
            </Table.Thead>
            <Table.Tbody>
              {result.breaks.map((b, i) => (
                <Table.Tr key={i}>
                  <Table.Td>{b.instrument_id ?? <Text c="dimmed">—</Text>}</Table.Td>
                  <Table.Td>{b.symbol ?? <Text c="dimmed">—</Text>}</Table.Td>
                  <Table.Td ta="right">{b.oms_qty}</Table.Td>
                  <Table.Td ta="right">{b.custodian_qty}</Table.Td>
                  <Table.Td ta="right">{b.diff}</Table.Td>
                  <Table.Td><Badge color={KIND_COLOR[b.kind] ?? "gray"}>{b.kind}</Badge></Table.Td>
                </Table.Tr>
              ))}
              {result.breaks.length === 0 && (
                <Table.Tr>
                  <Table.Td colSpan={6}>
                    <Text c="green" ta="center" py="md">Clean — no breaks.</Text>
                  </Table.Td>
                </Table.Tr>
              )}
            </Table.Tbody>
          </Table>
        </Stack>
      )}
    </Stack>
  );
}
