import { test } from "node:test";
import assert from "node:assert/strict";
import { accessFromMe, isAdmin } from "./access.ts";

test("401 is signed out", () => assert.equal(accessFromMe(401, null), "signed_out"));
test("5xx or network is unavailable", () => {
  assert.equal(accessFromMe(502, null), "unavailable");
  assert.equal(accessFromMe(0, null), "unavailable");
});
test("pending role", () => assert.equal(accessFromMe(200, { auth_method: "local", role: "pending" }), "pending"));
test("403 approval_pending is pending", () =>
  assert.equal(accessFromMe(403, null, '{"error":"approval_pending"}'), "pending"));
test("403 password_change_required is change_password", () =>
  assert.equal(accessFromMe(403, null, '{"error":"password_change_required"}'), "change_password"));
test("must change password beats ready", () =>
  assert.equal(accessFromMe(200, { auth_method: "local", role: "user", must_change_password: true }), "change_password"));
test("plain user is ready", () => assert.equal(accessFromMe(200, { auth_method: "oidc", role: "user" }), "ready"));
test("admin is ready and admin", () => {
  const me = { auth_method: "local", role: "admin" };
  assert.equal(accessFromMe(200, me), "ready");
  assert.equal(isAdmin(me), true);
});
test("user and nobody are not admin", () => {
  assert.equal(isAdmin({ auth_method: "local", role: "user" }), false);
  assert.equal(isAdmin(null), false);
});
