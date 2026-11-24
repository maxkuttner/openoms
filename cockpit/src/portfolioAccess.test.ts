import { test } from "node:test";
import assert from "node:assert/strict";
import { accessSummary, addEntry, removeEntry, setLevel, venueSummary, type AccessEntry } from "./portfolioAccess.ts";

const g = (id: string, name: string, access: AccessEntry["access"]): AccessEntry => ({ kind: "group", id, name, access });
const p = (id: string, name: string, access: AccessEntry["access"]): AccessEntry => ({ kind: "person", id, name, access });

test("access summary names each entry with its level, or a dash when empty", () => {
  assert.equal(accessSummary([g("1", "Traders", "trade"), p("2", "Ana R.", "manage")]), "Traders: Trade · Ana R.: Manage");
  assert.equal(accessSummary([]), "—");
});

test("adding starts at view and never lists the same group or person twice", () => {
  const one = addEntry([], { kind: "group", id: "1", name: "Traders" });
  assert.deepEqual(one, [g("1", "Traders", "view")]);
  assert.deepEqual(addEntry(one, { kind: "group", id: "1", name: "Traders" }), one);
  // Same id as a person is a different entry.
  assert.equal(addEntry(one, { kind: "person", id: "1", name: "Ana" }).length, 2);
});

test("setting a level and removing touch only the matching entry", () => {
  const list = [g("1", "A", "view"), p("1", "B", "view")];
  assert.deepEqual(setLevel(list, "person", "1", "manage"), [g("1", "A", "view"), p("1", "B", "manage")]);
  assert.deepEqual(removeEntry(list, "group", "1"), [p("1", "B", "view")]);
});

test("venue summary lists connection codes, or a dash", () => {
  assert.equal(venueSummary(["binance-paper", "ibkr-live"]), "binance-paper, ibkr-live");
  assert.equal(venueSummary([]), "—");
});
