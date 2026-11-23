import { test } from "node:test";
import assert from "node:assert/strict";
import { bookRows, movingAverage, sampleMarketData as md } from "./marketData.ts";

test("same instrument gives identical candles", () => {
  const a = md.candles("BTCUSDT", "1h", 100);
  assert.deepEqual(a, md.candles("BTCUSDT", "1h", 100));
  assert.equal(a.length, 100);
  for (let i = 1; i < a.length; i++) assert.equal(a[i].time - a[i - 1].time, 3600);
});

test("candles are well formed", () => {
  for (const c of md.candles("ETHUSDT", "5m", 300)) {
    for (const v of [c.open, c.high, c.low, c.close, c.volume]) assert.ok(Number.isFinite(v));
    assert.ok(c.low <= Math.min(c.open, c.close));
    assert.ok(c.high >= Math.max(c.open, c.close));
    assert.ok(c.low > 0 && c.volume >= 0);
  }
});

test("unknown instrument gets a stable series", () => {
  const a = md.candles("", "1d", 50);
  assert.equal(a.length, 50);
  assert.ok(a.every((c) => Number.isFinite(c.close) && c.close > 0));
  assert.deepEqual(a, md.candles("", "1d", 50));
});

test("book is crossed-free", () => {
  const b = md.book("BTCUSDT", 10);
  assert.equal(b.bids.length, 10);
  assert.equal(b.asks.length, 10);
  assert.ok(b.bids[0].price < b.asks[0].price);
  for (let i = 1; i < 10; i++) {
    assert.ok(b.bids[i].price < b.bids[i - 1].price);
    assert.ok(b.asks[i].price > b.asks[i - 1].price);
  }
  const t = md.ticker("BTCUSDT");
  assert.ok(t.bid < t.ask && Number.isFinite(t.last) && Number.isFinite(t.changePct));
});

test("bookRows accumulates", () => {
  const r = bookRows([{ price: 1, size: 2 }, { price: 0.9, size: 3 }], 5);
  assert.deepEqual(r.map((x) => x.total), [2, 5]);
  assert.deepEqual(r.map((x) => x.widthPct), [40, 100]);
});

test("empty book yields no rows and no NaN", () => {
  assert.deepEqual(bookRows([], 0), []);
  assert.ok(bookRows([{ price: 1, size: 0 }], 0).every((x) => x.widthPct === 0));
});

test("moving average", () => assert.deepEqual(movingAverage([1, 2, 3, 4], 2), [null, 1.5, 2.5, 3.5]));
