import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { cn } from "@/lib/utils";
import { useDebounced } from "@/lib/useDebounced";
import { Notice, Spinner } from "../common";
import { Pill } from "../status";
import { api, ApiError } from "../../api/client";
import { useFutureRoots } from "../../api/hooks";
import { searchRootEntries } from "../../futures";
import type { SearchInstrument, ChainRow, StrategyLeg } from "./types";

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
  const debounced = useDebounced(search, 250);
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
    queryFn: () => api.get<SearchInstrument[]>(`/instruments?search=${encodeURIComponent(debounced)}&limit=50`),
    enabled: step === "search" && debounced.length > 0,
  });

  // Futures roots first: "es" offers "ES → ESZ6 (Dec 2026)", which trades the front contract.
  const roots = useFutureRoots();
  const rootEntries = step === "search" ? searchRootEntries(debounced, roots.data ?? []) : [];

  const expiries = useQuery<string[]>({
    queryKey: ["/instruments/options/expiries", underlying?.symbol],
    queryFn: () =>
      api.get<string[]>(`/instruments/options/expiries?underlying=${encodeURIComponent(underlying!.symbol)}`),
    enabled: step === "underlying" && !!underlying,
  });

  const chain = useQuery<ChainRow[]>({
    queryKey: ["/instruments/options/chain", underlying?.symbol, expiry],
    queryFn: () =>
      api.get<ChainRow[]>(
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
      const rows = await api.get<{ instrument_id: number; bid: number | null; ask: number | null }[]>(
        `/marks?instrument_ids=${ids.join(",")}`,
      );
      const byId: Record<number, { bid: number | null; ask: number | null }> = {};
      for (const r of rows) byId[r.instrument_id] = { bid: r.bid, ask: r.ask };
      return byId;
    },
    enabled: step === "chain" && !!chain.data && chain.data.length > 0,
  });

  // A row click always means "trade this instrument": the server ranks the
  // exact symbol first, so typing SPY and clicking the top row puts SPY
  // itself on the ticket. The options flow is the row's own Options button
  // (openOptions), never the default.
  function pickInstrument(instrument: SearchInstrument) {
    onPickSingle({ id: String(instrument.id), symbol: instrument.symbol, venue: instrument.venue, name: instrument.name });
    resetAndClose();
  }

  function openOptions(instrument: SearchInstrument) {
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
        (await api.get<string[]>(`/instruments/options/expiries?underlying=${encodeURIComponent(underlying.symbol)}`));
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

      const rows = await api.get<ChainRow[]>(
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

  // One priced cell of the chain: Ask buys, Bid sells; highlighted when that leg is picked.
  const cell = (row: ChainRow, side: "call" | "put", tradeSide: "buy" | "sell", price: number | null | undefined, has: boolean) => {
    const ref = side === "call" ? row.call : row.put;
    return (
      <TableCell
        className={cn(
          "num text-right",
          ref ? "cursor-pointer" : "text-muted-foreground",
          ref && (side === "call" ? "text-buy" : "text-sell"),
          tradeSide === "buy" && "font-semibold",
          has && (tradeSide === "buy" ? "bg-buy/15" : "bg-sell/15"),
        )}
        onClick={() => ref && toggleLeg(row, side, tradeSide)}
      >
        {ref ? price ?? "—" : "—"}
      </TableCell>
    );
  };

  return (
    <Dialog open={opened} onOpenChange={(open) => { if (!open) resetAndClose(); }}>
      <DialogContent className="flex h-[90vh] max-w-[min(1100px,95vw)] flex-col gap-3 overflow-y-auto sm:max-w-[min(1100px,95vw)]">
        <DialogHeader><DialogTitle>Search instruments</DialogTitle></DialogHeader>
        {step === "search" && (
          <>
            <Input placeholder="Search symbol or name…" value={search} onChange={(e) => setSearch(e.currentTarget.value)} autoFocus />
            {results.isLoading && <Spinner />}
            {results.error && <Notice>{notifyableMessage(results.error)}</Notice>}
            {debounced.length === 0 && <p className="text-muted-foreground">Type a symbol or name to search.</p>}
            {debounced.length > 0 && !results.isLoading && (results.data ?? []).length === 0 && rootEntries.length === 0 && (
              <p className="text-muted-foreground">No instruments match "{debounced}".</p>
            )}
            {/* One list, in the server's ranked order (see search_instruments
                in src/instruments_api.rs) — never regrouped here, or the exact
                match can be pushed below a section header. */}
            <ul className="flex flex-col gap-1">
              {rootEntries.map((r) => (
                <li key={`root-${r.root}`} className="flex items-center justify-between rounded-md border hover:bg-accent">
                  <button type="button"
                    onClick={() => { onPickSingle({ id: String(r.instrument_id), symbol: r.symbol, venue: r.venue, name: r.name }); resetAndClose(); }}
                    className="flex min-w-0 flex-1 items-center gap-2 px-3 py-2 text-left">
                    <span className="font-semibold">{r.label}</span>
                    <span className="truncate text-muted-foreground">{r.name} · {r.venue}</span>
                  </button>
                </li>
              ))}
              {(results.data ?? []).map((r) => (
                <li key={r.id} className="flex items-center justify-between rounded-md border hover:bg-accent">
                  <button type="button" onClick={() => pickInstrument(r)} className="flex min-w-0 flex-1 items-center gap-2 px-3 py-2 text-left">
                    <span className="font-semibold">{r.symbol}</span>
                    <span className="truncate text-muted-foreground">{r.name} · {r.venue}</span>
                  </button>
                  {r.has_options && (
                    <Button variant="secondary" size="xs" className="mr-2 text-buy" onClick={() => openOptions(r)}>Options</Button>
                  )}
                </li>
              ))}
            </ul>
          </>
        )}

        {step === "underlying" && underlying && (
          <>
            <div className="flex items-center justify-between">
              <h3 className="font-semibold">{underlying.symbol}</h3>
              <div className="flex gap-2">
                <Button variant="secondary" onClick={() => pickInstrument(underlying)}>Trade {underlying.symbol} directly</Button>
                <Button variant="ghost" onClick={() => { setTemplateError(null); setStep("search"); }}>← Back to search</Button>
              </div>
            </div>
            {expiries.isLoading && <Spinner />}
            {expiries.error && <Notice>{notifyableMessage(expiries.error)}</Notice>}
            {templateError && <Notice>{templateError}</Notice>}
            {expiries.data && expiries.data.length === 0 && (
              <Notice tone="info">
                {underlying.symbol} has no active option expiries right now — use "Trade {underlying.symbol} directly"
                above, or check back later.
              </Notice>
            )}
            {expiries.data && expiries.data.length > 0 && (
              <div className="grid grid-cols-2 gap-2 md:grid-cols-4">
                {TEMPLATES.map((t) => (
                  <button key={t.key} type="button" onClick={() => pickTemplate(t.key)}
                    className="flex flex-col gap-1 rounded-md border bg-card p-3 text-left hover:bg-accent">
                    <span className="flex items-center justify-between">
                      <span className="font-semibold">{t.name}</span>
                      <Pill>{t.legCount}</Pill>
                    </span>
                    <span className="text-xs text-muted-foreground">{t.description}</span>
                  </button>
                ))}
              </div>
            )}
          </>
        )}

        {step === "chain" && underlying && (
          <>
            <div className="flex items-center justify-between">
              <h3 className="font-semibold">{underlying.symbol} chain — <span className="num">{expiry}</span></h3>
              <Button variant="ghost" onClick={() => { setTemplateError(null); setStep("underlying"); }}>← Back to templates</Button>
            </div>
            <div className="flex flex-wrap gap-1">
              {(expiries.data ?? []).map((d) => (
                <button key={d} type="button" onClick={() => setExpiry(d)}
                  className={cn("num rounded border px-2 py-0.5 text-xs", d === expiry ? "border-primary bg-primary text-primary-foreground" : "hover:bg-accent")}>
                  {d}
                </button>
              ))}
            </div>
            {chain.isLoading && <Spinner />}
            {chain.error && <Notice>{notifyableMessage(chain.error)}</Notice>}
            {chain.data && chain.data.length === 0 && !chain.isLoading && (
              <Notice tone="info">No strikes listed for {underlying.symbol} on {expiry}.</Notice>
            )}
            <p className="text-xs text-muted-foreground">Click a price to add that leg — Ask buys, Bid sells. Click again to remove it.</p>
            <div className="rounded-md border">
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead colSpan={2} className="text-center text-buy">Calls</TableHead>
                    <TableHead className="text-center">Strike</TableHead>
                    <TableHead colSpan={2} className="text-center text-sell">Puts</TableHead>
                  </TableRow>
                  <TableRow>
                    <TableHead className="text-right">Bid</TableHead>
                    <TableHead className="text-right">Ask</TableHead>
                    <TableHead />
                    <TableHead className="text-right">Bid</TableHead>
                    <TableHead className="text-right">Ask</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {(chain.data ?? []).map((row) => {
                    const callMark = row.call ? marks.data?.[row.call.instrument_id] : undefined;
                    const putMark = row.put ? marks.data?.[row.put.instrument_id] : undefined;
                    const has = (side: "call" | "put", tradeSide: "buy" | "sell") => {
                      const ref = side === "call" ? row.call : row.put;
                      return !!ref && legs.some((l) => l.instrumentId === String(ref.instrument_id) && l.side === tradeSide);
                    };
                    return (
                      <TableRow key={row.strike}>
                        {cell(row, "call", "sell", callMark?.bid, has("call", "sell"))}
                        {cell(row, "call", "buy", callMark?.ask, has("call", "buy"))}
                        <TableCell className="num text-center font-bold">{row.strike}</TableCell>
                        {cell(row, "put", "sell", putMark?.bid, has("put", "sell"))}
                        {cell(row, "put", "buy", putMark?.ask, has("put", "buy"))}
                      </TableRow>
                    );
                  })}
                </TableBody>
              </Table>
            </div>
            <div className="flex items-center justify-between">
              <p>
                {legs.length} leg{legs.length === 1 ? "" : "s"} selected
                {legs.length > 0 &&
                  (allChainLegsPriced
                    ? ` · net ${netPremium >= 0 ? "debit" : "credit"} ${Math.abs(netPremium).toFixed(2)}`
                    : " · net premium unavailable (not all legs priced)")}
              </p>
              <Button disabled={legs.length === 0} onClick={useCombo}>Use this combo</Button>
            </div>
          </>
        )}
      </DialogContent>
    </Dialog>
  );
}
