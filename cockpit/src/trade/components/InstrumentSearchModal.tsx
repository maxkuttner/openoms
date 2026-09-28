import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { useDebouncedValue } from "@mantine/hooks";
import {
  Alert, Badge, Button, Group, Loader, Modal, Paper, Stack, Table, Text, TextInput, Title,
  UnstyledButton,
} from "@mantine/core";
import { tradeApi, ApiError } from "../api/client";
import type { SearchInstrument, ChainRow, StrategyLeg } from "../types";

type Step = "search" | "underlying" | "chain";

type Template = {
  key: string;
  name: string;
  legCount: string;
  description: string;
};

// Covered Call and Iron Condor are explicitly out of scope for this pass —
// see docs/superpowers/specs/2026-09-28-options-search-chain-strategy-design.md.
const TEMPLATES: Template[] = [
  { key: "call", name: "Long Call", legCount: "1 leg", description: "Buy a call. Bullish, defined risk." },
  { key: "put", name: "Long Put", legCount: "1 leg", description: "Buy a put. Bearish, defined risk." },
  {
    key: "straddle",
    name: "Straddle",
    legCount: "2 legs",
    description: "Buy call + put, same strike. Bet on a big move either way.",
  },
  {
    key: "strangle",
    name: "Strangle",
    legCount: "2 legs",
    description: "Buy call + put, different strikes. Cheaper than a straddle.",
  },
  {
    key: "vertical",
    name: "Vertical Spread",
    legCount: "2 legs",
    description: "Buy a call, sell a further call. Capped risk & reward.",
  },
  { key: "custom", name: "Custom Combo", legCount: "any", description: "Pick any legs off the chain yourself." },
];

function notifyableMessage(err: unknown): string {
  return err instanceof ApiError ? `${err.status}: ${err.message}` : String(err);
}

