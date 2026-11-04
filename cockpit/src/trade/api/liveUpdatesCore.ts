// The live-updates protocol, kept free of React and of the API client so
// `node --test` can load it on its own.
//
// The OMS sends notices, not data (src/live_updates.rs): a notice says which
// cached queries are now stale, and they refetch through the same REST calls
// as before — so what a trader may see is still decided by those endpoints.

export type UiNotice =
  | { type: "orders" }
  | { type: "positions" }
  | { type: "resync" }
  | { type: "marks"; instrument_ids: number[] };

// Query keys as the trade screens build them today:
//   ["/orders", …]                    blotter list and the followed order
//   ["/portfolios", id, "positions"]  positions
//   ["/marks", ids[]]                 watchlist and positions marks (ids as strings)
const isOrders = (key: readonly unknown[]) => key[0] === "/orders";
const isPositions = (key: readonly unknown[]) => key[0] === "/portfolios" && key[2] === "positions";

export function staleFor(n: UiNotice): (queryKey: readonly unknown[]) => boolean {
  switch (n.type) {
    case "orders":
      return isOrders;
    case "positions":
      return isPositions;
    case "resync":
      return () => true;
    case "marks": {
      const moved = new Set(n.instrument_ids.map(String));
      // Positions too: the server prices unrealized P&L from these marks.
      return (key) =>
        isPositions(key) ||
        (key[0] === "/marks" && Array.isArray(key[1]) && key[1].some((id) => moved.has(String(id))));
    }
  }
}

export function reconnectDelayMs(attempt: number): number {
  return Math.min(1000 * 2 ** attempt, 30_000);
}

export function updatesUrl(loc: { protocol: string; host: string }, apiBase: string): string {
  const scheme = loc.protocol === "https:" ? "wss:" : "ws:";
  return `${scheme}//${loc.host}${apiBase}/updates`;
}
