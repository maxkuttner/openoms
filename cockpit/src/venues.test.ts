// Run with `npm test` (node --test; Node strips the types itself).
import { test } from "node:test";
import assert from "node:assert/strict";
import { autoCode, autoMaxOrderValue, connectionCode, environmentOptions, groupFields, initialValues, sectionStartsOpen, venueOptions } from "./venues.ts";
import type { Venue } from "./venues.ts";

const binance: Venue = {
  code: "BINANCE",
  name: "Binance",
  description: "Crypto spot pairs",
  docs_url: "https://example.org/binance/",
  universe: "About 2,000 spot pairs",
  environments: [
    { code: "PAPER", label: "spot testnet", keys_url: "https://testnet.example.org/" },
    { code: "LIVE", label: "live account", keys_url: null },
  ],
  fields: [
    { name: "api_key", label: "API key", secret: true, input: "text", section: null, paper: null, live: null },
    { name: "host", label: "Host", secret: false, input: "text", section: "FIX session", paper: "testnet.host", live: "live.host" },
    { name: "ssl", label: "TLS", secret: false, input: "checkbox", section: "FIX session", paper: null, live: null },
  ],
};

test("a connection code is venue and environment, lower case", () => {
  assert.equal(connectionCode("BYBIT", "PAPER"), "bybit-paper");
  assert.equal(connectionCode("", "PAPER"), "");
});

test("the code follows venue and environment until the user types their own", () => {
  const before = { broker_code: "BINANCE", environment: "PAPER", code: "binance-paper" };
  assert.equal(autoCode(before, { ...before, environment: "LIVE" }), "binance-live");
  assert.equal(autoCode({ broker_code: "", environment: "", code: "" }, { broker_code: "ALPACA", environment: "", code: "" }), "");
  const typed = { ...before, code: "my-binance" };
  assert.equal(autoCode(typed, { ...typed, environment: "LIVE" }), "my-binance");
});

test("the dropdown says what each venue trades", () => {
  assert.deepEqual(venueOptions([binance]), [{ value: "BINANCE", label: "Binance — Crypto spot pairs" }]);
});

test("environments say what they are at the chosen venue", () => {
  assert.deepEqual(environmentOptions(binance), [
    { value: "PAPER", label: "PAPER — spot testnet" },
    { value: "LIVE", label: "LIVE — live account" },
  ]);
  assert.deepEqual(environmentOptions(undefined).map((o) => o.value), ["PAPER", "LIVE"]);
});

test("a fresh form is pre-filled for its environment, secrets never", () => {
  assert.deepEqual(initialValues(binance.fields, "PAPER", new Map()), { api_key: "", host: "testnet.host", ssl: "true" });
  assert.equal(initialValues(binance.fields, "LIVE", new Map()).host, "live.host");
});

test("a stored value wins over the pre-fill", () => {
  const stored = new Map([["host", { value: "my.host" }], ["api_key", { value: "leaked?" }]]);
  const vals = initialValues(binance.fields, "PAPER", stored);
  assert.equal(vals.host, "my.host");
  assert.equal(vals.api_key, "");
});

test("a stored credential is never pre-filled, so a masked field stays blank", () => {
  const vals = initialValues(binance.fields, null, new Map([["host", { value: null }]]));
  assert.equal(vals.host, "");
});

test("connection settings are grouped under their section, credentials stay on top", () => {
  const { main, sections } = groupFields(binance.fields);
  assert.deepEqual(main.map((f) => f.name), ["api_key"]);
  assert.deepEqual(sections.map((s) => [s.name, s.fields.map((f) => f.name)]), [["FIX session", ["host", "ssl"]]]);
});

test("a section starts closed when everything in it is filled in", () => {
  const fix = groupFields(binance.fields).sections[0].fields;
  assert.equal(sectionStartsOpen(fix, { host: "testnet.host", ssl: "false" }), false);
  assert.equal(sectionStartsOpen(fix, { host: "", ssl: "true" }), true);
});

test("picking LIVE fills in the 1,000 order-value cap unless one was typed", () => {
  const none = { environment: "", max_order_value: "" };
  assert.equal(autoMaxOrderValue(none, { ...none, environment: "LIVE" }), 1000);
  assert.equal(autoMaxOrderValue(none, { ...none, environment: "PAPER" }), "");
  // Switching back to PAPER leaves the auto-filled cap alone; the admin can clear it.
  assert.equal(autoMaxOrderValue({ environment: "LIVE", max_order_value: 1000 }, { environment: "PAPER", max_order_value: 1000 }), 1000);
  assert.equal(autoMaxOrderValue({ environment: "", max_order_value: 250 }, { environment: "LIVE", max_order_value: 250 }), 250);
});
