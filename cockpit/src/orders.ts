// Pure order-list logic shared by the blotter and the Orders page.

/** POST /orders/cancel refuses orders in these states: they are finished. */
export const TERMINAL: ReadonlySet<string> = new Set(["filled", "canceled", "rejected", "expired"]);

export type StatusFilter = "all" | "open" | "filled" | "closed";

type Filterable = { status: string; order_id: string; instrument_symbol?: string | null; client_order_id: string };

/** Orders matching a status bucket and a free-text query (order id, client order id, symbol). */
export function filterOrders<T extends Filterable>(rows: T[], filter: StatusFilter, query: string): T[] {
  const q = query.trim().toLowerCase();
  return rows.filter((r) => {
    const done = TERMINAL.has(r.status);
    const bucket =
      filter === "all" || (filter === "open" && !done) || (filter === "filled" && r.status === "filled") ||
      (filter === "closed" && done && r.status !== "filled");
    if (!bucket) return false;
    if (!q) return true;
    return [r.order_id, r.client_order_id, r.instrument_symbol ?? ""].some((v) => v.toLowerCase().includes(q));
  });
}
