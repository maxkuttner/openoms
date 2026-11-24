// Run with `npm test` (node --test; Node strips the types itself).
import { test } from "node:test";
import assert from "node:assert/strict";
import { adoptBreak, dismissBreak, errorText, lookbackOptions, openBreakCount, reconPatch, rulePriority, DEFAULT_RULE_PRIORITY, showAccounts, sweepSummary } from "./recon.ts";

test("blank_rule_priority_cannot_save", () => {
  assert.equal(rulePriority(""), null, "blank is not 0 (highest precedence)");
  assert.equal(rulePriority("  "), null);
  assert.equal(rulePriority(-1), null);
  assert.equal(rulePriority("1.5"), null);
  assert.equal(rulePriority("7"), 7);
  assert.equal(rulePriority(0), 0);
  assert.equal(DEFAULT_RULE_PRIORITY, 100);
});

test("lookback_select_offers_six_values", () => {
  assert.deepEqual(lookbackOptions().map((o) => o.value), ["1d", "1w", "1m", "1y", "5y", "10y"]);
  assert.equal(lookbackOptions().length, 6);
});

test("accounts_hidden_for_single_account_connector", () => {
  const venues = [
    { code: "BINANCE", multi_account: false },
    { code: "IBKR_TWS", multi_account: true },
  ];
  assert.equal(showAccounts({ broker_code: "BINANCE" }, venues), false);
  assert.equal(showAccounts({ broker_code: "IBKR_TWS" }, venues), true);
  assert.equal(showAccounts({ broker_code: "UNKNOWN" }, venues), false);
});

test("adopt_break_posts_portfolio", async () => {
  const calls: [string, unknown][] = [];
  const post = async (path: string, body: unknown) => {
    calls.push([path, body]);
    return {};
  };
  await adoptBreak(post, "b-1", "p-9");
  await dismissBreak(post, "b-1");
  assert.deepEqual(calls, [
    ["/admin/recon-breaks/b-1/adopt", { portfolio_id: "p-9" }],
    ["/admin/recon-breaks/b-1/dismiss", {}],
  ]);
});

test("the settings patch carries only what was set", () => {
  assert.deepEqual(reconPatch({ default_portfolio_id: null, external_policy: "break", eod_lookback: null, intraday_poll_secs: "" }), { external_policy: "break" });
  assert.deepEqual(reconPatch({ default_portfolio_id: "p", external_policy: null, eod_lookback: "1m", intraday_poll_secs: "30" }), {
    default_portfolio_id: "p",
    eod_lookback: "1m",
    intraday_poll_secs: 30,
  });
});

test("open breaks are counted", () => {
  assert.equal(openBreakCount([{ status: "open" }, { status: "open" }]), 2);
  assert.equal(openBreakCount(undefined), 0);
});

test("the sweep summary says what the venue served", () => {
  const s = sweepSummary({ requested_from: "2026-11-11T00:00:00Z", obtained_from: "2026-11-15T00:00:00Z", booked: 3, duplicates: 1, breaks: 0, failed: 0, errors: [] });
  assert.match(s, /3 booked/);
  assert.match(s, /history starts 2026-11-15/);
});

test("an API error shows its message", () => {
  assert.equal(errorText({ status: 422, message: "portfolio is inactive" }), "portfolio is inactive");
  assert.equal(errorText(new Error("boom")), "boom");
});
