import { useState, useEffect } from "react";
import {
  Stack, Title, Text, Table, Badge, Loader, Group, Button, Modal,
  NumberInput, Switch, Alert, Code, TextInput, Checkbox, Pill, ScrollArea,
} from "@mantine/core";
import { useQuery } from "@tanstack/react-query";
import { api } from "../api/client";
import { useApiMutation, notifyError } from "../api/hooks";
import type { UniverseSummary, EstimateResponse, UnderlyingCandidate } from "../api/types";

const STATUS_COLOR: Record<string, string> = {
  SEEDED: "green",
  SEEDING: "blue",
  PARTIAL: "orange",
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

  const [estimateFailed, setEstimateFailed] = useState(false);

  // Fetch the free estimate whenever the modal opens. Best-effort: Databento's
  // get_cost is flaky for definition schema, and the estimate is informational
  // (definitions are ~free; the seed doesn't need it unless a max_cost is set).
  // So on failure we degrade to "unavailable" rather than a blocking error toast.
  useEffect(() => {
    if (!code) return;
    let live = true;
    setEstimate(null);
    setEstimateFailed(false);
    setEstimating(true);
    api
      .get<EstimateResponse>(`/admin/universes/${code}/estimate`)
      .then((e) => { if (live) setEstimate(e); })
      .catch(() => { if (live) setEstimateFailed(true); })
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

        <Alert color={estimateFailed ? "gray" : "blue"} variant="light" title="Estimated cost (Databento)">
          {estimating && <Loader size="xs" />}
          {estimate && (
            <Text size="sm">
              <b>${estimate.usd.toFixed(4)}</b>
              {estimate.symbol_count != null && ` · ${estimate.symbol_count} symbol(s)`}
            </Text>
          )}
          {estimateFailed && (
            <Text size="sm" c="dimmed">
              Estimate unavailable (Databento cost API). Definition data is ~free; seeding is unaffected.
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

function EditUnderlyingsModal({
  universe,
  onClose,
}: {
  universe: UniverseSummary | null;
  onClose: () => void;
}) {
  const code = universe?.code;
  const opened = universe !== null;
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [search, setSearch] = useState("");
  const [candidates, setCandidates] = useState<UnderlyingCandidate[]>([]);
  const [loadingSel, setLoadingSel] = useState(false);
  const [searching, setSearching] = useState(false);

  // Load the current selection when the modal opens.
  useEffect(() => {
    if (!code) return;
    let live = true;
    setSearch("");
    setCandidates([]);
    setLoadingSel(true);
    api
      .get<string[]>(`/admin/universes/${code}/symbols`)
      .then((s) => { if (live) setSelected(new Set(s)); })
      .catch((e) => { if (live) notifyError(e); })
      .finally(() => { if (live) setLoadingSel(false); });
    return () => { live = false; };
  }, [code]);

  // Debounced candidate search.
  useEffect(() => {
    if (!opened) return;
    let live = true;
    const q = search.trim();
    const t = setTimeout(() => {
      setSearching(true);
      api
        .get<UnderlyingCandidate[]>(`/admin/underlyings?limit=50${q ? `&search=${encodeURIComponent(q)}` : ""}`)
        .then((c) => { if (live) setCandidates(c); })
        .catch((e) => { if (live) notifyError(e); })
        .finally(() => { if (live) setSearching(false); });
    }, 250);
    return () => { live = false; clearTimeout(t); };
  }, [search, opened]);

  const save = useApiMutation(
    () => api.put(`/admin/universes/${code}/symbols`, { symbols: [...selected] }),
    { invalidate: ["/admin/universes"], success: "Underlyings saved", onDone: onClose },
  );

  function toggle(sym: string, on: boolean) {
    setSelected((prev) => {
      const next = new Set(prev);
      if (on) next.add(sym); else next.delete(sym);
      return next;
    });
  }

  return (
    <Modal opened={opened} onClose={onClose} title={`Underlyings — ${code ?? ""}`} size="lg" centered>
      <Stack>
        <Text size="sm" c="dimmed">
          Pick the underlyings whose option chains to seed. Candidates are equities
          already in the master. Only the chosen chains are loaded — never the whole tape.
        </Text>

        <div>
          <Text size="xs" fw={600} mb={4}>Selected ({selected.size})</Text>
          {loadingSel ? <Loader size="xs" /> : selected.size === 0 ? (
            <Text size="xs" c="dimmed">None selected — this OPTION universe can't be seeded until you pick at least one.</Text>
          ) : (
            <Group gap={6}>
              {[...selected].sort().map((s) => (
                <Pill key={s} withRemoveButton onRemove={() => toggle(s, false)}>{s}</Pill>
              ))}
            </Group>
          )}
        </div>

        <TextInput
          label="Search equities"
          placeholder="ticker or name…"
          value={search}
          onChange={(e) => setSearch(e.currentTarget.value)}
          rightSection={searching ? <Loader size="xs" /> : null}
        />

        <ScrollArea.Autosize mah={280}>
          <Stack gap={4}>
            {candidates.map((c) => (
              <Checkbox
                key={`${c.symbol}/${c.venue}`}
                checked={selected.has(c.symbol)}
                onChange={(e) => toggle(c.symbol, e.currentTarget.checked)}
                label={<Text size="sm">{c.symbol} <Text span c="dimmed" size="xs">· {c.venue} · {c.name}</Text></Text>}
              />
            ))}
            {candidates.length === 0 && !searching && (
              <Text size="xs" c="dimmed" py="sm">No matches.</Text>
            )}
          </Stack>
        </ScrollArea.Autosize>

        <Group justify="flex-end">
          <Button variant="default" onClick={onClose}>Cancel</Button>
          <Button loading={save.isPending} onClick={() => save.mutate(undefined)}>Save underlyings</Button>
        </Group>
      </Stack>
    </Modal>
  );
}

export function UniversesPage() {
  const [target, setTarget] = useState<UniverseSummary | null>(null);
  const [editing, setEditing] = useState<UniverseSummary | null>(null);
  const { data, isLoading, error } = useQuery<UniverseSummary[]>({
    queryKey: ["/admin/universes"],
    queryFn: () => api.get<UniverseSummary[]>("/admin/universes"),
    // Poll every 3s while anything is mid-seed so the status column self-updates.
    refetchInterval: (q) =>
      (q.state.data ?? []).some((u) => u.status === "SEEDING") ? 3000 : false,
  });

  const anySeeding = (data ?? []).some((u) => u.status === "SEEDING");

  // Seeded/seeding/partial universes first, then everything else; alphabetical within each.
  const loaded = (s: string) =>
    s === "SEEDED" || s === "SEEDING" || s === "PARTIAL" ? 0 : 1;
  const sorted = [...(data ?? [])].sort(
    (a, b) => loaded(a.status) - loaded(b.status) || a.code.localeCompare(b.code),
  );

  return (
    <Stack>
      <Title order={3}>Instrument universes</Title>
      <Text size="sm" c="dimmed">
        The catalog of provider datasets available for seeding — their seed status
        and when each was last loaded. Seed one with the button on its row.
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
              <Table.Th>Status</Table.Th>
              <Table.Th ta="right">Instruments</Table.Th>
              <Table.Th>Last loaded</Table.Th>
              <Table.Th />
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {sorted.map((u) => (
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
                  <Group gap={6}>
                    <Badge color={STATUS_COLOR[u.status] ?? "gray"}>{u.status}</Badge>
                    {(u.status === "ERROR" || u.status === "PARTIAL") && u.last_error && (
                      <Text
                        size="xs"
                        c={u.status === "ERROR" ? "red" : "orange"}
                        lineClamp={1}
                        maw={200}
                        title={u.last_error}
                      >
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
                  <Group gap={6} justify="flex-end">
                    {u.category === "OPTION" && (
                      <Button size="xs" variant="default" onClick={() => setEditing(u)}>
                        Underlyings
                      </Button>
                    )}
                    <Button
                      size="xs"
                      variant="light"
                      loading={u.status === "SEEDING"}
                      onClick={() => setTarget(u)}
                    >
                      Seed
                    </Button>
                  </Group>
                </Table.Td>
              </Table.Tr>
            ))}
            {sorted.length === 0 && (
              <Table.Tr>
                <Table.Td colSpan={7}>
                  <Text c="dimmed" ta="center" py="md">No universes in the catalog.</Text>
                </Table.Td>
              </Table.Tr>
            )}
          </Table.Tbody>
        </Table>
      )}

      <SeedModal universe={target} onClose={() => setTarget(null)} />
      <EditUnderlyingsModal universe={editing} onClose={() => setEditing(null)} />
    </Stack>
  );
}
