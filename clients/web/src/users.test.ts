import { test } from "node:test";
import assert from "node:assert/strict";
import { accountRoleLabel, filterUsers, groupBadges, pendingUsers, portfolioCount } from "./users.ts";

test("account roles read as their labels and pending ones are picked out", () => {
  assert.equal(accountRoleLabel("pending"), "Pending");
  assert.equal(accountRoleLabel("admin"), "Admin");
  const rows = [{ id: 1, account_role: "user" }, { id: 2, account_role: "pending" }, { id: 3, account_role: "admin" }];
  assert.deepEqual(pendingUsers(rows).map((r) => r.id), [2]);
});

test("filterUsers matches name, email or code, ignoring case", () => {
  const rows = [
    { code: "ana", display_name: "Ana Ramos", email: "ana@x.com" },
    { code: "ben", display_name: null, email: "BEN@y.com" },
  ];
  assert.deepEqual(filterUsers(rows, "  ").length, 2);
  assert.deepEqual(filterUsers(rows, "ramos").map((u) => u.code), ["ana"]);
  assert.deepEqual(filterUsers(rows, "ben@").map((u) => u.code), ["ben"]);
  assert.deepEqual(filterUsers(rows, "zzz"), []);
});

test("groupBadges shows two and counts the rest", () => {
  assert.deepEqual(groupBadges([]), { shown: [], more: "" });
  assert.deepEqual(groupBadges(["a", "b"]), { shown: ["a", "b"], more: "" });
  assert.deepEqual(groupBadges(["a", "b", "c", "d"]), { shown: ["a", "b"], more: "+2" });
});

test("portfolioCount reads as a count", () => {
  assert.equal(portfolioCount(0), "—");
  assert.equal(portfolioCount(1), "1 portfolio");
  assert.equal(portfolioCount(3), "3 portfolios");
});
