import { useState } from "react";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { Button, Group, Paper, Stack, Text, Title } from "@mantine/core";
import { tradeApi } from "../api/client";
import { InstrumentSelect, type Instrument } from "../../components/InstrumentSelect";
import type { MarkRow } from "../types";

interface WatchlistRow {
  instrument_id: string;
  created_at: string;
}

// Live watchlist: a trader's own list of symbols, ticking price + day
// %-change. Polls /marks on the same ~3s cadence TradeBlotter already uses
// for orders — no new transport, just another poll.
//
// Uses plain "×"/"+" text in a subtle Button rather than ActionIcon +
// @tabler/icons-react: that package is not a dependency of this app, and
// this task does not add one just for two glyphs.
export function Watchlist({ onSelectInstrument }: { onSelectInstrument: (instrumentId: string) => void }) {
  const queryClient = useQueryClient();
  const [adding, setAdding] = useState(false);
  const [pendingInstrument, setPendingInstrument] = useState<string | null>(null);

  const watchlist = useQuery<WatchlistRow[]>({
    queryKey: ["/watchlist"],
    queryFn: () => tradeApi.get<WatchlistRow[]>("/watchlist"),
  });

  const instrumentIds = (watchlist.data ?? []).map((w) => w.instrument_id);
  const marks = useQuery<MarkRow[]>({
    queryKey: ["/marks", instrumentIds],
    queryFn: () => tradeApi.get<MarkRow[]>(`/marks?instrument_ids=${instrumentIds.join(",")}`),
    enabled: instrumentIds.length > 0,
    refetchInterval: 3000,
  });
  const markByInstrument = new Map((marks.data ?? []).map((m) => [String(m.instrument_id), m]));

  const addMutation = useMutation({
    mutationFn: (instrumentId: string) => tradeApi.post("/watchlist", { instrument_id: instrumentId }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["/watchlist"] });
      setAdding(false);
      setPendingInstrument(null);
    },
  });

  const removeMutation = useMutation({
    mutationFn: (instrumentId: string) => tradeApi.del(`/watchlist/${instrumentId}`),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["/watchlist"] }),
  });

  return (
    <Paper withBorder p="md">
      <Group justify="space-between" mb="xs">
        <Title order={4}>Watchlist</Title>
        <Button
          variant="subtle"
          size="compact-sm"
          onClick={() => setAdding((v) => !v)}
          aria-label="Add symbol"
        >
          +
        </Button>
      </Group>

      {adding && (
        <InstrumentSelect
          value={pendingInstrument}
          onChange={setPendingInstrument}
          onSelected={(instrument: Instrument | null) => {
            if (instrument) addMutation.mutate(String(instrument.id));
          }}
          placeholder="Search symbol to add"
          basePath="/instruments"
          apiGet={tradeApi.get}
        />
      )}

      <Stack gap={4} mt="xs">
        {(watchlist.data ?? []).map((row) => {
          const mark = markByInstrument.get(row.instrument_id);
          const pct = mark?.pct_change ?? null;
          return (
            <Group
              key={row.instrument_id}
              justify="space-between"
              wrap="nowrap"
              style={{ cursor: "pointer" }}
              onClick={() => onSelectInstrument(row.instrument_id)}
            >
              <Text size="sm">{row.instrument_id}</Text>
              <Group gap="xs" wrap="nowrap">
                <Text size="sm" ff="monospace">
                  {mark?.mid !== null && mark?.mid !== undefined ? mark.mid.toFixed(2) : "—"}
                </Text>
                <Text size="xs" c={pct === null ? "dimmed" : pct >= 0 ? "depth" : "offer"}>
                  {pct === null ? "" : `${pct >= 0 ? "+" : ""}${pct.toFixed(2)}%`}
                </Text>
                <Button
                  size="compact-xs"
                  variant="subtle"
                  aria-label="Remove from watchlist"
                  onClick={(e) => {
                    e.stopPropagation();
                    removeMutation.mutate(row.instrument_id);
                  }}
                >
                  ×
                </Button>
              </Group>
            </Group>
          );
        })}
        {watchlist.data?.length === 0 && (
          <Text size="sm" c="dimmed">
            No symbols yet — add one above.
          </Text>
        )}
      </Stack>
    </Paper>
  );
}
