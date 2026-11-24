import { test } from "node:test";
import assert from "node:assert/strict";
import { historyLabel, type HistoryEvent } from "./portfolioHistory.ts";

const ev = (action: string, detail: unknown): HistoryEvent => ({ at: "2026-01-01T00:00:00Z", actor: "ada", action, detail });

test("created and edited", () => {
  assert.equal(historyLabel(ev("portfolio.created", {})), "Created");
  assert.equal(historyLabel(ev("portfolio.updated", { changes: { status: ["ACTIVE", "CLOSED"], name: ["a", "b"] } })), "Edited: status ACTIVE → CLOSED, name a → b");
  assert.equal(historyLabel(ev("portfolio.updated", { changes: { base_currency: [null, "USD"] } })), "Edited: base_currency — → USD");
  assert.equal(historyLabel(ev("portfolio.updated", {})), "Edited");
});

test("venues, access, limits, group", () => {
  assert.equal(historyLabel(ev("grants.set", { grants: [{ kind: "venue", params: { connection: "alpaca-paper" } }, { kind: "venue", params: { connection: "ibkr" } }] })), "Venues set: alpaca-paper, ibkr");
  assert.equal(historyLabel(ev("grants.set", { grants: [] })), "Venues set: none");
  assert.equal(historyLabel(ev("portfolio.access_set", { detail: [{}, {}] })), "Access changed (2 entries)");
  assert.equal(historyLabel(ev("portfolio.access_set", { detail: [{}] })), "Access changed (1 entry)");
  assert.equal(historyLabel(ev("limits.changed", {})), "Risk limits changed");
  assert.equal(historyLabel(ev("group.created", { detail: { name: "portfolio X" } })), "Group created: portfolio X");
});

test("unknown action shown raw", () => {
  assert.equal(historyLabel(ev("weird.thing", null)), "weird.thing");
});
