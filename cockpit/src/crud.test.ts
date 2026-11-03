// Run with `npm test` (node --test; Node strips the types itself).
import { test } from "node:test";
import assert from "node:assert/strict";
import { initialFormValues, requestBody } from "./crud.ts";

const fields = [
  { name: "code" },
  { name: "status", default: "ACTIVE" },
  { name: "enabled", type: "checkbox" as const },
];

test("a new row starts from each field's default", () => {
  assert.deepEqual(initialFormValues(fields, null), { code: "", status: "ACTIVE", enabled: false });
});

test("editing starts from the row, not the default", () => {
  assert.deepEqual(initialFormValues(fields, { code: "a", status: "CLOSED", enabled: true }), {
    code: "a",
    status: "CLOSED",
    enabled: true,
  });
});

test("an emptied nullable field is sent as null on edit, so the server clears it", () => {
  const f = [{ name: "status" }, { name: "max_order_value", nullable: true }];
  assert.deepEqual(requestBody({ status: "ACTIVE", max_order_value: "" }, f, false), { status: "ACTIVE", max_order_value: null });
  // On create an empty optional field is left out, so the server applies its default.
  assert.deepEqual(requestBody({ status: "ACTIVE", max_order_value: "" }, f, true), { status: "ACTIVE" });
  assert.deepEqual(requestBody({ status: "", max_order_value: 250 }, f, false), { max_order_value: 250 });
});
