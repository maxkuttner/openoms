import { useState } from "react";
import { Anchor, Group, Table, Text, TextInput, Title, Badge, Loader } from "@mantine/core";
import { Link as RouterLink } from "react-router-dom";
import { useDebouncedValue } from "@mantine/hooks";
import { useQuery } from "@tanstack/react-query";
import { api } from "../api/client";
import type { InstrumentSummary } from "../api/types";

// Read-only browser over the seeded master catalog. Instruments are seeded
// broker-first (a universe load per broker connection); pricing coverage is derived per feed at
// startup, not seeded — there is no in-UI seeding flow. Server-side search
// (debounced) against /admin/instruments.
export function InstrumentsPage() {
  const [search, setSearch] = useState("");
  const [debounced] = useDebouncedValue(search, 250);
  const { data, isLoading, error } = useQuery<InstrumentSummary[]>({
    queryKey: ["/admin/instruments", debounced],
    queryFn: () =>
      api.get<InstrumentSummary[]>(
        `/admin/instruments?limit=200${debounced ? `&search=${encodeURIComponent(debounced)}` : ""}`,
      ),
  });

  return (
    <>
      <Group justify="space-between" mb="md">
        <Title order={3}>Instruments</Title>
        {isLoading && <Loader size="sm" />}
      </Group>
      <Text size="sm" c="dimmed" mb="md">
        Loaded from each broker's catalog — see the instrument universe on Broker connections. Which
        of these a data feed can price is derived from the feed's own symbology at startup.
      </Text>
      <TextInput
        placeholder="Search symbol or name…"
        value={search}
        onChange={(e) => setSearch(e.currentTarget.value)}
        mb="md"
        maw={360}
      />
      {error && <Text c="red">Failed to load instruments.</Text>}
      <Table striped highlightOnHover>
        <Table.Thead>
          <Table.Tr>
            <Table.Th>Symbol</Table.Th>
            <Table.Th>Name</Table.Th>
            <Table.Th>Venue</Table.Th>
            <Table.Th>Asset class</Table.Th>
            <Table.Th>Status</Table.Th>
          </Table.Tr>
        </Table.Thead>
        <Table.Tbody>
          {(data ?? []).map((i) => (
            <Table.Tr key={i.id}>
              <Table.Td><b>{i.symbol}</b></Table.Td>
              <Table.Td>{i.name}</Table.Td>
              <Table.Td>{i.venue}</Table.Td>
              <Table.Td>{i.asset_class}</Table.Td>
              <Table.Td>
                <Badge color={i.status === "ACTIVE" ? "green" : "gray"} variant="light">
                  {i.status}
                </Badge>
              </Table.Td>
            </Table.Tr>
          ))}
          {!isLoading && (data ?? []).length === 0 && (
            <Table.Tr>
              <Table.Td colSpan={5}>
                <Text c="dimmed" ta="center" py="md">
                  No instruments yet — load a broker's universe from{" "}
                  <Anchor component={RouterLink} to="/broker-connections">
                    Broker connections
                  </Anchor>
                  .
                </Text>
              </Table.Td>
            </Table.Tr>
          )}
        </Table.Tbody>
      </Table>
    </>
  );
}
