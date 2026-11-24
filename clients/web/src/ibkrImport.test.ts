import { test } from "node:test";
import assert from "node:assert/strict";
import { importErrorMessage, selectedConIds } from "./ibkrImport.ts";
import type { Contract, ResolveRow } from "./ibkrImport.ts";

const c = (con_id: number): Contract => ({
  con_id, symbol: "X", primary_exchange: "SMART", mic: "XNAS", currency: "USD",
  min_tick: 0.01, lot_size: null, name: "X Inc", isin: null,
});
const rows: ResolveRow[] = [
  { line: 2, status: "found", candidates: [c(1)], message: "" },
  { line: 3, status: "ambiguous", candidates: [c(2), c(3)], message: "" },
  { line: 4, status: "ambiguous", candidates: [c(4), c(5)], message: "" },
  { line: 5, status: "not_found", candidates: [], message: "no match" },
];

test("found rows are included, ambiguous only when picked, not_found never", () => {
  assert.deepEqual(selectedConIds(rows, { 3: 3 }), [1, 3]);
  assert.deepEqual(selectedConIds(rows, {}), [1]);
});

test("a pick that is not a candidate of its row is ignored", () => {
  assert.deepEqual(selectedConIds(rows, { 3: 99 }), [1]);
});

test("503 shows the start hint, other errors stay verbatim", () => {
  assert.equal(importErrorMessage(503, "x"), "Start the IBKR connection first");
  assert.equal(importErrorMessage(422, "too many rows"), "too many rows");
});
