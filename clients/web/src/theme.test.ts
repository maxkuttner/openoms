import { test } from "node:test";
import assert from "node:assert/strict";
import { otherTheme, parseTheme } from "./theme.ts";

test("parseTheme defaults to dark", () => {
  assert.equal(parseTheme(null), "dark");
  assert.equal(parseTheme("nonsense"), "dark");
  assert.equal(parseTheme("dark"), "dark");
  assert.equal(parseTheme("light"), "light");
});

test("otherTheme flips", () => {
  assert.equal(otherTheme("dark"), "light");
  assert.equal(otherTheme("light"), "dark");
});
