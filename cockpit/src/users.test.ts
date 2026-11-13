import { test } from "node:test";
import assert from "node:assert/strict";
import { accessBody, roleLabel } from "./users.ts";

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
