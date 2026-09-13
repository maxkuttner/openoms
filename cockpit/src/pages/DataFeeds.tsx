import { useState } from "react";
import { Badge, Button, Drawer, Group, Loader, Stack, Table, Text, Title } from "@mantine/core";
import { useQuery } from "@tanstack/react-query";
import { api } from "../api/client";
import { StreamHealthStrip } from "../components/StreamHealthStrip";
import { CredentialsPanel } from "../components/CredentialsPanel";
import type { FeedConnectionSummary, FeedSummary } from "../api/types";

// Market-data feeds — distinct from broker connections (execution). Shows live
// feed stream health, the configured feed connections (where credentials are
// managed), the ranked failover policy, and how many instruments each feed
// currently prices. The policy/coverage table is derived data and stays
// read-only; feed connection credentials are configured below via
// CredentialsPanel.
export function DataFeedsPage() {
  const { data, isLoading, error } = useQuery<FeedSummary[]>({
    queryKey: ["/admin/feeds"],
    queryFn: () => api.get<FeedSummary[]>("/admin/feeds"),
  });

  const connections = useQuery<FeedConnectionSummary[]>({
    queryKey: ["/admin/feed-connections"],
    queryFn: () => api.get<FeedConnectionSummary[]>("/admin/feed-connections"),
  });
  const [selected, setSelected] = useState<FeedConnectionSummary | null>(null);

  return (
    <Stack gap="lg">
      <StreamHealthStrip
        kind="feed"
        title="Live data feeds"
        emptyText="No data feeds running in this process."
      />

      <Stack gap="xs">
        <Title order={4}>Feed connections</Title>
        <Text size="sm" c="dimmed">
          Configure, test, and clear the API key each feed connection uses.
        </Text>
        {connections.isLoading ? (
          <Loader size="sm" />
        ) : (
          <Table striped highlightOnHover withTableBorder>
            <Table.Thead>
              <Table.Tr>
                <Table.Th>Code</Table.Th>
                <Table.Th>Provider</Table.Th>
                <Table.Th>Dataset</Table.Th>
                <Table.Th>Status</Table.Th>
                <Table.Th />
              </Table.Tr>
            </Table.Thead>
            <Table.Tbody>
              {(connections.data ?? []).map((row) => (
                <Table.Tr key={row.code}>
                  <Table.Td>{row.code}</Table.Td>
                  <Table.Td>{row.provider}</Table.Td>
                  <Table.Td>{row.dataset ?? <Text c="dimmed">—</Text>}</Table.Td>
                  <Table.Td>
                    <Badge color={row.status === "ACTIVE" ? "green" : "gray"} variant="light">
                      {row.status}
                    </Badge>
                  </Table.Td>
                  <Table.Td>
                    <Group justify="flex-end">
                      <Button size="xs" variant="light" onClick={() => setSelected(row)}>
                        Credentials
                      </Button>
                    </Group>
                  </Table.Td>
                </Table.Tr>
              ))}
              {(connections.data ?? []).length === 0 && (
                <Table.Tr>
                  <Table.Td colSpan={5}>
                    <Text c="dimmed" ta="center" py="md">No feed connections yet.</Text>
                  </Table.Td>
                </Table.Tr>
              )}
            </Table.Tbody>
          </Table>
        )}
      </Stack>

      <Stack gap="xs">
        <Group justify="space-between">
          <Title order={4}>Feed policy</Title>
          {isLoading && <Loader size="sm" />}
        </Group>
        <Text size="sm" c="dimmed">
          Ranked market-data sources per instrument class; lower rank wins, higher ranks are
          failover. Coverage is derived from each feed's own symbology — there is no mapping table.
        </Text>
        {error && <Text c="red">Failed to load feeds.</Text>}
        <Table striped highlightOnHover>
          <Table.Thead>
            <Table.Tr>
              <Table.Th>Feed</Table.Th>
              <Table.Th>Instrument class</Table.Th>
              <Table.Th>Rank</Table.Th>
              <Table.Th>Enabled</Table.Th>
              <Table.Th>Mapped instruments</Table.Th>
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {(data ?? []).map((f) => (
              <Table.Tr key={`${f.feed_code}/${f.instrument_class}`}>
                <Table.Td><b>{f.feed_code}</b></Table.Td>
                <Table.Td>{f.instrument_class}</Table.Td>
                <Table.Td>{f.rank}</Table.Td>
                <Table.Td>
                  <Badge color={f.enabled ? "green" : "gray"} variant="light">
                    {f.enabled ? "enabled" : "disabled"}
                  </Badge>
                </Table.Td>
                <Table.Td>{f.mapped_instruments.toLocaleString()}</Table.Td>
              </Table.Tr>
            ))}
            {!isLoading && (data ?? []).length === 0 && (
              <Table.Tr>
                <Table.Td colSpan={5}>
                  <Text c="dimmed" ta="center" py="md">
                    No feed policy configured.
                  </Text>
                </Table.Td>
              </Table.Tr>
            )}
          </Table.Tbody>
        </Table>
      </Stack>

      <Drawer
        opened={!!selected}
        onClose={() => setSelected(null)}
        title={selected ? `Credentials — ${selected.code}` : undefined}
        position="right"
        size="md"
      >
        {selected && <CredentialsPanel kind="feed" code={selected.code} providerCode={selected.provider} />}
      </Drawer>
    </Stack>
  );
}