export function InstrumentSearchModal({
  opened,
  onClose,
  onPickSingle,
  onPickLegs,
}: {
  opened: boolean;
  onClose: () => void;
  onPickSingle: (instrument: { id: string; symbol: string; venue: string; name: string; side?: "buy" | "sell" }) => void;
  onPickLegs: (legs: StrategyLeg[]) => void;
}) {
  const [step, setStep] = useState<Step>("search");
  const [search, setSearch] = useState("");
  const [debounced] = useDebouncedValue(search, 250);
  const [underlying, setUnderlying] = useState<SearchInstrument | null>(null);
  const [expiry, setExpiry] = useState<string | null>(null);
  const [legs, setLegs] = useState<StrategyLeg[]>([]);
  // Set when a template pick's own fetches (outside useQuery, so they need
  // their own error handling) fail, or resolve to nothing buildable — shown
  // inline rather than left as a silent no-op click.
  const [templateError, setTemplateError] = useState<string | null>(null);

  function resetAndClose() {
    setStep("search");
    setSearch("");
    setUnderlying(null);
    setExpiry(null);
    setLegs([]);
    setTemplateError(null);
    onClose();
  }

  const results = useQuery<SearchInstrument[]>({
    queryKey: ["/instruments", "search-modal", debounced],
    queryFn: () => tradeApi.get<SearchInstrument[]>(`/instruments?search=${encodeURIComponent(debounced)}&limit=50`),
    enabled: step === "search" && debounced.length > 0,
  });

  const expiries = useQuery<string[]>({
    queryKey: ["/instruments/options/expiries", underlying?.symbol],
    queryFn: () =>
      tradeApi.get<string[]>(`/instruments/options/expiries?underlying=${encodeURIComponent(underlying!.symbol)}`),
    enabled: step === "underlying" && !!underlying,
  });

  const chain = useQuery<ChainRow[]>({
    queryKey: ["/instruments/options/chain", underlying?.symbol, expiry],
    queryFn: () =>
      tradeApi.get<ChainRow[]>(
        `/instruments/options/chain?underlying=${encodeURIComponent(underlying!.symbol)}&expiry=${expiry}`,
      ),
    enabled: step === "chain" && !!underlying && !!expiry,
  });

  const marks = useQuery<Record<number, { bid: number | null; ask: number | null }>>({
    queryKey: ["/marks", "chain", chain.data?.map((r) => [r.call?.instrument_id, r.put?.instrument_id])],
    queryFn: async () => {
      const ids = (chain.data ?? [])
        .flatMap((r) => [r.call?.instrument_id, r.put?.instrument_id])
        .filter((id): id is number => id != null);
      if (ids.length === 0) return {};
      const rows = await tradeApi.get<{ instrument_id: number; bid: number | null; ask: number | null }[]>(
        `/marks?instrument_ids=${ids.join(",")}`,
      );
      const byId: Record<number, { bid: number | null; ask: number | null }> = {};
      for (const r of rows) byId[r.instrument_id] = { bid: r.bid, ask: r.ask };
      return byId;
    },
    enabled: step === "chain" && !!chain.data && chain.data.length > 0,
  });

  function pickUnderlying(instrument: SearchInstrument) {
    if (!instrument.has_options) {
      onPickSingle({ id: String(instrument.id), symbol: instrument.symbol, venue: instrument.venue, name: instrument.name });
      resetAndClose();
      return;
    }
    setUnderlying(instrument);
    setStep("underlying");
  }

  // `withMark` is false when called from pickTemplate, before /marks has
  // been queried for this expiry — referencePrice is left null rather than
  // stale/wrong. useCombo (below) backfills any still-null price from
  // marks.data at hand-off time, once the chain step has had a chance to load it.
  function legFromChainRow(
    row: ChainRow,
    side: "call" | "put",
    tradeSide: "buy" | "sell",
    expiryDate: string,
    withMark: boolean,
  ): StrategyLeg | null {
    const ref = side === "call" ? row.call : row.put;
    if (!ref) return null;
    const mark = withMark ? marks.data?.[ref.instrument_id] : undefined;
    const price = withMark ? (tradeSide === "buy" ? mark?.ask ?? null : mark?.bid ?? null) : null;
    return {
      instrumentId: String(ref.instrument_id),
      symbol: ref.symbol,
      optionKind: side === "call" ? "CALL" : "PUT",
      strike: row.strike,
      expiry: expiryDate,
      side: tradeSide,
      referencePrice: price,
    };
  }

  function toggleLeg(row: ChainRow, side: "call" | "put", tradeSide: "buy" | "sell") {
    const ref = side === "call" ? row.call : row.put;
    if (!ref) return;
    const id = String(ref.instrument_id);
    const exists = legs.some((l) => l.instrumentId === id && l.side === tradeSide);
    if (exists) {
      setLegs(legs.filter((l) => !(l.instrumentId === id && l.side === tradeSide)));
      return;
    }
    const leg = legFromChainRow(row, side, tradeSide, expiry!, true);
    if (leg) setLegs([...legs, leg]);
  }

  async function pickTemplate(templateKey: string) {
    if (!underlying) return;
    setTemplateError(null);
    try {
      const dates =
        expiries.data ??
        (await tradeApi.get<string[]>(`/instruments/options/expiries?underlying=${encodeURIComponent(underlying.symbol)}`));
      if (dates.length === 0) {
        setTemplateError(`${underlying.symbol} has no active option expiries.`);
        return;
      }
      const nearest = dates[0];

      if (templateKey === "custom") {
        setExpiry(nearest);
        setLegs([]);
        setStep("chain");
        return;
      }

      const rows = await tradeApi.get<ChainRow[]>(
        `/instruments/options/chain?underlying=${encodeURIComponent(underlying.symbol)}&expiry=${nearest}`,
      );
      if (rows.length === 0) {
        setTemplateError(`No strikes found for ${underlying.symbol} on ${nearest}.`);
        return;
      }

      // ATM proxy: the middle strike of the returned ladder. This is a known
      // approximation — a real broker-sourced ladder is not always centered
      // on spot, so this can land meaningfully away from the money. A better
      // version would fetch the underlying's own mark and pick the nearest
      // strike to it; deferred rather than done here to keep this fix pass
      // scoped to the reviewed defects.
      const atmIndex = Math.floor(rows.length / 2);
      const atmRow = rows[atmIndex];

      let built: StrategyLeg[] = [];
      if (templateKey === "call") {
        const leg = legFromChainRow(atmRow, "call", "buy", nearest, false);
        if (leg) built = [leg];
      } else if (templateKey === "put") {
        const leg = legFromChainRow(atmRow, "put", "buy", nearest, false);
        if (leg) built = [leg];
      } else if (templateKey === "straddle") {
        const call = legFromChainRow(atmRow, "call", "buy", nearest, false);
        const put = legFromChainRow(atmRow, "put", "buy", nearest, false);
        built = [call, put].filter((l): l is StrategyLeg => l !== null);
      } else if (templateKey === "strangle") {
        const lowerIndex = Math.max(0, atmIndex - 1);
        const upperIndex = Math.min(rows.length - 1, atmIndex + 1);
        const put = legFromChainRow(rows[lowerIndex], "put", "buy", nearest, false);
        const call = legFromChainRow(rows[upperIndex], "call", "buy", nearest, false);
        built = [call, put].filter((l): l is StrategyLeg => l !== null);
      } else if (templateKey === "vertical") {
        const upperIndex = Math.min(rows.length - 1, atmIndex + 1);
        const buyCall = legFromChainRow(atmRow, "call", "buy", nearest, false);
        const sellCall = legFromChainRow(rows[upperIndex], "call", "sell", nearest, false);
        built = [buyCall, sellCall].filter((l): l is StrategyLeg => l !== null);
      }

      if (built.length === 0) {
        setTemplateError(`Could not build ${templateKey} for ${underlying.symbol} on ${nearest}.`);
        return;
      }

      setExpiry(nearest);
      setLegs(built);
      setStep("chain");
    } catch (err) {
      setTemplateError(notifyableMessage(err));
    }
  }

  // A marks.data entry always exists for every chain instrument once the
  // query resolves (the modal seeds { bid: null, ask: null } per id), so its
  // mere presence proves nothing — only the actual bid/ask value does.
  const pricedChainLegs = legs.filter((l) => {
    const mark = marks.data?.[Number(l.instrumentId)];
    const resolvedPrice = l.referencePrice ?? (l.side === "buy" ? mark?.ask : mark?.bid) ?? null;
    return resolvedPrice != null;
  });
  const allChainLegsPriced = pricedChainLegs.length === legs.length && legs.length > 0;
  const netPremium = legs.reduce((sum, l) => {
    const price = l.referencePrice ?? marks.data?.[Number(l.instrumentId)]?.[l.side === "buy" ? "ask" : "bid"] ?? 0;
    return sum + (l.side === "buy" ? price : -price);
  }, 0);

  // Backfills any leg still carrying a null referencePrice (built by
  // pickTemplate, before /marks had loaded for this expiry) from whatever
  // marks.data holds now — the chain step has had time to load it by the
  // point a trader actually reviews and confirms the combo.
  function useCombo() {
    if (legs.length === 0) return;
    const resolved = legs.map((l) => ({
      ...l,
      referencePrice: l.referencePrice ?? marks.data?.[Number(l.instrumentId)]?.[l.side === "buy" ? "ask" : "bid"] ?? null,
    }));
    if (resolved.length === 1) {
      const leg = resolved[0];
      onPickSingle({ id: leg.instrumentId, symbol: leg.symbol, venue: "OPRA", name: leg.symbol, side: leg.side });
    } else {
      onPickLegs(resolved);
    }
    resetAndClose();
  }

  return (
    <Modal opened={opened} onClose={resetAndClose} fullScreen title="Search instruments">
      <Stack gap="md">
        {step === "search" && (
          <>
            <TextInput
              placeholder="Search symbol or name…"
              value={search}
              onChange={(e) => setSearch(e.currentTarget.value)}
              autoFocus
            />
            {results.isLoading && <Loader size="sm" />}
            {results.error && <Alert color="red">{notifyableMessage(results.error)}</Alert>}
            {debounced.length === 0 && <Text c="dimmed">Type a symbol or name to search.</Text>}
            {debounced.length > 0 && !results.isLoading && (results.data ?? []).length === 0 && (
              <Text c="dimmed">No instruments match "{debounced}".</Text>
            )}
            {(["Underlyings", "Equity / other"] as const).map((group) => {
              const rows = (results.data ?? []).filter((r) =>
                group === "Underlyings" ? r.has_options : !r.has_options,
              );
              if (rows.length === 0) return null;
              return (
                <Stack key={group} gap={4}>
                  <Text size="xs" c="dimmed" fw={600} tt="uppercase">
                    {group}
                  </Text>
                  {rows.map((r) => (
                    <UnstyledButton key={r.id} onClick={() => pickUnderlying(r)}>
                      <Paper withBorder p="sm">
                        <Group justify="space-between">
                          <Group gap="xs">
                            <Text fw={600}>{r.symbol}</Text>
                            <Text c="dimmed" size="sm">
                              {r.name} · {r.venue}
                            </Text>
                          </Group>
                          {r.has_options && <Badge color="depth">Options</Badge>}
                        </Group>
                      </Paper>
                    </UnstyledButton>
                  ))}
                </Stack>
              );
            })}
          </>
        )}

        {step === "underlying" && underlying && (
          <>
            <Group justify="space-between">
              <Title order={4}>{underlying.symbol}</Title>
              <Group gap="xs">
                <Button
                  variant="light"
                  onClick={() =>
                    onPickSingle({
                      id: String(underlying.id),
                      symbol: underlying.symbol,
                      venue: underlying.venue,
                      name: underlying.name,
                    })
                  }
                >
                  Trade {underlying.symbol} directly
                </Button>
                <Button variant="subtle" onClick={() => { setTemplateError(null); setStep("search"); }}>
                  ← Back to search
                </Button>
              </Group>
            </Group>
            {expiries.isLoading && <Loader size="sm" />}
            {expiries.error && <Alert color="red">{notifyableMessage(expiries.error)}</Alert>}
            {templateError && <Alert color="red">{templateError}</Alert>}
            {expiries.data && expiries.data.length === 0 && (
              <Alert color="yellow">
                {underlying.symbol} has no active option expiries right now — use "Trade {underlying.symbol} directly"
                above, or check back later.
              </Alert>
            )}
            {expiries.data && expiries.data.length > 0 && (
              <Group grow>
                {TEMPLATES.map((t) => (
                  <UnstyledButton key={t.key} onClick={() => pickTemplate(t.key)}>
                    <Paper withBorder p="sm">
                      <Stack gap={4}>
                        <Group justify="space-between">
                          <Text fw={600}>{t.name}</Text>
                          <Badge size="sm" color="gray">
                            {t.legCount}
                          </Badge>
                        </Group>
                        <Text size="xs" c="dimmed">
                          {t.description}
                        </Text>
                      </Stack>
                    </Paper>
                  </UnstyledButton>
                ))}
              </Group>
            )}
          </>
        )}

        {step === "chain" && underlying && (
          <>
            <Group justify="space-between">
              <Title order={4}>
                {underlying.symbol} chain — {expiry}
              </Title>
              <Button variant="subtle" onClick={() => { setTemplateError(null); setStep("underlying"); }}>
                ← Back to templates
              </Button>
            </Group>
            <Group gap="xs">
              {(expiries.data ?? []).map((d) => (
                <Badge
                  key={d}
                  variant={d === expiry ? "filled" : "outline"}
                  style={{ cursor: "pointer" }}
                  onClick={() => setExpiry(d)}
                >
                  {d}
                </Badge>
              ))}
            </Group>
            {chain.isLoading && <Loader size="sm" />}
            {chain.error && <Alert color="red">{notifyableMessage(chain.error)}</Alert>}
            {chain.data && chain.data.length === 0 && !chain.isLoading && (
              <Alert color="yellow">No strikes listed for {underlying.symbol} on {expiry}.</Alert>
            )}
            <Text size="xs" c="dimmed">
              Click a price to add that leg — Ask buys, Bid sells. Click again to remove it.
            </Text>
            <Table striped highlightOnHover>
              <Table.Thead>
                <Table.Tr>
                  <Table.Th colSpan={2} ta="center" c="depth">
                    Calls
                  </Table.Th>
                  <Table.Th ta="center">Strike</Table.Th>
                  <Table.Th colSpan={2} ta="center" c="offer">
                    Puts
                  </Table.Th>
                </Table.Tr>
                <Table.Tr>
                  <Table.Th>Bid</Table.Th>
                  <Table.Th>Ask</Table.Th>
                  <Table.Th />
                  <Table.Th>Bid</Table.Th>
                  <Table.Th>Ask</Table.Th>
                </Table.Tr>
              </Table.Thead>
              <Table.Tbody>
                {(chain.data ?? []).map((row) => {
                  const callMark = row.call ? marks.data?.[row.call.instrument_id] : undefined;
                  const putMark = row.put ? marks.data?.[row.put.instrument_id] : undefined;
                  const has = (side: "call" | "put", tradeSide: "buy" | "sell") => {
                    const ref = side === "call" ? row.call : row.put;
                    return !!ref && legs.some((l) => l.instrumentId === String(ref.instrument_id) && l.side === tradeSide);
                  };
                  return (
                    <Table.Tr key={row.strike}>
                      <Table.Td
                        style={{ cursor: row.call ? "pointer" : "default" }}
                        onClick={() => row.call && toggleLeg(row, "call", "sell")}
                        bg={has("call", "sell") ? "var(--mantine-color-offer-light)" : undefined}
                        c={row.call ? "depth" : "dimmed"}
                      >
                        {row.call ? callMark?.bid ?? "—" : "—"}
                      </Table.Td>
                      <Table.Td
                        style={{ cursor: row.call ? "pointer" : "default" }}
                        onClick={() => row.call && toggleLeg(row, "call", "buy")}
                        bg={has("call", "buy") ? "var(--mantine-color-depth-light)" : undefined}
                        c={row.call ? "depth" : "dimmed"}
                        fw={600}
                      >
                        {row.call ? callMark?.ask ?? "—" : "—"}
                      </Table.Td>
                      <Table.Td ta="center" fw={700}>
                        {row.strike}
                      </Table.Td>
                      <Table.Td
                        style={{ cursor: row.put ? "pointer" : "default" }}
                        onClick={() => row.put && toggleLeg(row, "put", "sell")}
                        bg={has("put", "sell") ? "var(--mantine-color-offer-light)" : undefined}
                        c={row.put ? "offer" : "dimmed"}
                      >
                        {row.put ? putMark?.bid ?? "—" : "—"}
                      </Table.Td>
                      <Table.Td
                        style={{ cursor: row.put ? "pointer" : "default" }}
                        onClick={() => row.put && toggleLeg(row, "put", "buy")}
                        bg={has("put", "buy") ? "var(--mantine-color-depth-light)" : undefined}
                        c={row.put ? "offer" : "dimmed"}
                        fw={600}
                      >
                        {row.put ? putMark?.ask ?? "—" : "—"}
                      </Table.Td>
                    </Table.Tr>
                  );
                })}
              </Table.Tbody>
            </Table>
            <Group justify="space-between">
              <Text>
                {legs.length} leg{legs.length === 1 ? "" : "s"} selected
                {legs.length > 0 &&
                  (allChainLegsPriced
                    ? ` · net ${netPremium >= 0 ? "debit" : "credit"} ${Math.abs(netPremium).toFixed(2)}`
                    : " · net premium unavailable (not all legs priced)")}
              </Text>
              <Button disabled={legs.length === 0} onClick={useCombo}>
                Use this combo
              </Button>
            </Group>
          </>
        )}
      </Stack>
    </Modal>
  );
}
