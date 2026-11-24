import { test } from "node:test";
import assert from "node:assert/strict";
import { resolveGrants, toggleGrant } from "./accessGrants.ts";

test("default grants show as on and inherited; group-only grants as on", () => {
  assert.deepEqual(resolveGrants(["orders", "limits_view"], ["cancel_any", "orders"]), [
    { key: "orders", on: true, inherited: true },
    { key: "limits_view", on: true, inherited: true },
    { key: "cancel_any", on: true, inherited: false },
  ]);
});

test("no grants at all resolves to nothing", () => {
  assert.deepEqual(resolveGrants([], []), []);
});

test("toggling adds or removes a key without duplicates", () => {
  assert.deepEqual(toggleGrant(["a"], "b", true), ["a", "b"]);
  assert.deepEqual(toggleGrant(["a", "b"], "b", true), ["a", "b"]);
  assert.deepEqual(toggleGrant(["a", "b"], "a", false), ["b"]);
});
