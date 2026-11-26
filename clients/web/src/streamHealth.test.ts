// Run with `npm test` (node --test; Node strips the types itself).
import { test } from "node:test";
import assert from "node:assert/strict";
import { describeStream, lastEventLabel } from "./streamHealth.ts";
import type { StreamHealth } from "./api/types.ts";

const now = Date.parse("2026-01-01T00:01:00Z");
const at = (secsAgo: number) => new Date(now - secsAgo * 1000).toISOString();

const base: StreamHealth = {
  broker_code: "IBKR_TWS",
  environment: "PAPER",
  kind: "execution",
  transport: "websocket",
  state: "connecting",
  connected_since: null,
  last_event_at: null,
  last_error: null,
  last_checked_at: null,
  next_retry_at: null,
  heartbeat_secs: 30,
};

test("live says when it last checked", () => {
  const s = { ...base, state: "live" as const, connected_since: at(60), last_checked_at: at(5) };
  assert.equal(describeStream(s, now), "live · checked 5s ago");
});

test("an idle live stream says so", () => {
  const s = { ...base, kind: "feed" as const, state: "live" as const, last_error: "idle: nothing held to price", last_checked_at: at(4) };
  assert.equal(describeStream(s, now), "live · idle: nothing held to price · checked 4s ago");
});

test("connecting shows the attempt's reason, or nothing", () => {
  assert.equal(
    describeStream({ ...base, last_error: "IB Gateway connection lost; reconnecting" }, now),
    "connecting… IB Gateway connection lost; reconnecting",
  );
  assert.equal(describeStream(base, now), "connecting…");
});

test("down shows why, when it retries and when it last checked", () => {
  const s = {
    ...base,
    state: "down" as const,
    last_error: "IB Gateway at gw:4004 not reachable: Name or service not known",
    next_retry_at: at(-30),
    last_checked_at: at(10),
  };
  assert.equal(
    describeStream(s, now),
    "down: IB Gateway at gw:4004 not reachable: Name or service not known · retrying in 30s · checked 10s ago",
  );
  // A retry already due reads "retrying now"; no retry time, no retry clause.
  assert.equal(describeStream({ ...s, next_retry_at: at(2) }, now).includes("retrying now"), true);
  assert.equal(describeStream({ ...s, next_retry_at: null, last_checked_at: null }, now), `down: ${s.last_error}`);
});

test("stopped tells the user what to do", () => {
  const s = { ...base, state: "stopped" as const, last_error: "IB Gateway at gw:4004 is logged into a live account but this connection is PAPER" };
  assert.equal(
    describeStream(s, now),
    "stopped: IB Gateway at gw:4004 is logged into a live account but this connection is PAPER — fix the credentials and save",
  );
});

test("the last event names what it was, and is absent when there was none", () => {
  assert.equal(lastEventLabel(base, now), null);
  assert.equal(lastEventLabel({ ...base, last_event_at: at(120) }, now), "last order/fill event 2m ago");
  assert.equal(lastEventLabel({ ...base, kind: "feed", last_event_at: at(3) }, now), "last quote 3s ago");
});
