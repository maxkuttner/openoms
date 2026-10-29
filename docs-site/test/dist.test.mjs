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
