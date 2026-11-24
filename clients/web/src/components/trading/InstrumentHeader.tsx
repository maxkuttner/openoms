import { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { cn } from "@/lib/utils";
import { notifyError } from "../../api/hooks";
import { Layers, Search, Star } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Command, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList, CommandSeparator } from "@/components/ui/command";
import { Kbd } from "@/components/ui/kbd";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { useDebounced } from "@/lib/useDebounced";
import { api } from "../../api/client";
import type { MarketDataSource } from "../../marketData";
import { SampleBadge } from "../market/SampleBadge";
import { useMarketTick } from "../market/useMarketTick";
import type { SearchInstrument } from "./types";

export type PickedInstrument = { id: string; symbol: string; venue: string; name: string };

const fmt = (n: number) => n.toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: n < 10 ? 4 : 2 });

function Stat({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex flex-col gap-0.5">
      <span className="text-xs text-muted-foreground">{label}</span>
      <span className="num text-sm">{children}</span>
    </div>
  );
}

type WatchRow = { instrument_id: string; symbol: string; venue: string; name: string };

/** True when a key press lands in a text field, where "/" must type a slash. */
const typing = (e: KeyboardEvent) => {
  const el = e.target as HTMLElement | null;
  return !!el && (el.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(el.tagName));
};

/**
 * Command-style instrument search: watchlist suggestions while empty, server matches
 * as you type, and the full dialog for options and combos. "/" opens it.
 */
function InstrumentPicker({
  instrument, onPick, onOpenSearch,
}: {
  instrument: PickedInstrument | null;
  onPick: (i: PickedInstrument) => void;
  onOpenSearch: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [q, setQ] = useState("");
  const term = useDebounced(q.trim(), 200);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "/" && !e.metaKey && !e.ctrlKey && !typing(e)) { e.preventDefault(); setOpen(true); }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);
  const watch = useQuery({ queryKey: ["/watchlist"], queryFn: () => api.get<WatchRow[]>("/watchlist"), enabled: open });
  const found = useQuery({
    queryKey: ["/instruments", "picker", term],
    queryFn: () => api.get<SearchInstrument[]>(`/instruments?search=${encodeURIComponent(term)}&limit=20`),
    enabled: open && term.length > 0,
  });
  const qc = useQueryClient();
  const watched = new Set((watch.data ?? []).map((w) => w.instrument_id));
  const toggleWatch = (id: string) => (e: React.MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    (watched.has(id) ? api.del(`/watchlist/${id}`) : api.post("/watchlist", { instrument_id: id }))
      .then(() => qc.invalidateQueries({ queryKey: ["/watchlist"] }))
      .catch(notifyError);
  };
  const star = (id: string) => (
    <button type="button" aria-label={watched.has(id) ? "Remove from watchlist" : "Add to watchlist"}
      onPointerDown={(e) => e.stopPropagation()} onClick={toggleWatch(id)} className="rounded p-0.5 hover:bg-muted">
      <Star className={cn("size-4", watched.has(id) ? "fill-warn text-warn" : "text-muted-foreground")} />
    </button>
  );
  const pick = (i: PickedInstrument) => { setOpen(false); setQ(""); onPick(i); };
  const watchRows = (watch.data ?? []).filter((w) => !q || w.symbol.toLowerCase().includes(q.trim().toLowerCase()));
  return (
    <Popover open={open} onOpenChange={(o) => { setOpen(o); if (!o) setQ(""); }}>
      <PopoverTrigger asChild>
        <Button variant="outline" className="h-auto min-h-11 w-72 max-w-full justify-start gap-3 py-1.5 text-left" aria-label="Search instrument">
          <Search className="text-muted-foreground" />
          <span className="flex min-w-0 flex-1 flex-col">
            <span className="truncate text-lg font-semibold">{instrument?.symbol ?? "Search instrument…"}</span>
            {instrument && <span className="truncate text-xs text-muted-foreground">{instrument.venue} · {instrument.name}</span>}
          </span>
          <Kbd>/</Kbd>
        </Button>
      </PopoverTrigger>
      <PopoverContent align="start" className="w-[420px] max-w-[90vw] p-0">
        <Command shouldFilter={false}>
          <CommandInput placeholder="Symbol or name…" value={q} onValueChange={setQ} />
          <CommandList>
            <CommandEmpty>{found.isFetching ? "Searching…" : "No instruments match."}</CommandEmpty>
            {watchRows.length > 0 && (
              <CommandGroup heading="Watchlist">
                {watchRows.map((w) => (
                  <CommandItem key={`w${w.instrument_id}`} value={`w${w.instrument_id}`}
                    onSelect={() => pick({ id: w.instrument_id, symbol: w.symbol, venue: w.venue, name: w.name })}>
                    {star(w.instrument_id)}
                    <span className="font-medium">{w.symbol}</span>
                    <span className="truncate text-muted-foreground">{w.name}</span>
                    <span className="ml-auto text-xs text-muted-foreground">{w.venue}</span>
                  </CommandItem>
                ))}
              </CommandGroup>
            )}
            {term && (found.data ?? []).length > 0 && (
              <CommandGroup heading="Instruments">
                {(found.data ?? []).map((r) => (
                  <CommandItem key={`i${r.id}`} value={`i${r.id}`}
                    onSelect={() => pick({ id: String(r.id), symbol: r.symbol, venue: r.venue, name: r.name })}>
                    {star(String(r.id))}
                    <span className="font-medium">{r.symbol}</span>
                    <span className="truncate text-muted-foreground">{r.name}</span>
                    <span className="ml-auto text-xs text-muted-foreground">{r.venue} · {r.instrument_class}</span>
                  </CommandItem>
                ))}
              </CommandGroup>
            )}
            <CommandSeparator />
            <CommandGroup>
              <CommandItem value="advanced" onSelect={() => { setOpen(false); onOpenSearch(); }}>
                <Layers /> Options chains and combos…
              </CommandItem>
            </CommandGroup>
          </CommandList>
        </Command>
      </PopoverContent>
    </Popover>
  );
}

/** The trade screen's header strip: the instrument search and the instrument's quote. */
export function InstrumentHeader({
  source, instrument, onPick, onOpenSearch,
}: {
  source: MarketDataSource;
  instrument: PickedInstrument | null;
  onPick: (i: PickedInstrument) => void;
  onOpenSearch: () => void;
}) {
  const key = instrument?.symbol ?? "";
  useMarketTick(source, key);
  const t = instrument ? source.ticker(key) : null;

  return (
    <header className="flex flex-wrap items-center gap-x-7 gap-y-3 rounded-md border bg-card px-4 py-3">
      <InstrumentPicker instrument={instrument} onPick={onPick} onOpenSearch={onOpenSearch} />
      {t && (
        <>
          <div className="flex flex-col gap-0.5">
            <span className="num text-xl font-medium">{fmt(t.last)}</span>
            <span className={`num text-xs ${t.changePct >= 0 ? "text-buy" : "text-sell"}`}>
              {t.changePct >= 0 ? "+" : ""}{t.changePct.toFixed(2)}% 24h
            </span>
          </div>
          <Stat label="Bid / Ask">{fmt(t.bid)} / {fmt(t.ask)}</Stat>
          {source.sample && <SampleBadge />}
        </>
      )}
    </header>
  );
}
