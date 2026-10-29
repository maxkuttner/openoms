// The workflows that build this site must ask for a Node that Astro accepts.
// Astro hard-exits below its floor, and a bare "22" resolves to whatever 22.x
// the runner image has cached — which can be older than that floor.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const ASTRO_MIN = [22, 23, 4]; // node_modules/astro/bin/astro.mjs `engines`

function nodeVersions(workflow) {
  const text = readFileSync(new URL(`../../.github/workflows/${workflow}`, import.meta.url), "utf8");
  return [...text.matchAll(/node-version:\s*"([^"]+)"/g)].map((m) => m[1]);
}

// A partial version ("24") means the newest of that line; one that names a
// major above Astro's floor major is always new enough. An exact one must
// meet the floor.
function satisfies(v) {
  const p = v.split(".").map(Number);
  if (p.length === 1) return p[0] > ASTRO_MIN[0];
  for (let i = 0; i < 3; i++) {
    const a = p[i] ?? 0;
    if (a !== ASTRO_MIN[i]) return a > ASTRO_MIN[i];
  }
  return true;
}

test("the version check itself", () => {
  assert.equal(satisfies("22"), false);
  assert.equal(satisfies("22.23.3"), false);
  assert.equal(satisfies("22.23.4"), true);
  assert.equal(satisfies("24"), true);
});

test("pages.yml builds the site on a Node Astro accepts", () => {
  const versions = nodeVersions("pages.yml");
  assert.ok(versions.length > 0, "no node-version in pages.yml");
  for (const v of versions) assert.ok(satisfies(v), `pages.yml node-version "${v}" is below Astro's floor`);
});

test("build.yml's docs-site job builds on a Node Astro accepts", () => {
  const text = readFileSync(new URL("../../.github/workflows/build.yml", import.meta.url), "utf8");
  const job = text.slice(text.indexOf("  docs-site:"));
  const v = job.match(/node-version:\s*"([^"]+)"/)?.[1];
  assert.ok(v, "docs-site job has no node-version");
  assert.ok(satisfies(v), `docs-site job node-version "${v}" is below Astro's floor`);
});
