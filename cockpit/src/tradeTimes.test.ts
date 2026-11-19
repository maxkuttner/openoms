import { test } from "node:test";
import assert from "node:assert/strict";
import { sortBlotter, eventLabel, eventTimes, positionLabel, tradeTimeCell, placedCell } from "./tradeTimes.ts";

const r = (id: string, trade_time: string | null, booked_at: string) => ({ order_id: id, trade_time, booked_at });

test("blotter_sorts_by_trade_time_then_booked", () => {
  const rows = [
    r("none", null, "2026-01-05T00:00:00Z"),
    r("old", "2026-01-01T00:00:00Z", "2026-01-09T00:00:00Z"),
    r("new-early-book", "2026-01-03T00:00:00Z", "2026-01-03T01:00:00Z"),
    r("new-late-book", "2026-01-03T00:00:00Z", "2026-01-03T02:00:00Z"),
  ];
  assert.deepEqual(sortBlotter(rows).map((x) => x.order_id), ["new-late-book", "new-early-book", "old", "none"]);
  assert.equal(rows[0].order_id, "none"); // input untouched
});

test("timeline_labels_observed_not_submitted", () => {
  assert.equal(eventLabel("order_observed"), "Observed at venue");
  assert.equal(eventLabel("order_submitted"), "order_submitted");
  assert.equal(eventTimes("2026-01-01T00:00:00Z", "2026-01-01T00:00:05Z").length, 2);
  assert.equal(eventTimes("2026-01-01T00:00:00Z", "2026-01-01T00:00:00Z").length, 1);
});

test("positions_label_uses_instrument_name", () => {
  assert.deepEqual(positionLabel({ instrument_id: "7", instrument_symbol: "AAPL", instrument_name: "Apple Inc" }), { text: "AAPL", tooltip: "Apple Inc (7)" });
  assert.deepEqual(positionLabel({ instrument_id: "7", instrument_symbol: null, instrument_name: null }), { text: "7", tooltip: null });
});

test("estimated_and_first_seen_marked", () => {
  const fmt = (s: string) => `<${s}>`;
  assert.deepEqual(tradeTimeCell({ trade_time: "t", trade_time_estimated: true }, fmt), { text: "~<t>", tooltip: "estimated: venue sent no execution time" });
  assert.deepEqual(tradeTimeCell({ trade_time: "t", trade_time_estimated: false }, fmt), { text: "<t>", tooltip: null });
  assert.equal(tradeTimeCell({ trade_time: null, trade_time_estimated: false }, fmt).text, "—");
  assert.equal(placedCell({ placed_at: "p", placed_is_first_seen: true }, fmt).text, "<p> (first seen)");
});
