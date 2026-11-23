import { useState } from "react";
import { ChevronDown, Search } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import type { MarketDataSource } from "../../marketData";
import { SampleBadge } from "../market/SampleBadge";
import { useMarketTick } from "../market/useMarketTick";
import { Watchlist } from "./Watchlist";

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

/**
 * The trade screen's header strip: the instrument button (watchlist popover, plus the
 * full search dialog for everything else and combos) and the instrument's quote.
 */
export function InstrumentHeader({
  source, instrument, onPick, onOpenSearch,
}: {
  source: MarketDataSource;
  instrument: PickedInstrument | null;
  onPick: (i: PickedInstrument) => void;
  onOpenSearch: () => void;
}) {
  const [open, setOpen] = useState(false);
  const key = instrument?.symbol ?? "";
  useMarketTick(source, key);
  const t = instrument ? source.ticker(key) : null;

  return (
    <header className="flex flex-wrap items-center gap-x-7 gap-y-3 rounded-md border bg-card px-4 py-3">
      <Popover open={open} onOpenChange={setOpen}>
        <PopoverTrigger asChild>
          <Button variant="outline" className="h-auto min-h-11 gap-3 py-1.5 text-left" aria-label="Change instrument">
            <span className="flex flex-col">
              <span className="text-lg font-semibold">{instrument?.symbol ?? "Pick an instrument"}</span>
              {instrument && <span className="text-xs text-muted-foreground">{instrument.venue} · {instrument.name}</span>}
            </span>
            <ChevronDown className="text-muted-foreground" />
          </Button>
        </PopoverTrigger>
        <PopoverContent align="start" className="w-[420px] max-w-[90vw] p-2">
          <Button variant="secondary" className="mb-2 w-full justify-start" onClick={() => { setOpen(false); onOpenSearch(); }}>
            <Search /> Search all instruments, options and combos…
          </Button>
          <Watchlist onSelectInstrument={(i) => { setOpen(false); onPick(i); }} />
        </PopoverContent>
      </Popover>
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
