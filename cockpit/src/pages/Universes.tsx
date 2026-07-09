import { useState, useEffect } from "react";
import {
  Stack, Title, Text, Table, Badge, Loader, Group, Button, Modal,
  NumberInput, Switch, Alert, Code,
} from "@mantine/core";
import { useQuery } from "@tanstack/react-query";
import { api } from "../api/client";
import { useApiMutation, notifyError } from "../api/hooks";
import type { UniverseSummary, EstimateResponse } from "../api/types";

const STATUS_COLOR: Record<string, string> = {
  SEEDED: "green",
  SEEDING: "blue",
  PENDING: "gray",
  ERROR: "red",
};

function fmtDate(iso: string | null): string {
  if (!iso) return "—";
  return new Date(iso).toLocaleString();
}

function SeedModal({
  universe,
  onClose,
}: {
  universe: UniverseSummary | null;
  onClose: () => void;
}) {
  const [estimate, setEstimate] = useState<EstimateResponse | null>(null);
  const [estimating, setEstimating] = useState(false);
  const [maxCost, setMaxCost] = useState<number | "">("");
  const [enrich, setEnrich] = useState(true);

  const opened = universe !== null;
  const code = universe?.code;

  // Fetch the free estimate whenever the modal opens for a universe.
  useEffect(() => {
    if (!code) return;
    let live = true;
    setEstimate(null);
    setEstimating(true);
    api
      .get<EstimateResponse>(`/admin/universes/${code}/estimate`)
      .then((e) => { if (live) setEstimate(e); })
      .catch((err) => { if (live) notifyError(err); })
      .finally(() => { if (live) setEstimating(false); });
    return () => { live = false; };
  }, [code]);

  const seed = useApiMutation(
    () =>
      api.post(`/admin/universes/${code}/seed`, {
        max_cost: maxCost === "" ? null : maxCost,
        enrich,
      }),
    {
      invalidate: ["/admin/universes"],
      success: "Seeding started",
      onDone: () => close(),
    },
  );

  function close() {
    setEstimate(null);
    setMaxCost("");
    setEnrich(true);
    onClose();
  }

  return (
    <Modal opened={opened} onClose={close} title={`Seed ${code ?? ""}`} centered>
      <Stack>
        <Text size="sm" c="dimmed">{universe?.description}</Text>

        <Alert color="blue" variant="light" title="Estimated cost (Databento)">
          {estimating && <Loader size="xs" />}
          {estimate && (
            <Text size="sm">
              <b>${estimate.usd.toFixed(4)}</b>
              {estimate.symbol_count != null && ` · ${estimate.symbol_count} symbol(s)`}
            </Text>
          )}
        </Alert>

        <NumberInput
          label="Max cost (USD)"
          description="Abort if the estimate exceeds this. Leave blank for no gate."
          placeholder="no gate"
          value={maxCost}
          onChange={(v) => setMaxCost(typeof v === "number" ? v : "")}
          min={0}
          decimalScale={4}
          step={0.01}
        />

        <Switch
          label="Run enrichment (OpenFIGI)"
          checked={enrich}
          onChange={(e) => setEnrich(e.currentTarget.checked)}
        />

        <Text size="xs" c="dimmed">
          Seeding runs in the background. Watch the <Code>Status</Code> column flip
          to <Code>SEEDED</Code> (or <Code>ERROR</Code>).
        </Text>

        <Group justify="flex-end">
          <Button variant="default" onClick={close}>Cancel</Button>
          <Button loading={seed.isPending} disabled={estimating} onClick={() => seed.mutate(undefined)}>
            Start seeding
          </Button>
        </Group>
      </Stack>
    </Modal>
  );
}

export function UniversesPage() {
  const [target, setTarget] = useState<UniverseSummary | null>(null);
  const { data, isLoading, error } = useQuery<UniverseSummary[]>({
    queryKey: ["/admin/universes"],
    queryFn: () => api.get<UniverseSummary[]>("/admin/universes"),
    // Poll every 3s while anything is mid-seed so the status column self-updates.
    refetchInterval: (q) =>
      (q.state.data ?? []).some((u) => u.status === "SEEDING") ? 3000 : false,
  });

  const anySeeding = (data ?? []).some((u) => u.status === "SEEDING");

  return (
    <Stack>
      <Title order={3}>Instrument universes</Title>
      <Text size="sm" c="dimmed">
        The catalog of provider datasets available for seeding — which are enabled,
        their seed status, and when each was last loaded. Seed one with the button
        on its row.
      </Text>
      {anySeeding && (
        <Text size="xs" c="blue">A universe is seeding — this list refreshes automatically.</Text>
      )}

      {isLoading && <Loader />}
      {error && <Text c="red">Failed to load universes.</Text>}

      {data && (
        <Table striped withTableBorder>
          <Table.Thead>
            <Table.Tr>
              <Table.Th>Code</Table.Th>
              <Table.Th>Category</Table.Th>
              <Table.Th>Dataset</Table.Th>
              <Table.Th>Enabled</Table.Th>
              <Table.Th>Status</Table.Th>
              <Table.Th ta="right">Instruments</Table.Th>
              <Table.Th>Last loaded</Table.Th>
              <Table.Th />
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {data.map((u) => (
              <Table.Tr key={u.code}>
                <Table.Td>
                  <Text fw={600} size="sm">{u.code}</Text>
                  {u.description && <Text size="xs" c="dimmed">{u.description}</Text>}
                </Table.Td>
                <Table.Td>{u.category}</Table.Td>
                <Table.Td>
                  <Text size="sm">{u.dataset}</Text>
                  {u.option_dataset && <Text size="xs" c="dimmed">+ {u.option_dataset}</Text>}
                </Table.Td>
                <Table.Td>
                  <Badge color={u.enabled ? "teal" : "gray"} variant={u.enabled ? "filled" : "light"}>
                    {u.enabled ? "yes" : "no"}
                  </Badge>
                </Table.Td>
                <Table.Td>
                  <Group gap={6}>
                    <Badge color={STATUS_COLOR[u.status] ?? "gray"}>{u.status}</Badge>
                    {u.status === "ERROR" && u.last_error && (
                      <Text size="xs" c="red" lineClamp={1} maw={200} title={u.last_error}>
                        {u.last_error}
                      </Text>
                    )}
                  </Group>
                </Table.Td>
                <Table.Td ta="right">
                  {u.instrument_count != null ? u.instrument_count.toLocaleString() : "—"}
                </Table.Td>
                <Table.Td><Text size="sm">{fmtDate(u.last_seeded_at)}</Text></Table.Td>
                <Table.Td ta="right">
                  <Button
                    size="xs"
                    variant="light"
                    loading={u.status === "SEEDING"}
                    onClick={() => setTarget(u)}
                  >
                    Seed
                  </Button>
                </Table.Td>
              </Table.Tr>
            ))}
            {data.length === 0 && (
              <Table.Tr>
                <Table.Td colSpan={8}>
                  <Text c="dimmed" ta="center" py="md">No universes in the catalog.</Text>
                </Table.Td>
              </Table.Tr>
            )}
          </Table.Tbody>
        </Table>
      )}

      <SeedModal universe={target} onClose={() => setTarget(null)} />
    </Stack>
  );
}
