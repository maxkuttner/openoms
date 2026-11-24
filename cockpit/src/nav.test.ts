import { test } from "node:test";
import assert from "node:assert/strict";
import {
  ADMIN_GROUP, ADMIN_TABS, ACCOUNT_PAGES, MAIN_PAGES, TRADE_GROUP, adminRouteAllowed, adminTabFor, commandItems, legacyRedirect, pageFor, sidebarFor,
} from "./nav.ts";

test("trade group is Trade entry, Strategies, API", () =>
  assert.deepEqual(TRADE_GROUP.pages.map((p) => [p.title, p.path]), [["Trade entry", "/"], ["Strategies", "/strategies"], ["API", "/api"]]));
test("main pages", () => assert.deepEqual(MAIN_PAGES.map((p) => p.path), ["/portfolios", "/orders", "/jobs", "/events"]));
test("admin group deep-links each admin tab's first page", () =>
  assert.deepEqual(ADMIN_GROUP.pages.map((p) => [p.title, p.path]), [
    ["People", "/admin/users"], ["Trading setup", "/admin/portfolios"], ["Connectivity", "/admin/broker-connections"], ["Monitoring", "/admin/blotter"],
  ]));
test("non-admin sidebar has no Admin group", () => {
  assert.deepEqual(sidebarFor(false).groups.map((g) => g.id), ["trade"]);
  assert.deepEqual(sidebarFor(true).groups.map((g) => g.id), ["trade", "admin"]);
  assert.ok(commandItems(false).every((p) => !p.path.startsWith("/admin")));
});
test("legacy paths map to new pages", () => {
  assert.equal(legacyRedirect("/positions"), "/portfolios");
  assert.equal(legacyRedirect("/reported"), "/strategies");
  assert.equal(legacyRedirect("/orders"), null);
});
test("strategies page is reachable by everyone", () => {
  assert.equal(pageFor("/strategies")?.title, "Strategies");
  assert.ok(commandItems(false).some((p) => p.path === "/strategies"));
});
test("account pages are settings only; /access is the refused-sign-in landing, not a page to visit", () => {
  assert.deepEqual(ACCOUNT_PAGES.map((p) => p.path), ["/settings"]);
  assert.ok(commandItems(true).every((p) => p.path !== "/access"));
});
test("admin tabs group the twelve admin pages", () => {
  assert.deepEqual(ADMIN_TABS.map((t) => t.title), ["People", "Trading setup", "Connectivity", "Monitoring"]);
  assert.equal(ADMIN_TABS.flatMap((t) => t.pages).length, 12);
  assert.deepEqual(ADMIN_TABS[0].pages.map((p) => p.title), ["Users", "Groups", "Sign-in", "API"]);
  assert.deepEqual(ADMIN_TABS[3].pages.map((p) => p.path), ["/admin/blotter", "/admin/breaks"]);
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
test("pageFor names the page a path belongs to", () => {
  assert.equal(pageFor("/")?.title, "Trade entry");
  assert.equal(pageFor("/orders")?.title, "Orders");
  assert.equal(pageFor("/settings")?.title, "Settings");
  assert.equal(pageFor("/admin/broker-connections")?.title, "Broker connections");
  assert.equal(pageFor("/admin/users/")?.title, "Users");
  assert.equal(pageFor("/nowhere"), null);
});
