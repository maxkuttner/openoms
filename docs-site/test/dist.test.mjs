// Checks against the built site in dist/ (run `npm run build` first, or
// `npm run check` for both). These pin what Starlight's own build does not:
// the install URL people pipe into a shell, and that everything works under
// the /openoms/ base path GitHub Pages serves the site from.
import { test } from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";

const DIST = new URL("../dist/", import.meta.url).pathname;
const BASE = "/openoms/";

function htmlFiles(dir) {
  return readdirSync(dir).flatMap((name) => {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) return htmlFiles(p);
    return name.endsWith(".html") ? [p] : [];
  });
}

test("install.sh in dist is byte-identical to the repo copy", () => {
  const built = readFileSync(join(DIST, "install.sh"));
  const repo = readFileSync(new URL("../../install.sh", import.meta.url));
  assert.deepEqual(built, repo);
});

test("the home page carries the install one-liner", () => {
  const html = readFileSync(join(DIST, "index.html"), "utf8");
  assert.ok(
    html.includes("curl -fsSL https://maxkuttner.github.io/openoms/install.sh | sh"),
    "install command missing from the home page",
  );
});

test("every internal href in dist resolves to a built file", () => {
  const broken = [];
  for (const file of htmlFiles(DIST)) {
    const html = readFileSync(file, "utf8");
    for (const [, url] of html.matchAll(/\s(?:href|src)="(\/[^"]*)"/g)) {
      if (url.startsWith("//")) continue; // protocol-relative: external
      if (!url.startsWith(BASE)) {
        broken.push(`${file}: ${url} (outside ${BASE})`);
        continue;
      }
      let rel = decodeURI(url.slice(BASE.length).split("#")[0].split("?")[0]);
      if (rel === "" || rel.endsWith("/")) rel += "index.html";
      if (!existsSync(join(DIST, rel))) broken.push(`${file}: ${url}`);
    }
  }
  assert.deepEqual(broken, []);
});

// The trading endpoints a reader of "Your first API call" needs. Each must have
// its own generated page, so a plugin change or a spec-shape the plugin skips
// cannot silently drop them from the reference.
const TRADING_OPERATIONS = [
  ["post", "/orders/submit"],
  ["post", "/orders/cancel"],
  ["get", "/orders"],
  ["get", "/orders/{id}"],
];

test("the API reference has a page for each trading endpoint", () => {
  const spec = JSON.parse(readFileSync(new URL("../../docs/openapi.json", import.meta.url), "utf8"));
  const pages = htmlFiles(join(DIST, "api")).map((f) => readFileSync(f, "utf8"));
  for (const [method, path] of TRADING_OPERATIONS) {
    const op = spec.paths[path]?.[method];
    assert.ok(op, `${method.toUpperCase()} ${path} is not in docs/openapi.json`);
    const title = op.summary ?? op.operationId;
    const page = pages.find((html) => html.includes(`<title>${title}`) && html.includes(path));
    assert.ok(page, `no API page titled "${title}" showing ${path}`);
  }
});

test("every operation in the spec has a page", () => {
  const spec = JSON.parse(readFileSync(new URL("../../docs/openapi.json", import.meta.url), "utf8"));
  const operations = Object.values(spec.paths).reduce((n, ops) => n + Object.keys(ops).length, 0);
  const operationPages = htmlFiles(join(DIST, "api", "operations"));
  assert.ok(
    operationPages.length >= operations,
    `${operationPages.length} operation pages for ${operations} operations`,
  );
});

test("the architecture page renders all three diagrams", () => {
  const html = readFileSync(join(DIST, "concepts/architecture/index.html"), "utf8");
  for (const name of ["overview", "er", "runtime"]) {
    assert.ok(html.includes(`data-diagram="${name}"`), `diagram "${name}" missing`);
  }
  // Rendered in the browser (no headless Chromium in CI): the page must ship
  // the source for mermaid to render, not an empty placeholder.
  assert.equal(html.match(/<pre class="mermaid\b/g)?.length, 3, "expected three mermaid blocks");
  assert.ok(html.includes("flowchart LR") && html.includes("erDiagram"), "diagram source not shipped");
});

test("the OPRA tutorial is published", () => {
  const html = readFileSync(join(DIST, "tutorials/opra-marks-pnl/index.html"), "utf8");
  assert.ok(!html.includes("This page is being written."), "still the placeholder");
});
