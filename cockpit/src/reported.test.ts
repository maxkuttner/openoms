import { test } from "node:test";
import assert from "node:assert/strict";
import { gapSummary, isReportedBreak, reportedBreakLabel, staleLabel, tokenActions } from "./reported.ts";

const now = Date.parse("2026-11-23T12:00:00Z");

test("staleLabel: never reported", () => {
  assert.equal(staleLabel({ last_report_at: null, stale: true }, now), "never reported");
});
test("staleLabel: stale shows age in minutes", () => {
  assert.equal(staleLabel({ last_report_at: "2026-11-23T11:48:00Z", stale: true }, now), "stale 12m");
});
test("staleLabel: stale for hours", () => {
  assert.equal(staleLabel({ last_report_at: "2026-11-23T09:30:00Z", stale: true }, now), "stale 2h30m");
});
test("staleLabel: ok", () => {
  assert.equal(staleLabel({ last_report_at: "2026-11-23T11:59:00Z", stale: false }, now), "ok");
});
test("gapSummary: none", () => assert.equal(gapSummary([]), "no gaps"));
test("gapSummary: lists expected vs got", () => {
  assert.equal(
    gapSummary([{ batch_id: "b7", sequence: 9, expected: 7 }]),
    "1 gap: expected 7, got 9 (b7)",
  );
  assert.equal(
    gapSummary([{ batch_id: "a", sequence: 5, expected: 3 }, { batch_id: "b", sequence: 9, expected: 6 }]),
    "2 gaps: expected 3, got 5 (a); expected 6, got 9 (b)",
  );
});
test("reported breaks are labelled unverified", () => {
  assert.equal(isReportedBreak({ source: "REPORTED" }), true);
  assert.equal(isReportedBreak({ source: "RECONCILED" }), false);
  assert.equal(reportedBreakLabel, "Reported, not verified");
});

test("token actions follow token_active", () => {
  assert.deepEqual(tokenActions({ token_active: true }), { rotateLabel: "Rotate token", canRevoke: true });
  assert.deepEqual(tokenActions({ token_active: false }), { rotateLabel: "Issue token", canRevoke: false });
  assert.deepEqual(tokenActions(undefined), { rotateLabel: "Rotate token", canRevoke: false });
});
