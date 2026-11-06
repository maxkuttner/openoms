// Run with `npm test` (node --test; Node strips the types itself).
import { test } from "node:test";
import assert from "node:assert/strict";
import { connectionEntries, isColocated, killLabel, reportSummary, savedAccounts } from "./strategies.ts";

test("a halted strategy offers Enable, any other offers Kill", () => {
  assert.equal(killLabel("HALTED"), "Enable");
  assert.equal(killLabel("ACTIVE"), "Kill");
  assert.equal(killLabel("REDUCING"), "Kill");
});

test("a control report reads as one line", () => {
  assert.equal(
    reportSummary({ cancelled: 3, cancel_failures: [], flatten_orders: 2, skipped: ["ETHBTC: no reference price"] }),
    "cancelled 3, flatten orders 2, skipped: ETHBTC: no reference price",
  );
  assert.equal(
    reportSummary({ cancelled: 1, cancel_failures: ["o1: broker rejected cancel"], flatten_orders: 0, skipped: [] }),
    "cancelled 1, failed: o1: broker rejected cancel",
  );
});

test("connections are the picked accounts with their brokers, one per broker", () => {
  const accounts = [
    { id: "a1", broker_connection_code: "binance-paper" },
    { id: "a2", broker_connection_code: "bybit-paper" },
  ];
  const connections = [
    { code: "binance-paper", broker_code: "BINANCE" },
    { code: "bybit-paper", broker_code: "BYBIT" },
  ];
  assert.deepEqual(connectionEntries(["a1", "a2"], accounts, connections), [
    { broker_code: "BINANCE", account_id: "a1" },
    { broker_code: "BYBIT", account_id: "a2" },
  ]);
});

test("the connections editor starts from the saved connections", () => {
  assert.deepEqual(savedAccounts([{ broker_code: "BINANCE", account_id: "a1" }, { broker_code: "BYBIT", account_id: "a2" }]), ["a1", "a2"]);
  assert.deepEqual(savedAccounts(undefined), []);
});

test("a co-located strategy is told apart by its mode", () => {
  assert.equal(isColocated({ mode: "COLOCATED" }), true);
  assert.equal(isColocated({ mode: "VIA_OMS" }), false);
});
