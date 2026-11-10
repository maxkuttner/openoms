// Run with `npm test` (node --test; Node strips the types itself).
import { test } from "node:test";
import assert from "node:assert/strict";
import { activeDeployment, connectionEntries, deploymentHealth, isColocated, killLabel, reportSummary, savedAccounts, type Deployment } from "./strategies.ts";

test("a halted strategy offers Enable, any other offers Kill", () => {
  assert.equal(killLabel("HALTED"), "Enable");
  assert.equal(killLabel("ACTIVE"), "Kill");
  assert.equal(killLabel("REDUCING"), "Kill");
});

test("a control report reads as one line", () => {
  assert.equal(
    reportSummary({ cancelled: 3, cancel_failures: [], flatten_orders: 2, skipped: ["ETHBTC: no reference price"] }),
    "cancelled 3, flatten orders 2, skipped: ETHBTC: no reference price",
  );
  assert.equal(
    reportSummary({ cancelled: 1, cancel_failures: ["o1: broker rejected cancel"], flatten_orders: 0, skipped: [] }),
    "cancelled 1, failed: o1: broker rejected cancel",
  );
});

test("connections are the picked accounts with their brokers, one per broker", () => {
  const accounts = [
    { id: "a1", broker_connection_code: "binance-paper" },
    { id: "a2", broker_connection_code: "bybit-paper" },
  ];
  const connections = [
    { code: "binance-paper", broker_code: "BINANCE" },
    { code: "bybit-paper", broker_code: "BYBIT" },
  ];
  assert.deepEqual(connectionEntries(["a1", "a2"], accounts, connections), [
    { broker_code: "BINANCE", account_id: "a1" },
    { broker_code: "BYBIT", account_id: "a2" },
  ]);
});

test("the connections editor starts from the saved connections", () => {
  assert.deepEqual(savedAccounts([{ broker_code: "BINANCE", account_id: "a1" }, { broker_code: "BYBIT", account_id: "a2" }]), ["a1", "a2"]);
  assert.deepEqual(savedAccounts(undefined), []);
});

test("a co-located strategy is told apart by its mode", () => {
  assert.equal(isColocated({ mode: "COLOCATED" }), true);
  assert.equal(isColocated({ mode: "VIA_OMS" }), false);
});

const base: Deployment = {
  id: "d1",
  status: "ACTIVE",
  epoch: 3,
  policy_revision: 2,
  acked_policy_revision: 2,
  kill_epoch: null,
  kill_ack_stage: null,
  recovery_ready: true,
  recovery_missing: null,
  inventory_shortfall: null,
  created_at: "2026-11-10T08:00:00Z",
};

test("a reporting, unkilled, current deployment is healthy", () => {
  assert.deepEqual(deploymentHealth(base), [{ label: "healthy", color: "green" }]);
});

test("a shortfall and an unconfirmed kill come first and in red", () => {
  const h = deploymentHealth({ ...base, inventory_shortfall: "USDT is 500 short", kill_epoch: 3, kill_ack_stage: "CANCELS_SENT" });
  assert.deepEqual(h.map((x) => [x.label, x.color]), [["balance short", "red"], ["cancels sent", "red"]]);
  assert.equal(h[0].detail, "USDT is 500 short");
});

test("a confirmed kill is orange, a lagging policy is shown", () => {
  const h = deploymentHealth({ ...base, kill_epoch: 3, kill_ack_stage: "CONFIRMED_CLEAR", acked_policy_revision: 1 });
  assert.deepEqual(h.map((x) => x.label), ["killed, nothing resting", "policy 1 of 2"]);
});

test("a deployment whose runtime never connected says so", () => {
  const h = deploymentHealth({ ...base, recovery_ready: null, acked_policy_revision: null });
  assert.deepEqual(h.map((x) => x.label), ["policy not acknowledged", "runtime never reported"]);
});

test("the active deployment is the newest active one", () => {
  const old = { ...base, id: "old", created_at: "2026-11-01T00:00:00Z" };
  const retired = { ...base, id: "r", status: "RETIRED", created_at: "2026-11-11T00:00:00Z" };
  assert.equal(activeDeployment([old, base, retired])?.id, "d1");
  assert.equal(activeDeployment(undefined), undefined);
});
