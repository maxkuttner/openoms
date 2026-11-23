import { useEffect, useRef, useState } from "react";
import {
  AreaSeries, CandlestickSeries, ColorType, CrosshairMode, HistogramSeries, LineSeries, createChart,
  type IChartApi, type UTCTimestamp,
} from "lightweight-charts";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { movingAverage, type MarketDataSource, type Timeframe } from "../../marketData";
import { SampleBadge } from "./SampleBadge";
import { cssVar, useMarketTick } from "./useMarketTick";

const TIMEFRAMES: Timeframe[] = ["1m", "5m", "1h", "4h", "1d"];
const CANDLES = 300;
const MA_PERIOD = 20;
type View = "price" | "depth";

/** Candles + volume + MA, or the depth curve, drawn with TradingView Lightweight Charts. */
export function PriceChart({ source, instrumentId }: { source: MarketDataSource; instrumentId: string }) {
  const box = useRef<HTMLDivElement>(null);
  const chart = useRef<IChartApi | null>(null);
  const [tf, setTf] = useState<Timeframe>("1h");
  const [view, setView] = useState<View>("price");
  const tick = useMarketTick(source, instrumentId);
  const live = useRef<((t: number) => void) | null>(null);

  useEffect(() => {
    const el = box.current;
    if (!el) return;
    const buy = cssVar("--buy");
    const sell = cssVar("--sell");
    const c = createChart(el, {
      autoSize: true,
      layout: { background: { type: ColorType.Solid, color: "transparent" }, textColor: cssVar("--muted-foreground"), attributionLogo: true },
      grid: { vertLines: { color: cssVar("--border-subtle") }, horzLines: { color: cssVar("--border-subtle") } },
      rightPriceScale: { borderColor: cssVar("--border") },
      timeScale: { borderColor: cssVar("--border"), timeVisible: view === "price" && tf !== "1d" },
      crosshair: { mode: CrosshairMode.Normal },
    });
    chart.current = c;
    live.current = null;

    if (view === "price") {
      const candles = source.candles(instrumentId, tf, CANDLES);
      const time = (t: number) => t as UTCTimestamp;
      const price = c.addSeries(CandlestickSeries, {
        upColor: buy, downColor: sell, borderUpColor: buy, borderDownColor: sell, wickUpColor: buy, wickDownColor: sell,
      });
      price.setData(candles.map((k) => ({ time: time(k.time), open: k.open, high: k.high, low: k.low, close: k.close })));
      const volume = c.addSeries(HistogramSeries, { priceScaleId: "volume", priceFormat: { type: "volume" } });
      c.priceScale("volume").applyOptions({ scaleMargins: { top: 0.8, bottom: 0 } });
      volume.setData(candles.map((k) => ({ time: time(k.time), value: k.volume, color: (k.close >= k.open ? buy : sell) + "66" })));
      const ma = movingAverage(candles.map((k) => k.close), MA_PERIOD);
      c.addSeries(LineSeries, { color: cssVar("--warn"), lineWidth: 1, priceLineVisible: false, lastValueVisible: false })
        .setData(candles.flatMap((k, i) => (ma[i] === null ? [] : [{ time: time(k.time), value: ma[i]! }])));
      const last = candles[candles.length - 1];
      live.current = () => {
        const close = source.ticker(instrumentId).last;
        price.update({ time: time(last.time), open: last.open, high: Math.max(last.high, close), low: Math.min(last.low, close), close });
      };
    } else {
      // Depth: cumulative size against price; bids left of mid, asks right. The x axis is
      // the price ladder, so time formatting is replaced by plain numbers.
      const draw = () => {
        const b = source.book(instrumentId, 40);
        let t = 0;
        const bids = b.bids.map((l) => ({ p: l.price, v: (t += l.size) })).reverse();
        t = 0;
        const asks = b.asks.map((l) => ({ p: l.price, v: (t += l.size) }));
        return { bids, asks };
      };
      c.applyOptions({ localization: { timeFormatter: (x: number) => (x / 100).toFixed(2) }, timeScale: { tickMarkFormatter: (x: number) => (x / 100).toFixed(0) } });
      const bidSeries = c.addSeries(AreaSeries, { lineColor: buy, topColor: buy + "55", bottomColor: buy + "08", lineWidth: 2 });
      const askSeries = c.addSeries(AreaSeries, { lineColor: sell, topColor: sell + "55", bottomColor: sell + "08", lineWidth: 2 });
      const put = () => {
        const { bids, asks } = draw();
        const pt = (x: { p: number; v: number }) => ({ time: Math.round(x.p * 100) as UTCTimestamp, value: x.v });
        bidSeries.setData(dedupe(bids.map(pt)));
        askSeries.setData(dedupe(asks.map(pt)));
      };
      put();
      c.timeScale().fitContent();
      live.current = put;
    }
    return () => { c.remove(); chart.current = null; live.current = null; };
  }, [source, instrumentId, tf, view]);

  useEffect(() => { live.current?.(tick); }, [tick]);

  return (
    <div className="flex h-full min-h-[420px] flex-col rounded-md border bg-card">
      <div className="flex flex-wrap items-center gap-2 border-b px-2.5 py-2">
        {view === "price" && (
          <Tabs value={tf} onValueChange={(v) => setTf(v as Timeframe)}>
            <TabsList>{TIMEFRAMES.map((t) => <TabsTrigger key={t} value={t}>{t}</TabsTrigger>)}</TabsList>
          </Tabs>
        )}
        <span className="text-xs text-warn">MA({MA_PERIOD})</span>
        <div className="ml-auto flex items-center gap-2">
          {source.sample && <SampleBadge />}
          <Tabs value={view} onValueChange={(v) => setView(v as View)}>
            <TabsList><TabsTrigger value="price">Price</TabsTrigger><TabsTrigger value="depth">Depth</TabsTrigger></TabsList>
          </Tabs>
        </div>
      </div>
      <div ref={box} className="min-h-0 flex-1" aria-label={view === "price" ? "Price chart" : "Depth chart"} role="img" />
    </div>
  );
}

/** Lightweight Charts needs strictly increasing x values; keep the last point per x. */
function dedupe<T extends { time: UTCTimestamp }>(pts: T[]): T[] {
  const out: T[] = [];
  for (const p of pts) {
    if (out.length && out[out.length - 1].time >= p.time) out[out.length - 1] = p;
    else out.push(p);
  }
  return out;
}
