// Market data behind one small interface, so the cockpit's chart and order
// book don't care where prices come from. Today the only implementation is a
// deterministic sample generator: there is no candle, book or quote feed yet
// (that architecture is a separate decision). Every surface fed from a source
// with `sample: true` must say so on screen.

export type Timeframe = "1m" | "5m" | "1h" | "4h" | "1d";
export type Candle = { time: number; open: number; high: number; low: number; close: number; volume: number };
export type BookLevel = { price: number; size: number };
export type Book = { bids: BookLevel[]; asks: BookLevel[] };
export type Ticker = { last: number; changePct: number; bid: number; ask: number };

export interface MarketDataSource {
  /** True when the data is made up; the UI shows a "Sample data" badge. */
  sample: boolean;
  candles(instrumentId: string, tf: Timeframe, count: number): Candle[];
  book(instrumentId: string, depth: number): Book;
  ticker(instrumentId: string): Ticker;
  /** Calls `cb` whenever this instrument's data may have changed; returns the unsubscribe. */
  subscribe(instrumentId: string, cb: () => void): () => void;
}

export const TIMEFRAME_SECONDS: Record<Timeframe, number> = { "1m": 60, "5m": 300, "1h": 3600, "4h": 14400, "1d": 86400 };

function hash(s: string): number {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) h = Math.imul(h ^ s.charCodeAt(i), 16777619);
  return h >>> 0;
}

function mulberry32(seed: number): () => number {
  let a = seed;
  return () => {
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/** A price level per instrument between 1 and ~100k, stable for its id. */
function basePrice(instrumentId: string): number {
  return Math.round(10 ** (hash(instrumentId) % 500 / 100) * 100) / 100;
}

/** Sample series end at a fixed time so screenshots and tests are stable. */
const SAMPLE_END = Date.UTC(2026, 10, 23, 12) / 1000;
const BOOK_STEP = 0.0005;
// A slow drift applied to the sample's "live" values; bumped by subscribe().
let liveTick = 0;

function sampleCandles(instrumentId: string, tf: Timeframe, count: number): Candle[] {
  const step = TIMEFRAME_SECONDS[tf];
  const rnd = mulberry32(hash(`${instrumentId}|${tf}`));
  const vol = { "1m": 0.002, "5m": 0.004, "1h": 0.01, "4h": 0.02, "1d": 0.035 }[tf];
  let price = basePrice(instrumentId);
  const end = Math.floor(SAMPLE_END / step) * step;
  const out: Candle[] = [];
  for (let i = 0; i < count; i++) {
    const open = price;
    const close = Math.max(open * (1 + (rnd() - 0.5) * 2 * vol), 0.01);
    const high = Math.max(open, close) * (1 + rnd() * vol * 0.6);
    const low = Math.max(Math.min(open, close) * (1 - rnd() * vol * 0.6), 0.005);
    out.push({ time: end - (count - 1 - i) * step, open, high, low, close, volume: Math.round(rnd() * 1000 * 100) / 100 });
    price = close;
  }
  return out;
}

function lastPrice(instrumentId: string): number {
  const [last] = sampleCandles(instrumentId, "1m", 1);
  return last.close * (1 + Math.sin(liveTick / 3) * 0.0008);
}

export const sampleMarketData: MarketDataSource = {
  sample: true,
  candles: sampleCandles,
  book(instrumentId, depth) {
    const mid = lastPrice(instrumentId);
    const rnd = mulberry32(hash(`${instrumentId}|book|${liveTick}`));
    const tick = mid * BOOK_STEP;
    const level = (i: number, sign: 1 | -1): BookLevel => ({
      price: mid + sign * tick * (i + 0.5),
      size: Math.round((0.05 + rnd() * 2) * 10000) / 10000,
    });
    return {
      bids: Array.from({ length: depth }, (_, i) => level(i, -1)),
      asks: Array.from({ length: depth }, (_, i) => level(i, 1)),
    };
  },
  ticker(instrumentId) {
    const day = sampleCandles(instrumentId, "1d", 2);
    const last = lastPrice(instrumentId);
    const spread = last * BOOK_STEP;
    return { last, changePct: ((last - day[0].close) / day[0].close) * 100, bid: last - spread / 2, ask: last + spread / 2 };
  },
  subscribe(_instrumentId, cb) {
    const t = setInterval(() => { liveTick++; cb(); }, 1000);
    return () => clearInterval(t);
  },
};

export type BookRow = { price: number; size: number; total: number; widthPct: number };

/** Running totals from the best price outward, and each row's bar width as a share of `maxTotal`. */
export function bookRows(levels: BookLevel[], maxTotal: number): BookRow[] {
  let total = 0;
  return levels.map((l) => {
    total += l.size;
    const widthPct = maxTotal > 0 ? Math.min(100, (total / maxTotal) * 100) : 0;
    return { price: l.price, size: l.size, total, widthPct };
  });
}

export function movingAverage(closes: number[], period: number): (number | null)[] {
  let sum = 0;
  return closes.map((c, i) => {
    sum += c;
    if (i >= period) sum -= closes[i - period];
    return i >= period - 1 ? sum / period : null;
  });
}
