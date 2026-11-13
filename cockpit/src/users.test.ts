import { test } from "node:test";
import assert from "node:assert/strict";
import { accessBody, accessMessage, roleLabel } from "./users.ts";

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

test("access page text by reason", () => {
  assert.equal(accessMessage("no_invite", null), "Access not granted. Ask the OMS admin for an invite.");
  assert.equal(accessMessage(null, null), "Access not granted. Ask the OMS admin for an invite.");
  assert.equal(accessMessage("expired", null), "This invite expired. Ask for a new one.");
  assert.equal(accessMessage("revoked", null), "This invite was revoked.");
  assert.equal(
    accessMessage("email_mismatch", "j…@fundco.com"),
    "This invite is for j…@fundco.com. Sign in with that address, or ask for a new invite.",
  );
  assert.equal(
    accessMessage("email_unverified", null),
    "Your sign-in provider didn't confirm your email address. Use another sign-in method or ask for a new invite.",
  );
});
