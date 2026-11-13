import { test } from "node:test";
import assert from "node:assert/strict";
import { formatFees } from "./fees.ts";

test("formatFees joins per-currency amounts", () => {
  assert.equal(formatFees([{ currency: "USD", amount: 1.5 }, { currency: "BNB", amount: 0.01 }]), "1.50 USD · 0.01 BNB");
});

test("formatFees is empty with no fees", () => {
  assert.equal(formatFees([]), "");
  assert.equal(formatFees(undefined), "");
});

test("formatFees keeps small crypto amounts and rebate signs", () => {
  assert.equal(formatFees([{ currency: "BTC", amount: 0.00001 }]), "0.00001 BTC");
  assert.equal(formatFees([{ currency: "USDT", amount: -0.25 }]), "-0.25 USDT");
});
