import { test } from "node:test";
import assert from "node:assert/strict";
import { TERMINAL, filterOrders } from "./orders.ts";
import { pickDefaultPortfolio } from "./portfolioView.ts";

const row = (order_id: string, status: string, instrument_symbol: string | null = "BTCUSDT", client_order_id = `c-${order_id}`) =>
  ({ order_id, status, instrument_symbol, client_order_id });
const rows = [
  row("o1", "working"), row("o2", "partially_filled", "ETHUSDT"), row("o3", "filled"),
  row("o4", "canceled", null), row("o5", "rejected"), row("o6", "expired"), row("o7", "submitted"),
];
const ids = (xs: { order_id: string }[]) => xs.map((x) => x.order_id);

test("terminal states match the cancel endpoint's", () =>
  assert.deepEqual([...TERMINAL].sort(), ["canceled", "expired", "filled", "rejected"]));
test("open keeps non-terminal orders", () => assert.deepEqual(ids(filterOrders(rows, "open", "")), ["o1", "o2", "o7"]));
test("filled keeps only filled", () => assert.deepEqual(ids(filterOrders(rows, "filled", "")), ["o3"]));
test("closed keeps terminal but not filled", () => assert.deepEqual(ids(filterOrders(rows, "closed", "")), ["o4", "o5", "o6"]));
test("all with empty query keeps everything", () => assert.equal(filterOrders(rows, "all", "  ").length, 7));
test("query matches symbol, order id and client order id, case-insensitively", () => {
  assert.deepEqual(ids(filterOrders(rows, "all", "eth")), ["o2"]);
  assert.deepEqual(ids(filterOrders(rows, "all", "O5")), ["o5"]);
  assert.deepEqual(ids(filterOrders(rows, "all", "c-o6")), ["o6"]);
});
test("pickDefaultPortfolio returns null for none", () => {
  assert.equal(pickDefaultPortfolio([]), null);
  assert.equal(pickDefaultPortfolio([{ portfolio_id: "a", can_view: false }, { portfolio_id: "b", can_view: true }]), "b");
  assert.equal(pickDefaultPortfolio([{ portfolio_id: "a", can_view: false }]), null);
});
