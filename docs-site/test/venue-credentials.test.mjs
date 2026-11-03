// The cockpit's credentials drawer shows bare field labels and links to the
// venue's docs page for what they mean, so every field src/venues.rs lists must
// have a row in that page's credentials table: `| <Label> | … |`.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const VENUES_RS = readFileSync(new URL("../../src/venues.rs", import.meta.url), "utf8");
const page = (slug) => readFileSync(new URL(`../src/content/docs/integrations/${slug}.md`, import.meta.url), "utf8");

export function venuesFrom(source) {
  const body = source.slice(source.indexOf("pub fn venues()"), source.indexOf("#[cfg(test)]"));
  return body
    .split(/\n\s*Venue \{/)
    .slice(1)
    .map((block) => ({
      code: block.match(/code: "([A-Z]+)"/)[1],
      slug: block.match(/\{DOCS\}\/([a-z-]+)\//)[1],
      labels: [...block.matchAll(/field\("[a-z_]+", "([^"]+)"\)/g)].map((m) => m[1]),
    }));
}

test("the venue list is read from src/venues.rs", () => {
  const venues = venuesFrom(VENUES_RS);
  assert.ok(venues.length >= 3, "expected at least Alpaca, Binance and IBKR");
  for (const v of venues) assert.ok(v.labels.length > 0, `${v.code} has no fields`);
});

for (const venue of venuesFrom(VENUES_RS)) {
  test(`${venue.code}'s docs page explains every credentials field`, () => {
    const text = page(venue.slug);
    assert.match(text, /^## Credentials$/m, `${venue.slug}.md has no "## Credentials" section`);
    const escape = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    for (const label of venue.labels) {
      assert.match(text, new RegExp(`^\\| ${escape(label)} \\|`, "m"), `${venue.slug}.md: no row for "${label}"`);
    }
  });
}
