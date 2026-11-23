import { useEffect, useState } from "react";
import type { MarketDataSource } from "../../marketData";

/** A counter that bumps whenever the source says this instrument's data changed. */
export function useMarketTick(source: MarketDataSource, instrumentId: string): number {
  const [tick, setTick] = useState(0);
  useEffect(() => source.subscribe(instrumentId, () => setTick((t) => t + 1)), [source, instrumentId]);
  return tick;
}

/** Reads a theme colour (e.g. `--buy`) so canvas-drawn charts match the CSS theme. */
export function cssVar(name: string): string {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim() || "#888";
}
