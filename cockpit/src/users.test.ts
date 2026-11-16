import { test } from "node:test";
import assert from "node:assert/strict";
import { accessBody, accountRoleLabel, pendingUsers, roleLabel } from "./users.ts";

test("picked portfolios become API access entries", () => {
  assert.deepEqual(accessBody({ a: "trade", b: "view" }), [
    { portfolio_id: "a", can_trade: true },
    { portfolio_id: "b", can_trade: false },
  ]);
  assert.deepEqual(accessBody({}), []);
});

test("roles read as their labels", () => {
  assert.equal(roleLabel("pm"), "PM");
  assert.equal(roleLabel(null), "—");
});

test("account roles read as their labels and pending ones are picked out", () => {
  assert.equal(accountRoleLabel("pending"), "Pending");
  assert.equal(accountRoleLabel("admin"), "Admin");
  const rows = [{ id: 1, account_role: "user" }, { id: 2, account_role: "pending" }, { id: 3, account_role: "admin" }];
  assert.deepEqual(pendingUsers(rows).map((r) => r.id), [2]);
});
