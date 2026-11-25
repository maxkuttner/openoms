import { test } from "node:test";
import assert from "node:assert/strict";
import {
  rollLegs, searchRootEntries, rollBadgeDue, bannerText, defaultSpreadPrice, clampQty, defaultQty,
  venueToday, visibleRollAlerts, rollOffer, daysUntil,
  type FutureRoot, type AlertRow,
} from "./futures.ts";
import { staleFor } from "./api/liveUpdatesCore.ts";

const es: FutureRoot = {
  code: "ES", name: "E-mini S&P 500", venue: "CME", roll_warn_days: 5,
  front: { instrument_id: 11, symbol: "ESZ6", expiry_date: "2026-12-18", roll_date: "2026-12-10", contract_size: 50 },
  next: { instrument_id: 12, symbol: "ESH7", expiry_date: "2027-03-19", roll_date: "2027-03-11", contract_size: 50 },
};
const nq: FutureRoot = {
  code: "NQ", name: "E-mini Nasdaq-100", venue: "CME", roll_warn_days: 5,
  front: { instrument_id: 21, symbol: "NQZ6", expiry_date: "2026-12-18", roll_date: "2026-12-10", contract_size: 20 },
  next: null,
};

test("rollLegs: long sells near and buys far", () => {
  assert.equal(rollLegs(4, "ESZ6", "ESH7"), "Sell 4 ESZ6 · Buy 4 ESH7");
});
test("rollLegs: short buys near and sells far", () => {
  assert.equal(rollLegs(-2, "ESZ6", "ESH7"), "Buy 2 ESZ6 · Sell 2 ESH7");
});
test("rollLegs uses an explicit qty over the position", () => {
  assert.equal(rollLegs(4, "ESZ6", "ESH7", 1), "Sell 1 ESZ6 · Buy 1 ESH7");
});

test("search 'es' lists the root entry first, then contracts", () => {
  const out = searchRootEntries("es", [es, nq]);
  assert.equal(out.length, 1);
  assert.equal(out[0].label, "ES → ESZ6 (Dec 2026)");
  assert.equal(out[0].instrument_id, 11);
  assert.equal(searchRootEntries("zzz", [es]).length, 0);
  assert.equal(searchRootEntries("", [es]).length, 0);
  assert.equal(searchRootEntries("nq", [es, nq])[0].symbol, "NQZ6");
  assert.equal(searchRootEntries("nasdaq", [es, nq])[0].symbol, "NQZ6");
});
test("a root without a front contract is not offered", () => {
  assert.equal(searchRootEntries("es", [{ ...es, front: null }]).length, 0);
});

test("venueToday is the Chicago date", () => {
  assert.equal(venueToday(new Date("2026-12-10T03:00:00Z")), "2026-12-09");
  assert.equal(venueToday(new Date("2026-12-10T15:00:00Z")), "2026-12-10");
});
test("daysUntil counts calendar days", () => {
  assert.equal(daysUntil("2026-12-07", "2026-12-10"), 3);
  assert.equal(daysUntil("2026-12-10", "2026-12-10"), 0);
  assert.equal(daysUntil("2026-12-12", "2026-12-10"), -2);
});
test("roll badge from roll_date - warn days", () => {
  assert.equal(rollBadgeDue("2026-12-04", "2026-12-10", 5), false);
  assert.equal(rollBadgeDue("2026-12-05", "2026-12-10", 5), true);
  assert.equal(rollBadgeDue("2026-12-11", "2026-12-10", 5), true);
});

test("banner text", () => {
  assert.equal(bannerText({ contract: "ESZ6", days: 3, qty: 4, portfolio: "ACME" }, true),
    "ESZ6 rolls in 3 days — 4 contracts long in ACME");
  assert.equal(bannerText({ contract: "ESZ6", days: 1, qty: 1, portfolio: "ACME" }, false),
    "ESZ6 rolls in 1 day — 1 contract short in ACME");
  assert.equal(bannerText({ contract: "ESZ6", days: 0, qty: 2, portfolio: "ACME" }, true),
    "ESZ6 rolls today — 2 contracts long in ACME");
  assert.equal(bannerText({ contract: "ESZ6", days: -2, qty: 2, portfolio: "ACME" }, true),
    "ESZ6 roll is 2 days overdue — 2 contracts long in ACME");
});

