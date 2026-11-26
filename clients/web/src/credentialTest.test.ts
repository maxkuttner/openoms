// Run with `npm test` (node --test; Node strips the types itself).
import { test } from "node:test";
import assert from "node:assert/strict";
import { describeSaveTest, describeTest } from "./credentialTest.ts";

test("a clean pass with no message is 'credential verified'", () => {
  assert.deepEqual(describeTest({ tested: true, ok: true, message: null }), { tone: "success", text: "credential verified" });
});

test("a pass that found something shows it", () => {
  const msg = "IB Gateway at ib-gateway:4004 is up, logged into account DU1234567 (paper). The OMS will connect within a minute.";
  assert.deepEqual(describeTest({ tested: true, ok: true, message: msg }), { tone: "success", text: msg });
});

test("a warning is amber, not a pass and not a failure", () => {
  const msg = "IB Gateway at ib-gateway:4004 is not reachable (connection refused).";
  assert.deepEqual(describeTest({ tested: true, ok: false, message: msg, warning: true }), { tone: "warning", text: msg });
});

test("a failure and not-testable keep their meaning", () => {
  assert.deepEqual(describeTest({ tested: true, ok: false, message: "401" }), { tone: "error", text: "401" });
  assert.deepEqual(describeTest({ tested: false, ok: false, message: "at logon" }), { tone: "info", text: "at logon" });
});

test("the save note carries the test's message", () => {
  assert.deepEqual(describeSaveTest({ tested: true, message: null }), { tone: "info", text: "Credential verified." });
  assert.deepEqual(describeSaveTest({ tested: true, message: "Connected." }), { tone: "info", text: "Connected." });
  assert.deepEqual(describeSaveTest({ tested: true, message: "Not up.", warning: true }), { tone: "warning", text: "Not up." });
  assert.deepEqual(describeSaveTest({ tested: false, message: "at logon" }), { tone: "info", text: "Not tested before save (at logon)." });
});
