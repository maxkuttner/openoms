// Run with `npm test` (node --test; Node strips the types itself).
import { test } from "node:test";
import assert from "node:assert/strict";
import { reconnectDelayMs, staleFor, updatesUrl } from "./trade/api/liveUpdatesCore.ts";

const blotter = ["/orders", { portfolioId: "p", limit: 200 }];
const followed = ["/orders", "o-1", "state"];
const positions = ["/portfolios", "p", "positions"];
const marks = (ids: string[]) => ["/marks", ids];

test("orders notice stales blotter queries only", () => {
  const stale = staleFor({ type: "orders" });
  assert.equal(stale(blotter), true);
  assert.equal(stale(followed), true);
  assert.equal(stale(positions), false);
  assert.equal(stale(marks(["1"])), false);
});

test("positions notice stales positions queries only", () => {
  const stale = staleFor({ type: "positions" });
  assert.equal(stale(positions), true);
  assert.equal(stale(blotter), false);
  assert.equal(stale(marks(["1"])), false);
});

test("marks notice stales intersecting marks and all positions", () => {
  const stale = staleFor({ type: "marks", instrument_ids: [3] });
  assert.equal(stale(marks(["3", "4"])), true);
  assert.equal(stale(positions), true);
  assert.equal(stale(marks(["5"])), false);
  assert.equal(stale(blotter), false);
});

test("resync stales everything", () => {
  const stale = staleFor({ type: "resync" });
  for (const key of [blotter, followed, positions, marks(["9"]), ["/auth/me"]]) assert.equal(stale(key), true);
});

test("reconnect delay doubles to cap", () => {
  assert.deepEqual([0, 1, 2, 10].map(reconnectDelayMs), [1000, 2000, 4000, 30000]);
});

test("updates url follows page protocol", () => {
  assert.equal(updatesUrl({ protocol: "https:", host: "h" }, ""), "wss://h/updates");
  assert.equal(updatesUrl({ protocol: "http:", host: "h" }, "/api"), "ws://h/api/updates");
});
