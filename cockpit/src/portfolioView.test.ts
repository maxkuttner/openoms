import { test } from "node:test";
import assert from "node:assert/strict";
import { exposureShares, samplePnl } from "./portfolioView.ts";

const p = (instrument_symbol: string | null, market_value: number | null, instrument_id = instrument_symbol ?? "x") =>
  ({ instrument_symbol, instrument_id, market_value });

test("exposureShares keeps the top N by absolute value and folds the rest into Other", () => {
  const r = exposureShares([p("A", 50), p("B", -30), p("C", 10), p("D", 6), p("E", 4)], 3);
  assert.deepEqual(r.map((x) => x.label), ["A", "B", "C", "Other"]);
  assert.equal(r.reduce((s, x) => s + x.pct, 0), 100);
  assert.equal(r[1].pct, 30);
});
test("exposureShares skips unpriced positions and handles none", () => {
  assert.deepEqual(exposureShares([p("A", null), p("B", 0)], 3), []);
  assert.deepEqual(exposureShares([], 3), []);
  assert.deepEqual(exposureShares([p(null, 5, "42")], 3), [{ label: "42", pct: 100 }]);
});
test("samplePnl is deterministic per seed", () => {
  const a = samplePnl("p1", 30);
  assert.equal(a.length, 30);
  assert.deepEqual(a, samplePnl("p1", 30));
  assert.notDeepEqual(a, samplePnl("p2", 30));
  assert.ok(a.every((d) => Number.isFinite(d.pnl) && /^\d{4}-\d{2}-\d{2}$/.test(d.date)));
});