test("default spread price is net debit (buy mids minus sell mids)", () => {
  assert.equal(defaultSpreadPrice(4, 5000, 5010), 10); // long: far - near
  assert.equal(defaultSpreadPrice(-2, 5000, 5010), -10); // short: near - far
  assert.equal(defaultSpreadPrice(4, null, 5010), null);
  assert.equal(defaultSpreadPrice(4, 5000, null), null);
  assert.equal(defaultSpreadPrice(4, 5000, 5000), 0);
});

test("qty defaults to the full position and clamps to it", () => {
  assert.equal(defaultQty(-4), 4);
  assert.equal(clampQty(10, 4), 4);
  assert.equal(clampQty(2, -4), 2);
  assert.equal(clampQty(0, 4), null);
  assert.equal(clampQty(-1, 4), null);
  assert.equal(clampQty(Number.NaN, 4), null);
  assert.equal(clampQty(1.5, 4), 1.5);
});

const alert = (over: Partial<AlertRow> = {}): AlertRow => ({
  id: "a1", kind: "ROLL_DUE", portfolio_id: "p1", instrument_id: 11,
  payload: { root: "ES", contract: "ESZ6", next_contract: "ESH7", next_instrument_id: 12, net_qty: 4, roll_date: "2026-12-10", expires_at: "2026-12-18" },
  created_at: "2026-12-05T00:00:00Z", ...over,
});
test("banner hides alerts whose position is now flat or gone", () => {
  const open = new Map([["p1", [{ instrument_id: "11", net_qty: 4 }]]]);
  assert.equal(visibleRollAlerts([alert()], open).length, 1);
  const flat = new Map([["p1", [{ instrument_id: "11", net_qty: 0 }]]]);
  assert.equal(visibleRollAlerts([alert()], flat).length, 0);
  const gone = new Map([["p1", [] as { instrument_id: string; net_qty: number }[]]]);
  assert.equal(visibleRollAlerts([alert()], gone).length, 0);
  // not loaded yet: hidden; failed to load: shown
  assert.equal(visibleRollAlerts([alert()], new Map()).length, 0);
  assert.equal(visibleRollAlerts([alert()], new Map([["p1", null]])).length, 1);
  assert.equal(visibleRollAlerts([alert({ kind: "OTHER" })], open).length, 0);
});

test("rollOffer from roots, falling back to the alert for an overdue contract", () => {
  const pos = { portfolio_id: "p1", instrument_id: "11", net_qty: 4 };
  const o = rollOffer(pos, [es], [], "2026-12-06");
  assert.deepEqual([o?.nearSymbol, o?.farSymbol, o?.farInstrumentId, o?.due], ["ESZ6", "ESH7", 12, true]);
  assert.equal(rollOffer(pos, [es], [], "2026-11-01")?.due, false);
  assert.equal(rollOffer({ ...pos, instrument_id: "99" }, [es], [], "2026-12-06"), null);
  assert.equal(rollOffer({ ...pos, instrument_id: "21" }, [nq], [], "2026-12-06"), null);
  const late = { ...es, front: es.next, next: null };
  const f = rollOffer(pos, [late], [alert()], "2026-12-12");
  assert.deepEqual([f?.nearSymbol, f?.farInstrumentId, f?.due], ["ESZ6", 12, true]);
  assert.equal(rollOffer({ ...pos, net_qty: 0 }, [es], [], "2026-12-06"), null);
});

test("an alerts notice stales the alerts query only", () => {
  const stale = staleFor({ type: "alerts" });
  assert.equal(stale(["/alerts"]), true);
  assert.equal(stale(["/orders"]), false);
});
