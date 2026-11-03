// Run with `npm test` (node --test; Node strips the types itself).
import { test } from "node:test";
import assert from "node:assert/strict";
import { INACTIVE, isActive, visibleRows, statusOptions } from "./lifecycle.ts";

const rows = [
  { code: "a", status: "ACTIVE" },
  { code: "b", status: "DISABLED" },
  { code: "c", status: "SUSPENDED" },
];

test("each resource deactivates to a status its table accepts", () => {
  // The CHECK constraints in db/migrations/ods/oms/000{1..4}_*.sql.
  assert.equal(INACTIVE.principal, "DISABLED");
  assert.equal(INACTIVE.portfolio, "CLOSED");
  assert.equal(INACTIVE.account, "CLOSED");
  assert.equal(INACTIVE.brokerConnection, "DISABLED");
});

test("anything but ACTIVE counts as inactive", () => {
  assert.ok(isActive("ACTIVE"));
  assert.ok(!isActive("DISABLED"));
  assert.ok(!isActive("SUSPENDED"));
  assert.ok(!isActive("CLOSED"));
});

test("inactive rows are hidden until asked for", () => {
  assert.deepEqual(visibleRows(rows, false).map((r) => r.code), ["a"]);
  assert.deepEqual(visibleRows(rows, true).map((r) => r.code), ["a", "b", "c"]);
});

test("the status picker offers only what the table accepts", () => {
  assert.deepEqual(statusOptions("account").map((o) => o.value), ["ACTIVE", "SUSPENDED", "CLOSED"]);
  assert.deepEqual(statusOptions("portfolio").map((o) => o.value), ["ACTIVE", "CLOSED"]);
  assert.deepEqual(statusOptions("brokerConnection").map((o) => o.value), ["ACTIVE", "DISABLED"]);
  assert.deepEqual(statusOptions("principal").map((o) => o.value), ["ACTIVE", "DISABLED"]);
});
