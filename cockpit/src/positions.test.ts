// Run with `npm test` (node --test; Node strips the types itself).
import { test } from "node:test";
import assert from "node:assert/strict";
import { totalsByCurrency } from "./positions.ts";
import type { PositionRow } from "./positions.ts";

const row = (over: Partial<PositionRow>): PositionRow => ({
  portfolio_id: "p",
  instrument_id: "1",
  currency: "USD",
  net_qty: 1,
  avg_cost: 1,
  realized_pnl: 0,
  updated_at: "2026-01-01T00:00:00Z",
  mark: null,
  market_value: null,
  unrealized_pnl: null,
  mark_ts: null,
  ...over,
});

test("totals are kept per currency, never summed across them", () => {
  const t = totalsByCurrency([
    row({ instrument_id: "1", currency: "USD", realized_pnl: 10 }),
    row({ instrument_id: "2", currency: "EUR", realized_pnl: 5 }),
  ]);
  assert.deepEqual([...t.keys()], ["USD", "EUR"]);
  assert.equal(t.get("USD")!.realized, 10);
  assert.equal(t.get("EUR")!.realized, 5);
});

test("unrealized skips unpriced rows; day P&L uses the supplied marks", () => {
  const t = totalsByCurrency(
    [
      row({ instrument_id: "1", currency: "EUR", unrealized_pnl: 3, net_qty: 2 }),
      row({ instrument_id: "2", currency: "EUR", unrealized_pnl: null }),
    ],
    new Map([["1", { mid: 11, prev_close: 10 }]]),
  );
  assert.equal(t.get("EUR")!.unrealized, 3);
  assert.equal(t.get("EUR")!.day, 2);
});
