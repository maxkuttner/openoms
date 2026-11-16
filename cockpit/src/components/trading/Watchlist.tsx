import { useState } from "react";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";
import { Plus, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { api, ApiError } from "../../api/client";
import { useLive } from "../../api/liveUpdates";
import { InstrumentSelect, type Instrument } from "../InstrumentSelect";
import type { MarkRow } from "./types";

interface WatchlistRow {
  instrument_id: string;
  symbol: string;
  venue: string;
  name: string;
  created_at: string;
}

function notifyError(err: unknown) {
  const message = err instanceof ApiError ? `${err.status}: ${err.message}` : String(err);
  toast.error(message);
}

// Live watchlist: a trader's own list of symbols, ticking price + day
// %-change. Polls /marks on the same ~3s cadence TradeBlotter already uses
// for orders — no new transport, just another poll.
export function Watchlist({
  onSelectInstrument,
}: {
  // Hands up the full clicked row, not just the id — OrderTicket needs
  // {id, symbol, venue, name} to set BOTH its instrumentId and
  // selectedInstrument (the confirmation modal's label is built from the
  // latter). See OrderTicket.tsx's effect.
  onSelectInstrument: (instrument: { id: string; symbol: string; venue: string; name: string }) => void;
}) {
  const queryClient = useQueryClient();
  const live = useLive();
  const [adding, setAdding] = useState(false);
  const [pendingInstrument, setPendingInstrument] = useState<string | null>(null);

  const watchlist = useQuery<WatchlistRow[]>({
    queryKey: ["/watchlist"],
    queryFn: () => api.get<WatchlistRow[]>("/watchlist"),
  });

  const instrumentIds = (watchlist.data ?? []).map((w) => w.instrument_id);
  const marks = useQuery<MarkRow[]>({
    queryKey: ["/marks", instrumentIds],
    queryFn: () => api.get<MarkRow[]>(`/marks?instrument_ids=${instrumentIds.join(",")}`),
    enabled: instrumentIds.length > 0,
    refetchInterval: live ? false : 3000,
  });
  const markByInstrument = new Map((marks.data ?? []).map((m) => [String(m.instrument_id), m]));

  const addMutation = useMutation({
    mutationFn: (instrumentId: string) => api.post("/watchlist", { instrument_id: instrumentId }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["/watchlist"] });
      setAdding(false);
      setPendingInstrument(null);
    },
    onError: notifyError,
  });

  const removeMutation = useMutation({
    mutationFn: (instrumentId: string) => api.del(`/watchlist/${instrumentId}`),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["/watchlist"] }),
    onError: notifyError,
  });

  return (
    <section className="rounded-md border bg-card p-3">
      <div className="mb-2 flex items-center justify-between">
        <h2 className="text-sm font-semibold">Watchlist</h2>
        <Button variant="ghost" size="icon-xs" onClick={() => setAdding((v) => !v)} aria-label="Add symbol">
          <Plus />
        </Button>
      </div>

      {adding && (
        <InstrumentSelect
          value={pendingInstrument}
          onChange={setPendingInstrument}
          onSelected={(instrument: Instrument | null) => {
            if (instrument) addMutation.mutate(String(instrument.id));
          }}
          placeholder="Search symbol to add"
          basePath="/instruments"
          apiGet={api.get}
        />
      )}

      <ul className="mt-2 flex flex-col">
        {(watchlist.data ?? []).map((row) => {
          const mark = markByInstrument.get(row.instrument_id);
          const pct = mark?.pct_change ?? null;
          return (
            <li
              key={row.instrument_id}
              className="group flex cursor-pointer items-center justify-between gap-2 rounded px-1.5 py-1 hover:bg-accent"
              onClick={() =>
                onSelectInstrument({
                  id: row.instrument_id,
                  symbol: row.symbol,
                  venue: row.venue,
                  name: row.name,
                })
              }
            >
              <span>{row.symbol}</span>
              <span className="flex items-center gap-2">
                <span className="num">{mark?.mid !== null && mark?.mid !== undefined ? mark.mid.toFixed(2) : "—"}</span>
                <span className={cn("num w-14 text-right text-xs", pct === null ? "text-muted-foreground" : pct >= 0 ? "text-buy" : "text-sell")}>
                  {pct === null ? "" : `${pct >= 0 ? "+" : ""}${pct.toFixed(2)}%`}
                </span>
                <Button
                  size="icon-xs"
                  variant="ghost"
                  aria-label="Remove from watchlist"
                  onClick={(e) => {
                    e.stopPropagation();
                    removeMutation.mutate(row.instrument_id);
                  }}
                >
                  <X />
                </Button>
              </span>
            </li>
          );
        })}
        {watchlist.data?.length === 0 && <li className="text-muted-foreground">No symbols yet — add one above.</li>}
      </ul>
    </section>
  );
}
