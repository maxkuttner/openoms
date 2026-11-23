// Pure logic for the Portfolios and Orders pages.

/** The portfolio a page opens on: the first one the user may view, or null. */
export function pickDefaultPortfolio(portfolios: { portfolio_id: string; can_view?: boolean }[]): string | null {
  return portfolios.find((p) => p.can_view !== false)?.portfolio_id ?? null;
}

type Exposed = { instrument_id: string; instrument_symbol: string | null; market_value: number | null };

/**
 * Each position's share of gross exposure (|market value|), largest first, the top `top`
 * kept and the rest folded into "Other". Unpriced or flat positions are left out.
 * Percentages are whole numbers that sum to 100 (largest remainder).
 */
export function exposureShares(rows: Exposed[], top: number): { label: string; pct: number }[] {
  const sized = rows
    .filter((r) => r.market_value !== null && r.market_value !== 0)
    .map((r) => ({ label: r.instrument_symbol ?? r.instrument_id, v: Math.abs(r.market_value!) }))
    .sort((a, b) => b.v - a.v);
  const gross = sized.reduce((s, r) => s + r.v, 0);
  if (gross === 0) return [];
  const kept = sized.slice(0, top);
  const rest = sized.slice(top).reduce((s, r) => s + r.v, 0);
  const parts = rest > 0 ? [...kept, { label: "Other", v: rest }] : kept;
  const raw = parts.map((p) => (p.v / gross) * 100);
  const pct = raw.map(Math.floor);
  let left = 100 - pct.reduce((s, x) => s + x, 0);
  [...raw.keys()].sort((a, b) => (raw[b] - pct[b]) - (raw[a] - pct[a])).forEach((i) => { if (left-- > 0) pct[i]++; });
  return parts.map((p, i) => ({ label: p.label, pct: pct[i] }));
}

/** Made-up daily cumulative P&L for the chart until a P&L history exists (shown with a Sample badge). */
export function samplePnl(seed: string, days: number): { date: string; pnl: number }[] {
  let h = 2166136261;
  for (let i = 0; i < seed.length; i++) h = Math.imul(h ^ seed.charCodeAt(i), 16777619);
  let x = h >>> 0;
  const rnd = () => ((x = (Math.imul(x, 1664525) + 1013904223) >>> 0) / 4294967296);
  const end = Date.UTC(2026, 10, 23);
  let pnl = 0;
  return Array.from({ length: days }, (_, i) => {
    pnl += (rnd() - 0.45) * 400;
    return { date: new Date(end - (days - 1 - i) * 86400000).toISOString().slice(0, 10), pnl: Math.round(pnl * 100) / 100 };
  });
}
