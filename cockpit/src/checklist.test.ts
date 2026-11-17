import { test } from "node:test";
import assert from "node:assert/strict";
import { checklistSteps, checklistVisible, type SetupStatus } from "./checklist.ts";

const none: SetupStatus = { venue_connected: false, portfolio_exists: false, sign_in_ok: false, user_added: false, first_order: false };

test("checklistSteps lists the five steps in order with links", () => {
  const steps = checklistSteps(none);
  assert.deepEqual(steps.map((s) => s.label), [
    "Connect a venue",
    "Create a portfolio",
    "Turn on sign-in",
    "Add a user",
    "First order shows in the blotter",
  ]);
  assert.deepEqual(steps.map((s) => s.to), ["/admin/broker-connections", "/admin/portfolios", "/admin/sign-in", "/admin/users", "/admin/blotter"]);
  assert.ok(steps.every((s) => !s.done));
});

test("each step flips with its own flag", () => {
  const steps = checklistSteps({ ...none, portfolio_exists: true, first_order: true });
  assert.deepEqual(steps.map((s) => s.done), [false, true, false, false, true]);
});

test("the checklist is hidden once everything is done", () => {
  assert.equal(checklistVisible(none), true);
  assert.equal(checklistVisible({ ...none, venue_connected: true }), true);
  const all = { venue_connected: true, portfolio_exists: true, sign_in_ok: true, user_added: true, first_order: true };
  assert.equal(checklistVisible(all), false);
});
