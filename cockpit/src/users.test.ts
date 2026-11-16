import { test } from "node:test";
import assert from "node:assert/strict";
import { accountRoleLabel, pendingUsers } from "./users.ts";

test("account roles read as their labels and pending ones are picked out", () => {
  assert.equal(accountRoleLabel("pending"), "Pending");
  assert.equal(accountRoleLabel("admin"), "Admin");
  const rows = [{ id: 1, account_role: "user" }, { id: 2, account_role: "pending" }, { id: 3, account_role: "admin" }];
  assert.deepEqual(pendingUsers(rows).map((r) => r.id), [2]);
});
