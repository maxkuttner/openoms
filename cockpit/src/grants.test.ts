// Run with `npm test` (node --test).
import { test } from "node:test";
import assert from "node:assert/strict";
import { needsRef, venueGrants } from "./grants.ts";

const brokers = { "alpaca-paper": "ALPACA", "ibkr-paper": "IBKR" };

test("needsRef is true only for IBKR brokers", () => {
  assert.equal(needsRef("IBKR"), true);
  assert.equal(needsRef("IBKR_TWS"), true);
  assert.equal(needsRef("ALPACA"), false);
});
test("venueGrants builds one grant per picked connection, sorted", () => {
  const g = venueGrants(new Set(["ibkr-paper", "alpaca-paper"]), { "ibkr-paper": " DU123 " }, brokers);
  assert.deepEqual(g, [
    { kind: "venue", params: { connection: "alpaca-paper", account_ref: null } },
    { kind: "venue", params: { connection: "ibkr-paper", account_ref: "DU123" } },
  ]);
});
test("venueGrants ignores a ref for brokers that do not need one", () => {
  assert.equal(venueGrants(new Set(["alpaca-paper"]), { "alpaca-paper": "X" }, brokers)[0].params.account_ref, null);
});
test("venueGrants sends an empty IBKR ref as empty string", () => {
  assert.equal(venueGrants(new Set(["ibkr-paper"]), {}, brokers)[0].params.account_ref, "");
});
test("venueGrants is empty when nothing is picked", () => {
  assert.deepEqual(venueGrants(new Set(), {}, brokers), []);
});
