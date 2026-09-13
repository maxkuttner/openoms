import { useState } from "react";
import { Button, Drawer, Group, Loader, Stack, Table, Text, Title } from "@mantine/core";
import { CrudResource } from "../components/CrudResource";
import { CredentialsPanel } from "../components/CredentialsPanel";
import { StreamHealthStrip } from "../components/StreamHealthStrip";
import { useList } from "../api/hooks";
import type { BrokerConnection } from "../api/types";

const STATUS = [
  { value: "ACTIVE", label: "ACTIVE" },
  { value: "INACTIVE", label: "INACTIVE" },
];

export function BrokerConnectionsPage() {
  const [selected, setSelected] = useState<BrokerConnection | null>(null);
  const list = useList<BrokerConnection>("/admin/broker-connections");

  return (
    <Stack gap="lg">
      <StreamHealthStrip
        kind="execution"
        title="Live execution streams"
        emptyText="No execution streams — no broker credentials configured in this process."
      />
      <CrudResource
      title="Broker connections"
      path="/admin/broker-connections"
      idKey="code"
      editable
      columns={[
        { key: "code", label: "Code" },
        { key: "broker_code", label: "Broker" },
        { key: "environment", label: "Environment" },
        { key: "status", label: "Status" },
      ]}
      fields={[
        { name: "code", label: "Code", required: true, inEdit: false },
        { name: "broker_code", label: "Broker code", required: true },
        {
          name: "environment",
          label: "Environment",
          type: "select",
          required: true,
          options: [
            { value: "PAPER", label: "PAPER" },
            { value: "LIVE", label: "LIVE" },
          ],
        },
        { name: "status", label: "Status", type: "select", required: true, options: STATUS },
      ]}
      />

      <Stack gap="xs">
        <Title order={4}>Credentials</Title>
        <Text size="sm" c="dimmed">
          Configure, test, and clear the API key or FIX session credential each connection uses.
        </Text>
        {list.isLoading ? (
          <Loader size="sm" />
        ) : (
          <Table striped highlightOnHover withTableBorder>
            <Table.Thead>
              <Table.Tr>
                <Table.Th>Code</Table.Th>
                <Table.Th>Broker</Table.Th>
                <Table.Th>Environment</Table.Th>
                <Table.Th />
              </Table.Tr>
            </Table.Thead>
            <Table.Tbody>
              {(list.data ?? []).map((row) => (
                <Table.Tr key={row.code}>
                  <Table.Td>{row.code}</Table.Td>
                  <Table.Td>{row.broker_code}</Table.Td>
                  <Table.Td>{row.environment}</Table.Td>
                  <Table.Td>
                    <Group justify="flex-end">
                      <Button size="xs" variant="light" onClick={() => setSelected(row)}>
                        Credentials
                      </Button>
                    </Group>
                  </Table.Td>
                </Table.Tr>
              ))}
              {(list.data ?? []).length === 0 && (
                <Table.Tr>
                  <Table.Td colSpan={4}>
                    <Text c="dimmed" ta="center" py="md">No broker connections yet.</Text>
                  </Table.Td>
                </Table.Tr>
              )}
            </Table.Tbody>
          </Table>
        )}
      </Stack>

      <Drawer
        opened={!!selected}
        onClose={() => setSelected(null)}
        title={selected ? `Credentials — ${selected.code}` : undefined}
        position="right"
        size="md"
      >
        {selected && <CredentialsPanel kind="broker" code={selected.code} providerCode={selected.broker_code} />}
      </Drawer>
    </Stack>
  );
}
