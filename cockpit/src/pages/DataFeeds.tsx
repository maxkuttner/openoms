import { Badge, Group, Loader, Stack, Table, Text, Title } from "@mantine/core";
import { useQuery } from "@tanstack/react-query";
import { api } from "../api/client";
import { StreamHealthStrip } from "../components/StreamHealthStrip";
import type { FeedSummary } from "../api/types";

// Market-data feeds — distinct from broker connections (execution). Shows live
// feed stream health plus the configured ranked-failover policy and how many
// instruments each feed currently prices. Feeds are seeded/mapped from the CLI
// (`make map-feed FEED=…`), so this view is read-only.
export function DataFeedsPage() {
  const { data, isLoading, error } = useQuery<FeedSummary[]>({
    queryKey: ["/admin/feeds"],
    queryFn: () => api.get<FeedSummary[]>("/admin/feeds"),
  });

  return (
    <Stack gap="lg">
      <StreamHealthStrip
        kind="feed"
        title="Live data feeds"
        emptyText="No data feeds running in this process."
      />

      <Stack gap="xs">
        <Group justify="space-between">
          <Title order={4}>Feed policy</Title>
          {isLoading && <Loader size="sm" />}
        </Group>
        <Text size="sm" c="dimmed">
          Ranked market-data sources per instrument class; lower rank wins, higher ranks are
          failover. Mapped via <code>make map-feed FEED=…</code>.
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
    </Stack>
  );
}
