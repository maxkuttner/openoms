import { test } from "node:test";
import assert from "node:assert/strict";
import { accessLabel, accessTableBody, groupSummary, highestAccess } from "./groups.ts";

test("the highest access across groups wins", () => {
  assert.equal(highestAccess(["view", "manage", "trade"]), "manage");
  assert.equal(highestAccess(["view", "trade"]), "trade");
  assert.equal(highestAccess([]), null, "no group, no access");
});

test("the access table becomes the API body", () => {
  assert.deepEqual(accessTableBody({ a: "trade", b: "manage" }), [
    { portfolio_id: "a", access: "trade" },
    { portfolio_id: "b", access: "manage" },
  ]);
  assert.equal(accessLabel("manage"), "Manage (PM)");
});

test("a group row reads as counts", () => {
  assert.equal(groupSummary({ members: [1], portfolios: [1, 2] }), "1 member · 2 portfolios");
  assert.equal(groupSummary({ members: [], portfolios: [] }), "0 members · 0 portfolios");
});
