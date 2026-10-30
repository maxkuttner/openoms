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
  ["get", "/portfolios"],
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

test("every operation in the spec has its own page", () => {
  const spec = JSON.parse(readFileSync(new URL("../../docs/openapi.json", import.meta.url), "utf8"));
  const ids = Object.values(spec.paths).flatMap((ops) => Object.values(ops).map((op) => op.operationId));
  // Pages are keyed by operationId: two operations sharing one get one page.
  assert.equal(new Set(ids).size, ids.length, "duplicate operationIds in docs/openapi.json");
  const missing = ids.filter((id) => !existsSync(join(DIST, "api", "operations", id, "index.html")));
  assert.deepEqual(missing, []);
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

// The old hand-built site's pages. Every shipped cockpit links to
// /openoms/architecture.html, so these must keep resolving.
for (const [old, target] of [
  ["architecture.html", "/openoms/concepts/architecture/"],
  ["api.html", "/openoms/api/"],
  ["getting-started/first-run/index.html", "/openoms/getting-started/"],
]) {
  test(`the old /openoms/${old} redirects to ${target}`, () => {
    const html = readFileSync(join(DIST, old), "utf8");
    assert.match(html, new RegExp(`<meta http-equiv="refresh" content="0; url=${target}"`));
    assert.ok(existsSync(join(DIST, target.slice(BASE.length), "index.html")), `${target} is not built`);
  });
}

test("operation pages show cURL, Python and Rust samples, not JavaScript", () => {
  const html = readFileSync(join(DIST, "api/operations/orders_submit/index.html"), "utf8");
  assert.ok(html.includes("curl --request POST"), "no cURL sample");
  assert.ok(html.includes("requests.post("), "no Python sample");
  assert.ok(html.includes("reqwest::Client::new()"), "no Rust sample");
  assert.ok(!html.includes("fetch(url, options)"), "JavaScript sample still present");
});

test("the landing page has its background image, in dark mode only", () => {
  const css = readdirSync(join(DIST, "_astro"))
    .filter((f) => f.endsWith(".css"))
    .map((f) => readFileSync(join(DIST, "_astro", f), "utf8"))
    .join("\n");
  const rule = css.match(/\[data-theme=["']?dark["']?\]\[data-has-hero\][^{]*\{[^}]*background-image:url\(([^)]+)\)/);
  assert.ok(rule, "no dark-mode, landing-only background rule");
  const url = rule[1].replace(/["']/g, "");
  assert.ok(url.startsWith(BASE), `background url ${url} is not under ${BASE}`);
  assert.ok(existsSync(join(DIST, url.slice(BASE.length))), `${url} is not built`);
});

test("the landing page ships the interactive grain canvas", () => {
  const html = readFileSync(join(DIST, "index.html"), "utf8");
  assert.match(html, /<canvas[^>]*id="grain-bg"/, "no grain canvas on the landing page");
  assert.match(html, /aria-hidden="true"/, "the decorative canvas must be hidden from screen readers");
  const other = readFileSync(join(DIST, "getting-started/index.html"), "utf8");
  assert.ok(!other.includes('id="grain-bg"'), "grain canvas leaked onto a docs page");
});

test("the landing background never moves: no animation loop, no pointer tracking", () => {
  const html = readFileSync(join(DIST, "index.html"), "utf8");
  // Only the script that draws the grain: Starlight's own scripts on the page
  // (code blocks, search) legitimately use requestAnimationFrame.
  const inline = [...html.matchAll(/<script[^>]*>([\s\S]*?)<\/script>/g)].map((m) => m[1]);
  const bundled = [...html.matchAll(/<script[^>]*src="\/openoms\/(_astro\/[^"]+\.js)"/g)].map((m) =>
    readFileSync(join(DIST, m[1]), "utf8"),
  );
  const grain = inline.concat(bundled).filter((src) => src.includes("grain-bg"));
  assert.equal(grain.length, 1, "expected exactly one script drawing the grain background");
  assert.ok(!/requestAnimationFrame/.test(grain[0]), "grain background still animates");
  assert.ok(!/pointermove/.test(grain[0]), "grain background still follows the pointer");
});

test("the grain background shows in light mode too", () => {
  const html = readFileSync(join(DIST, "index.html"), "utf8");
  // Astro inlines a small component stylesheet into the page; check both.
  const css = readdirSync(join(DIST, "_astro"))
    .filter((f) => f.endsWith(".css"))
    .map((f) => readFileSync(join(DIST, "_astro", f), "utf8"))
    .concat([...html.matchAll(/<style[^>]*>([\s\S]*?)<\/style>/g)].map((m) => m[1]))
    .join("\n");
  // The reveal rule must not be limited to the dark theme.
  // Astro scopes component styles: #grain-bg:where(.astro-…)[data-ready].
  const reveal = css.match(/([^{}]*)#grain-bg(?::where\([^)]*\))?\[data-ready\][^{]*\{[^}]*opacity:\s*1/);
  assert.ok(reveal, "no rule revealing the drawn canvas");
  assert.ok(!/data-theme=["']?dark/.test(reveal[1]), `reveal rule is dark-only: ${reveal[1].trim()}`);
  assert.match(html, /uniform float light/, "shader has no light-scheme variant");
});

test("installation has OS tabs and links the installer source, not the raw script", () => {
  const html = readFileSync(join(DIST, "getting-started/installation/index.html"), "utf8");
  for (const tab of ["macOS &amp; Linux", "Windows", "Package Managers", "Docker"]) assert.ok(html.includes(`>${tab}<`), `${tab} tab missing`);
  assert.ok(html.includes('data-sync-key="os"'), "tabs not synced across pages");
  assert.ok(html.includes("https://github.com/maxkuttner/openoms/blob/main/install.sh"));
  assert.ok(!html.includes('href="/openoms/install.sh"'), "still links the raw script");
});

test("installation's Package Managers and Docker tabs give real commands", () => {
  const html = readFileSync(join(DIST, "getting-started/installation/index.html"), "utf8");
  // The first tab group's panels, in label order: macOS & Linux, Windows, Package Managers, Docker.
  const panels = html.split(/<div[^>]*role="tabpanel"/).slice(1, 5);
  const [, , pkg, docker] = panels;
  assert.ok(pkg.includes("brew install maxkuttner/tap/openoms"), "no Homebrew command");
  assert.ok(docker.includes("docker compose run --rm oms init"), "no compose flow");
  assert.ok(docker.includes("compose.yaml"), "compose.yaml not linked");
  assert.ok(!docker.includes("To be implemented"), "Docker tab still says To be implemented");
  assert.ok(existsSync(join(DIST, "compose.yaml")), "compose.yaml is not published");
  // Re-running the block must not replace the password the Postgres volume already holds.
  assert.ok(docker.includes("[ -f .env ] ||"), ".env is overwritten on a re-run");
  assert.ok(docker.includes("docker compose cp oms:/config/oms.toml"), "no way to back up the master key");
});
