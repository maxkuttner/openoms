import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { bookRows, type BookRow, type MarketDataSource } from "../../marketData";
import { cn } from "@/lib/utils";
import { SampleBadge } from "./SampleBadge";
import { useMarketTick } from "./useMarketTick";
import { useThrottled } from "./useThrottled";

const fmt = (n: number, d: number) => n.toLocaleString(undefined, { minimumFractionDigits: d, maximumFractionDigits: d });

function Rows({ rows, side, priceDigits }: { rows: BookRow[]; side: "bid" | "ask"; priceDigits: number }) {
  return rows.map((r) => (
    <TableRow key={`${side}${r.price}`} className="relative border-0 hover:bg-transparent">
      <TableCell className={cn("num py-0.5", side === "bid" ? "text-buy" : "text-sell")}>
        <span
          aria-hidden
          className={cn("pointer-events-none absolute inset-y-0 right-0", side === "bid" ? "bg-buy/15" : "bg-sell/15")}
          style={{ width: `${r.widthPct}%` }}
        />
        <span className="relative">{fmt(r.price, priceDigits)}</span>
      </TableCell>
      <TableCell className="num relative py-0.5 text-right">{fmt(r.size, 4)}</TableCell>
      <TableCell className="num relative py-0.5 text-right text-muted-foreground">{fmt(r.total, 3)}</TableCell>
    </TableRow>
  ));
}

/** Price ladder with cumulative depth bars; redraws at most 10×/s. */
export function OrderBook({ source, instrumentId, depth = 7 }: { source: MarketDataSource; instrumentId: string; depth?: number }) {
  const tick = useThrottled(useMarketTick(source, instrumentId), 100);
  const book = source.book(instrumentId, depth);
  void tick;
  const sumOf = (xs: { size: number }[]) => xs.reduce((a, l) => a + l.size, 0);
  const max = Math.max(sumOf(book.bids), sumOf(book.asks));
  const asks = bookRows(book.asks, max).reverse();
  const bids = bookRows(book.bids, max);
  const mid = book.bids.length && book.asks.length ? (book.bids[0].price + book.asks[0].price) / 2 : null;
  const spread = book.bids.length && book.asks.length ? book.asks[0].price - book.bids[0].price : null;
  const priceDigits = mid !== null && mid < 10 ? 4 : 2;

  return (
    <section aria-label="Order book" className="rounded-md border bg-card p-3">
      <div className="mb-2 flex items-center justify-between gap-2">
        <h2 className="text-sm font-semibold">Order book</h2>
        {source.sample && <SampleBadge />}
      </div>
      <Table className="text-xs">
        <TableHeader>
          <TableRow className="hover:bg-transparent">
            <TableHead className="h-6">Price</TableHead>
            <TableHead className="h-6 text-right">Size</TableHead>
            <TableHead className="h-6 text-right">Total</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          <Rows rows={asks} side="ask" priceDigits={priceDigits} />
          <TableRow className="hover:bg-transparent">
            <TableCell colSpan={3} className="py-1.5">
              <span className="num text-base font-semibold text-buy">{mid === null ? "—" : fmt(mid, priceDigits)}</span>
              <span className="ml-2 text-muted-foreground">spread {spread === null ? "—" : fmt(spread, priceDigits)}</span>
            </TableCell>
          </TableRow>
          <Rows rows={bids} side="bid" priceDigits={priceDigits} />
        </TableBody>
      </Table>
    </section>
  );
}
