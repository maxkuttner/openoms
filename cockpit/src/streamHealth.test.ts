// Run with `npm test` (node --test; Node strips the types itself).
import { test } from "node:test";
import assert from "node:assert/strict";
import { healthLabel } from "./streamHealth.ts";
import type { StreamHealth } from "./api/types.ts";

const base: StreamHealth = {
  broker_code: "IBKR_TWS",
  environment: "PAPER",
  kind: "execution",
  transport: "websocket",
  state: "connecting",
  connected_since: null,
  last_event_at: null,
  last_error: null,
};

test("a connecting stream shows why", () => {
  assert.equal(
    healthLabel({ ...base, last_error: "IB Gateway is starting" }),
    "connecting: IB Gateway is starting · last event never",
  );
});

test("a down stream shows its error and a live one its uptime", () => {
  assert.equal(healthLabel({ ...base, state: "down", last_error: "boom" }), "down: boom · last event never");
  const now = Date.parse("2026-01-01T00:01:00Z");
  const live = { ...base, state: "live" as const, connected_since: "2026-01-01T00:00:00Z", last_event_at: "2026-01-01T00:00:30Z" };
  assert.equal(healthLabel(live, now), "connected 1m ago · last event 30s ago");
});
