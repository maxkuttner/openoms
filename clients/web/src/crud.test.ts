// Run with `npm test` (node --test; Node strips the types itself).
import { test } from "node:test";
import assert from "node:assert/strict";
import { initialFormValues } from "./crud.ts";

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
