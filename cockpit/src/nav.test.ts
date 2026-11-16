import { test } from "node:test";
import assert from "node:assert/strict";
import { ADMIN_TABS, SIDEBAR, ACCOUNT_PAGES, adminRouteAllowed, adminTabFor, commandItems } from "./nav.ts";

test("sidebar is Trade and Positions", () => assert.deepEqual(SIDEBAR.map((p) => p.title), ["Trade", "Positions"]));
test("account pages are settings and access", () =>
  assert.deepEqual(ACCOUNT_PAGES.map((p) => p.path), ["/settings", "/access"]));
test("admin tabs group the twelve admin pages", () => {
  assert.deepEqual(ADMIN_TABS.map((t) => t.title), ["People", "Trading setup", "Connectivity", "Monitoring"]);
  assert.equal(ADMIN_TABS.flatMap((t) => t.pages).length, 12);
  assert.deepEqual(ADMIN_TABS[0].pages.map((p) => p.title), ["Users", "Groups", "Sign-in", "API"]);
  assert.deepEqual(ADMIN_TABS[3].pages.map((p) => p.path), ["/admin/blotter"]);
});
test("tab lookup", () => {
  assert.equal(adminTabFor("/admin/data-feeds")?.id, "connectivity");
  assert.equal(adminTabFor("/admin/risk-limits/")?.id, "setup");
  assert.equal(adminTabFor("/positions"), null);
});
test("non-admin may not open admin routes", () => {
  assert.equal(adminRouteAllowed("/admin/users", false), false);
  assert.equal(adminRouteAllowed("/admin", false), false);
  assert.equal(adminRouteAllowed("/admin/users", true), true);
  assert.equal(adminRouteAllowed("/positions", false), true);
  assert.equal(adminRouteAllowed("/administrator", false), true);
});
test("command menu hides admin pages from non-admins", () => {
  assert.ok(commandItems(false).every((p) => !p.path.startsWith("/admin")));
  assert.equal(commandItems(true).filter((p) => p.path.startsWith("/admin")).length, 12);
  assert.ok(commandItems(false).some((p) => p.path === "/settings"));
});
