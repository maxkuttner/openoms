# Docs to GitHub Pages Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move the cockpit's architecture page and API reference onto the public Pages site as static HTML, link them from the landing page, and delete them from the cockpit.

**Architecture:** A new `oms openapi` subcommand prints the OpenAPI spec; the generated spec is committed at `docs/openapi.json` and guarded by a Rust drift test, so the Pages job needs no Rust build. A dependency-free Node script renders that spec plus four mermaid diagrams into static HTML in the landing page's visual language. The cockpit then loses both pages and its `mermaid` dependency.

**Tech Stack:** Rust (clap 4, utoipa 4), Node 22 (built-in test runner, no npm deps in the build script), `@mermaid-js/mermaid-cli` in CI only, GitHub Actions, hand-written HTML/CSS.

**Spec:** `docs/superpowers/specs/2026-09-14-docs-to-pages-design.md`

## Global Constraints

- **Published pages carry no framework, no CDN, no external network requests.** Inline SVG and inline CSS/JS only. This is the same rule the landing page follows and the reason it exists: the site tells people to pipe a script into their shell.
- **Site palette** (already in `site/style.css` `:root`): `--bg: #0d1014`, `--panel: #151a20`, `--border: #2a2f38`, `--text: #d7dce3`, `--dim: #8b949e`, `--accent: #25b083`. The docs pages reuse these tokens — no new colour values except the method/status badges named in Task 3.
- **`/scalar` and `/api-docs/openapi.json` stay on the OMS binary.** They are API endpoints with their own consumers. Nothing in this plan removes or changes them.
- **`cockpit/src/components/apiTheme.tsx` stays.** `cockpit/src/pages/Api.tsx:16` imports `C`, `CmdLine` and `Eyebrow` from it.
- **Plain npm only** — never `--legacy-peer-deps` or `--force`.
- **A YAML `run:` scalar containing a colon followed by a space must be quoted**, or the file is invalid YAML. Run `actionlint` (at `/opt/homebrew/bin/actionlint`) on any workflow you touch.
- Tasks 3 and 4 port content out of `cockpit/src/pages/ApiDocs.tsx` and `cockpit/src/pages/Architecture.tsx`. **Those files are deleted in Task 6 — do not delete them earlier**, they are the source material.

---

### Task 1: `oms openapi`

The docs build needs the spec without a running server. Today the only way to get it is to GET `/api-docs/openapi.json` off a live OMS.

**Files:**
- Modify: `src/main.rs` — the `Command` enum (starts at line 301) and the `match cli.command` block (starts at line 441, ends with `None => serve().await,` at line 603)

**Interfaces:**
- Consumes: the existing `ApiDoc` struct (`src/main.rs:163`) and its `utoipa::OpenApi` derive.
- Produces: `oms openapi` prints the spec as pretty-printed JSON to stdout and exits 0, touching no database and no config. Task 2 pipes it into `docs/openapi.json`; Task 3 renders that file.

- [ ] **Step 1: Write the failing test**

Add to the existing `mod tests` block at the bottom of `src/main.rs`:

```rust
    #[test]
    fn the_openapi_subcommand_renders_a_usable_spec() {
        // The docs site is built from this JSON, not from a running server, so the
        // command has to produce a complete spec with no database and no config.
        let json = super::openapi_json();
        let spec: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");

        assert!(spec["openapi"].as_str().is_some(), "missing openapi version");
        assert!(
            spec["paths"].as_object().map(|p| !p.is_empty()).unwrap_or(false),
            "spec has no paths"
        );
        assert!(
            spec["paths"]["/orders/submit"].is_object(),
            "expected /orders/submit in the spec, got: {:?}",
            spec["paths"].as_object().map(|p| p.keys().collect::<Vec<_>>())
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test the_openapi_subcommand_renders_a_usable_spec`
Expected: FAIL to compile — `cannot find function 'openapi_json' in this scope`.

- [ ] **Step 3: Write the implementation**

Add the function near `ApiDoc` in `src/main.rs`:

```rust
/// The OpenAPI spec as pretty JSON.
///
/// Split out from the subcommand so a test can assert the spec is complete without
/// spawning a process. `docs/openapi.json` is generated from this and committed;
/// see the drift test that keeps the two in step.
fn openapi_json() -> String {
    serde_json::to_string_pretty(&ApiDoc::openapi()).expect("the OpenAPI spec must serialise")
}
```

Add the variant to the `Command` enum (after the `Init { .. }` variant, keeping the enum's existing doc-comment style):

```rust
    /// Print the OpenAPI spec as JSON, for the docs build and for scripting.
    Openapi,
```

Add the arm to `match cli.command`, immediately before `None => serve().await,`:

```rust
        Some(Command::Openapi) => println!("{}", openapi_json()),
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test the_openapi_subcommand_renders_a_usable_spec`
Expected: PASS

Then confirm the real command works and needs nothing provisioned:

Run: `cargo run --quiet -- openapi | head -3`
Expected: JSON beginning with `{` and an `"openapi":` line — no database connection attempted, no config error.

Run: `cargo run --quiet -- openapi | python3 -c 'import json,sys; d=json.load(sys.stdin); print(len(d["paths"]), "paths")'`
Expected: a non-zero path count.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat(cli): add oms openapi

The docs site renders the spec statically, so it needs the JSON at build
time — and the only way to get it until now was to GET it off a running
server. The subcommand touches no database and reads no config, because
the machine that needs it is a CI runner with nothing provisioned."
```

---

### Task 2: Commit the spec, and a test that catches drift

**Files:**
- Create: `docs/openapi.json` (generated, committed)
- Modify: `src/main.rs` — the `mod tests` block

**Interfaces:**
- Consumes: `openapi_json()` from Task 1.
- Produces: `docs/openapi.json`, the file Task 3's build script reads and Task 5's workflow copies.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `src/main.rs`:

```rust
    #[test]
    fn the_committed_openapi_spec_is_current() {
        // The docs site renders docs/openapi.json rather than calling a running
        // server, so a stale file silently publishes a wrong API reference.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/openapi.json");
        let committed = std::fs::read_to_string(path)
            .expect("docs/openapi.json is missing — regenerate: cargo run -- openapi > docs/openapi.json");

        assert_eq!(
            committed.trim(),
            super::openapi_json().trim(),
            "docs/openapi.json is out of date. Regenerate it:\n\n    \
             cargo run -- openapi > docs/openapi.json\n"
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test the_committed_openapi_spec_is_current`
Expected: FAIL — panics with "docs/openapi.json is missing — regenerate: …", because the file does not exist yet.

- [ ] **Step 3: Generate the file**

Run:

```bash
cargo run --quiet -- openapi > docs/openapi.json
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test the_committed_openapi_spec_is_current`
Expected: PASS

- [ ] **Step 5: Prove the drift test actually catches drift**

A snapshot test that cannot fail is worse than none. Corrupt the file, watch it fail, restore it:

```bash
python3 - <<'PY'
import json
d = json.load(open("docs/openapi.json"))
d["paths"]["/not-a-real-endpoint"] = {}
json.dump(d, open("docs/openapi.json", "w"), indent=2)
PY
cargo test the_committed_openapi_spec_is_current 2>&1 | tail -20
```

Expected: FAIL, with the message naming `cargo run -- openapi > docs/openapi.json`.

Then restore and confirm green:

```bash
cargo run --quiet -- openapi > docs/openapi.json
cargo test the_committed_openapi_spec_is_current
```

Expected: PASS. Put both outputs in your report.

- [ ] **Step 6: Commit**

```bash
git add docs/openapi.json src/main.rs
git commit -m "feat(docs): commit the OpenAPI spec, with a drift test

The Pages job renders this file rather than building the binary, which
keeps a C++ FIX engine compile out of every docs deploy. The trade is a
generated file in the tree, so a test asserts it still matches what the
code produces and names the regeneration command when it does not."
```

---

### Task 3: Render the API reference to static HTML

**Files:**
- Create: `site/build-docs.mjs`
- Create: `site/build-docs.test.mjs`
- Modify: `site/style.css` (append a docs section)
- Read (do not modify): `cockpit/src/pages/ApiDocs.tsx`, `cockpit/src/components/apiTheme.tsx`

**Interfaces:**
- Consumes: `docs/openapi.json` from Task 2.
- Produces: `node site/build-docs.mjs <out-dir>` writes `<out-dir>/api.html`. Task 4 extends the same script to also write `architecture.html`. Task 5 runs it in CI.

- [ ] **Step 1: Read the source you are porting**

Read `cockpit/src/pages/ApiDocs.tsx` in full before writing anything. It already contains the logic in near-portable form — plain functions over the spec:

- `resolve(spec, s)` — follows a `$ref` into `components.schemas`
- `typeLabel(spec, s)` — renders a schema as a short type string
- `example(spec, s)` — builds an example value for a schema
- `curlFor(spec, method, path, op)` — the `curl` line for an operation
- `respColor(code)` — status-code colour
- `MethodBadge`, `Row`, `Panel`, `Operation` — presentation, and the grouping in `ApiDocsPage` that buckets operations by tag

Port the first five as-is. Replace the last four with functions returning HTML strings. Do not redesign the output; this is a translation.

Also read `cockpit/src/components/apiTheme.tsx` for the palette (`C.panel`, `C.inset`, `C.border`, `C.ink`, `C.muted`, `C.faint`, `C.green`, `C.amber`, `C.red`) — those values become the docs CSS in Step 5.

- [ ] **Step 2: Write the failing tests**

Node 22 has a built-in test runner, so this needs no dependency. Create `site/build-docs.test.mjs`:

```js
import { test } from "node:test";
import assert from "node:assert/strict";
import { typeLabel, curlFor, escapeHtml, renderApi } from "./build-docs.mjs";

const spec = {
  openapi: "3.0.3",
  paths: {
    "/orders/submit": {
      post: {
        tags: ["orders"],
        summary: "Submit an order",
        requestBody: {
          content: {
            "application/json": {
              schema: { $ref: "#/components/schemas/SubmitOrderRequest" },
            },
          },
        },
        responses: { 200: { description: "accepted" }, 422: { description: "rejected" } },
      },
    },
  },
  components: {
    schemas: {
      SubmitOrderRequest: {
        type: "object",
        required: ["symbol"],
        properties: {
          symbol: { type: "string", example: "SPY@ARCX" },
          quantity: { type: "integer", format: "int64" },
        },
      },
    },
  },
};

test("typeLabel follows a $ref and names the type", () => {
  assert.equal(typeLabel(spec, { $ref: "#/components/schemas/SubmitOrderRequest" }), "object");
  assert.equal(typeLabel(spec, { type: "integer", format: "int64" }), "int64");
  assert.equal(typeLabel(spec, { type: "array", items: { type: "string" } }), "string[]");
});

test("curlFor builds a request against the documented path", () => {
  const curl = curlFor(spec, "post", "/orders/submit", spec.paths["/orders/submit"].post);
  assert.match(curl, /curl/);
  assert.match(curl, /-X POST/);
  assert.match(curl, /\/orders\/submit/);
  assert.match(curl, /SPY@ARCX/, "the body should use the schema's example");
});

test("escapeHtml neutralises markup from the spec", () => {
  // Descriptions come from doc comments in Rust source; a stray angle bracket
  // must not become a tag in the generated page.
  assert.equal(escapeHtml(`<script>"x"&`), "&lt;script&gt;&quot;x&quot;&amp;");
});

test("renderApi emits one section per tag and documents each operation", () => {
  const html = renderApi(spec);
  assert.match(html, /orders/, "tag heading missing");
  assert.match(html, /\/orders\/submit/, "path missing");
  assert.match(html, /POST/, "method missing");
  assert.match(html, /422/, "response code missing");
  assert.doesNotMatch(html, /<script src|https?:\/\/(?!localhost)/, "no external references allowed");
});
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `node --test site/`
Expected: FAIL — cannot resolve `./build-docs.mjs`.

- [ ] **Step 4: Write the build script**

Create `site/build-docs.mjs`. It must:

- Start with `#!/usr/bin/env node` and a comment block explaining that it renders the committed spec into static HTML because the published site has no server to ask.
- Export `resolve`, `typeLabel`, `example`, `curlFor`, `respColor`, `escapeHtml` and `renderApi` (so the tests can import them), and run `main()` when invoked directly.
- `escapeHtml(s)` escapes `&`, `<`, `>` and `"` — **every** value interpolated from the spec goes through it. Spec text originates in Rust doc comments and must never become markup.
- `renderApi(spec)` returns the full HTML document: the same `<head>` shape as `site/index.html` (charset, viewport, `<title>openoms — API reference</title>`, favicon, `style.css`), a header linking back to `index.html`, then one `<section>` per tag, each containing one block per operation with method badge, path, summary, parameters, request-body fields, response codes, and the `curl` example.
- `main()` reads `docs/openapi.json` relative to the repo root (derive it from `import.meta.url`, do not assume the CWD), and writes `api.html` into the directory given as `process.argv[2]`, defaulting to `_site`. Create the directory if absent.
- Use no npm dependencies. `node:fs`, `node:path` and `node:url` only.

Group operations by their first tag, matching `ApiDocsPage`'s current behaviour; operations with no tag go into a final `other` group.

- [ ] **Step 5: Add the docs styles**

Append a clearly commented docs section to `site/style.css`, reusing the existing `:root` tokens. It needs classes for: the operation block, the method badge (GET/POST/PATCH/DELETE), the parameter/field table rows, the response-code list, and the `curl` block. Take the specific badge and status colours from `apiTheme.tsx`'s palette: `green #22ae6c`, `amber #ce9a3b`, `red #b0554f`, `faint #6b7280`.

Do not introduce a new font or any `@import`.

- [ ] **Step 6: Run tests to verify they pass**

Run: `node --test site/`
Expected: PASS, 4 tests.

- [ ] **Step 7: Render the real spec and inspect it**

Run:

```bash
node site/build-docs.mjs /tmp/docs-out && ls -la /tmp/docs-out/
python3 -c "
import re,sys
h=open('/tmp/docs-out/api.html').read()
print('bytes:', len(h))
print('external refs:', re.findall(r'https?://(?!localhost)[^\"\\s]+', h)[:5])
print('script tags:', h.count('<script'))
"
```

Expected: `api.html` exists; the only external references are the GitHub links in the header/footer (those are `<a href>`, which is fine — what must not appear is any `src`/`@import`/`link rel=stylesheet` to another host); `<script` count is 0.

- [ ] **Step 8: Commit**

```bash
git add site/build-docs.mjs site/build-docs.test.mjs site/style.css
git commit -m "feat(site): render the API reference to static HTML

Ported from the cockpit's ApiDocs page, which was already plain functions
over the spec — the change is JSX to template strings and Mantine to the
site's own CSS. Everything interpolated from the spec is escaped: those
strings come from Rust doc comments and must not become markup.

No npm dependencies; node:fs and the built-in test runner are enough."
```

---

### Task 4: The architecture page and its diagrams

**Files:**
- Create: `site/diagrams/overview.mmd`, `site/diagrams/er.mmd`, `site/diagrams/seed.mmd`, `site/diagrams/runtime.mmd`
- Create: `site/architecture.html`
- Modify: `site/build-docs.mjs` (inline the rendered SVGs), `site/build-docs.test.mjs`
- Read (do not modify): `cockpit/src/pages/Architecture.tsx`

**Interfaces:**
- Consumes: `renderApi` and the `main()` from Task 3.
- Produces: `node site/build-docs.mjs <out-dir>` additionally writes `<out-dir>/architecture.html` with each diagram's SVG inlined. Task 5 generates the SVGs with mermaid-cli before calling it.

- [ ] **Step 1: Extract the diagram sources**

Read `cockpit/src/pages/Architecture.tsx`. Copy the four template-literal constants into files, content unchanged except for the `classDef` lines:

- `OVERVIEW` (line 8) → `site/diagrams/overview.mmd`
- `ER` (line 36) → `site/diagrams/er.mmd`
- `SEED` (line 75) → `site/diagrams/seed.mmd`
- `RUNTIME` (line 102) → `site/diagrams/runtime.mmd`

The existing `classDef` colours are light-themed (e.g. `fill:#e7edf3,stroke:#3a4a5a,color:#16202b`) because the cockpit draws them on a light card. The site is dark. Rewrite each `classDef` against the site palette — panel-ish fills (`#151a20`, `#1a1e26`), `#2a2f38` strokes, `#d7dce3` text, with the `feed`/`core`/`exec` classes distinguished by stroke colour using `#25b083`, `#ce9a3b` and `#0e7490`. Keep the class names as they are so the node definitions still bind.

- [ ] **Step 2: Write `site/architecture.html`**

Hand-written, same `<head>` shape and CSS vocabulary as `site/index.html`. Carry over from `Architecture.tsx`:

- the page title and the intro prose (from `ArchitecturePage`, line 253 onward)
- the four diagrams' section headings
- the three-step `STEPS` list (line 179) — `make db-seed`, `make sync-broker BROKER=…`, `(nothing — feeds derive)` — with each step's body text
- the `CARDS` content (line 215)
- the closing note

Each diagram gets a placeholder the build script fills:

```html
<figure class="diagram" data-diagram="overview">
  <figcaption>How feeds, instruments and broker connections relate</figcaption>
</figure>
```

Use `data-diagram` values `overview`, `er`, `seed`, `runtime`, matching the `.mmd` filenames.

- [ ] **Step 3: Write the failing test**

Append to `site/build-docs.test.mjs`:

```js
import { inlineDiagrams } from "./build-docs.mjs";

test("inlineDiagrams puts each SVG inside its own placeholder", () => {
  const html = `<figure class="diagram" data-diagram="overview"></figure>
                <figure class="diagram" data-diagram="er"></figure>`;
  const svgs = { overview: "<svg id='a'></svg>", er: "<svg id='b'></svg>" };
  const out = inlineDiagrams(html, svgs);

  assert.match(out, /data-diagram="overview"[^]*?<svg id='a'>/);
  assert.match(out, /data-diagram="er"[^]*?<svg id='b'>/);
  assert.doesNotMatch(out, /data-diagram="overview"[^]*?<svg id='b'>[^]*?<\/figure>\s*<figure/);
});

test("inlineDiagrams fails loudly on a placeholder with no SVG", () => {
  // A silently empty figure would publish an architecture page with a hole in it.
  assert.throws(
    () => inlineDiagrams(`<figure class="diagram" data-diagram="runtime"></figure>`, {}),
    /runtime/,
  );
});
```

- [ ] **Step 4: Run tests to verify they fail**

Run: `node --test site/`
Expected: FAIL — `inlineDiagrams` is not exported.

- [ ] **Step 5: Implement**

In `site/build-docs.mjs`:

```js
/**
 * Replace each `<figure data-diagram="NAME">` placeholder's contents with that
 * diagram's SVG, keeping any <figcaption> already inside it.
 *
 * A missing SVG throws rather than leaving an empty figure: a hole in a published
 * page is worse than a failed build, and CI is where this should stop.
 */
export function inlineDiagrams(html, svgs) { /* … */ }
```

Extend `main()` to: read `site/architecture.html`, load every `*.svg` found in the SVG directory (Task 5 writes them; default the path to `<out-dir>/diagrams`), call `inlineDiagrams`, and write `architecture.html` to the out dir.

When the SVG directory does not exist, `main()` must fail with a message naming the mermaid-cli step — not skip the page.

- [ ] **Step 6: Run tests to verify they pass**

Run: `node --test site/`
Expected: PASS, 6 tests.

- [ ] **Step 7: Render it end to end locally**

Install mermaid-cli into a temp prefix rather than the repo (it is a CI-only dependency and must not enter `cockpit/package.json` or any committed manifest):

```bash
mkdir -p /tmp/mmdc && cd /tmp/mmdc && npm init -y >/dev/null && npm i @mermaid-js/mermaid-cli >/dev/null 2>&1
cd /Users/max/workarea/oms
mkdir -p /tmp/docs-out/diagrams
for f in site/diagrams/*.mmd; do
  n=$(basename "$f" .mmd)
  /tmp/mmdc/node_modules/.bin/mmdc -i "$f" -o "/tmp/docs-out/diagrams/$n.svg" -b transparent
done
ls -la /tmp/docs-out/diagrams/
node site/build-docs.mjs /tmp/docs-out
```

Expected: four SVGs, then `architecture.html` written.

Then check the diagrams actually landed and no mermaid runtime is referenced:

```bash
python3 -c "
h=open('/tmp/docs-out/architecture.html').read()
print('svg count:', h.count('<svg'))
print('empty figures:', h.count('data-diagram=\"overview\"></figure>'))
print('mermaid refs:', h.lower().count('mermaid'))
"
```

Expected: `svg count: 4`, `empty figures: 0`, `mermaid refs: 0`.

- [ ] **Step 8: Commit**

```bash
git add site/diagrams site/architecture.html site/build-docs.mjs site/build-docs.test.mjs
git commit -m "feat(site): static architecture page with inlined diagrams

The four mermaid sources move out of the cockpit page into site/diagrams as
ordinary files — they are the source of truth now, rendered to SVG in CI and
inlined here, so the published page runs no mermaid at all.

Their classDefs were light-themed for the cockpit's white card; rewritten
against the site palette. A placeholder with no SVG throws: a hole in a
published page is worse than a failed build."
```

---

### Task 5: Build the docs in the Pages workflow

**Files:**
- Modify: `.github/workflows/pages.yml`

**Interfaces:**
- Consumes: `site/build-docs.mjs` (Tasks 3-4), `docs/openapi.json` (Task 2), `site/diagrams/*.mmd` (Task 4).
- Produces: a published site carrying `api.html` and `architecture.html`.

- [ ] **Step 1: Extend the triggers**

In `.github/workflows/pages.yml`, add to the existing `paths:` list so a spec or diagram change republishes:

```yaml
      - "docs/openapi.json"
```

`site/**` already covers `site/diagrams/**` and the build script. Note the existing list also omits `cockpit/public/favicon.svg`, which the assemble step copies — add that too while you are here:

```yaml
      - "cockpit/public/favicon.svg"
```

- [ ] **Step 2: Add Node and the diagram render**

Insert after the `actions/checkout@v4` step and before `assemble the site`:

```yaml
      - uses: actions/setup-node@v4
        with:
          node-version: "22"

      # CI-only dependency: it pulls headless Chromium, which is why it is not in
      # any committed manifest and not a devDependency of the cockpit.
      - name: render the diagrams
        run: |
          npm i -g @mermaid-js/mermaid-cli
          mkdir -p _site/diagrams
          for f in site/diagrams/*.mmd; do
            n="$(basename "$f" .mmd)"
            mmdc -i "$f" -o "_site/diagrams/$n.svg" -b transparent
          done
          ls -la _site/diagrams

      - name: build the docs pages
        run: node site/build-docs.mjs _site
```

- [ ] **Step 3: Extend the assemble step**

`build-docs.mjs` writes into `_site`, and the existing `assemble the site` step also writes there. Verified: that step only does `mkdir -p _site/assets` and a series of `cp` — no `rm -rf` — so it cannot clobber generated files. Place the two new steps from Step 2 **after** the assemble step regardless, so the ordering is explicit rather than incidental.

The generated `diagrams/*.svg` are intermediate — they were inlined into the HTML and do not need publishing. Remove them at the end of the docs-build step:

```yaml
      - name: drop the intermediate SVGs
        run: rm -rf _site/diagrams
```

- [ ] **Step 4: Verify the workflow**

Run: `actionlint .github/workflows/pages.yml`
Expected: no output.

- [ ] **Step 5: Rehearse the whole job locally**

Run the same sequence the workflow does, in order, from the repo root:

```bash
rm -rf /tmp/pages-rehearsal && mkdir -p /tmp/pages-rehearsal/assets
cp site/index.html site/style.css /tmp/pages-rehearsal/
cp install.sh /tmp/pages-rehearsal/install.sh
cp assets/screenshot01.png assets/screenshot02.png /tmp/pages-rehearsal/assets/
cp cockpit/public/favicon.svg /tmp/pages-rehearsal/favicon.svg
mkdir -p /tmp/pages-rehearsal/diagrams
for f in site/diagrams/*.mmd; do
  n=$(basename "$f" .mmd)
  /tmp/mmdc/node_modules/.bin/mmdc -i "$f" -o "/tmp/pages-rehearsal/diagrams/$n.svg" -b transparent
done
node site/build-docs.mjs /tmp/pages-rehearsal
rm -rf /tmp/pages-rehearsal/diagrams
(cd /tmp/pages-rehearsal && python3 -m http.server 8479 &)
sleep 2
for p in / /api.html /architecture.html /style.css /install.sh; do
  printf '%s -> ' "$p"; curl -sf -o /dev/null -w '%{http_code}\n' "localhost:8479$p"
done
pkill -f "http.server 8479"
```

Expected: every path returns `200`. Put the real output in your report. Kill the server; leave no background process.

- [ ] **Step 6: Commit**

```bash
git add .github/workflows/pages.yml
git commit -m "ci(pages): render the docs pages during deploy

mermaid-cli runs here rather than at authoring time so the .mmd files stay
the source of truth; it drags headless Chromium in, which is the reason the
Pages job is now minutes rather than seconds.

The intermediate SVGs are inlined into the HTML and then deleted — nothing
needs them at runtime."
```

---

### Task 6: Remove the docs from the cockpit

Do this only after Tasks 3 and 4 have ported their content — these files are the source material.

**Files:**
- Delete: `cockpit/src/pages/Architecture.tsx`, `cockpit/src/pages/ApiDocs.tsx`
- Modify: `cockpit/src/App.tsx`, `cockpit/package.json`, `cockpit/package-lock.json`

**Interfaces:**
- Consumes: nothing.
- Produces: a cockpit with no docs pages and no `mermaid` dependency.

- [ ] **Step 1: Record the bundle size before**

```bash
cd cockpit && npm run build >/dev/null 2>&1 && du -sh dist && ls -la dist/assets/*.js | awk '{print $5, $9}' | sort -rn | head -5
```

Save this output — Step 6 compares against it.

- [ ] **Step 2: Delete the pages and their wiring**

```bash
git rm cockpit/src/pages/Architecture.tsx cockpit/src/pages/ApiDocs.tsx
```

In `cockpit/src/App.tsx`:
- Remove the two `lazy(...)` declarations at lines 19-22 (`ApiDocsPage`, `ArchitecturePage`).
- Remove the `DOCS_NAV` array (line 37) and the block that renders it in the navbar (line 83).
- Remove the two `<Route>` blocks — `/docs/architecture` (lines 106-113) and `/api-docs` (lines 114-121).
- Replace the navbar's Docs group with a single external link:

```tsx
// The docs live on the public site now — they are what someone reads *before*
// installing, so they do not belong behind a login.
<Anchor
  href="https://maxkuttner.github.io/openoms/architecture.html"
  target="_blank"
  rel="noreferrer"
  fz="sm"
>
  Docs ↗
</Anchor>
```

Match the surrounding navbar's existing markup and spacing conventions rather than pasting this verbatim if they differ.
- Remove the `lazy` and `Suspense` imports (line 1) — the two deleted routes were their only users. **Keep `Loader`** (line 2): it is still used at line 162, outside the routes. Verify with `grep -n "Suspense\|Loader\|lazy" cockpit/src/App.tsx` after editing; the only remaining hit should be the line-162 `<Loader />` and its import.

- [ ] **Step 3: Drop mermaid**

Remove `"mermaid": "^11.16.0"` from `cockpit/package.json` dependencies, then:

```bash
cd cockpit && npm install
```

No flags. `Architecture.tsx` was its only importer — verify nothing else pulls it in:

Run: `grep -rn "mermaid" cockpit/src/ || echo "no importers left"`
Expected: `no importers left`

- [ ] **Step 4: Typecheck and build**

Run: `cd cockpit && npx tsc --noEmit`
Expected: no output. Any "declared but never used" error here means a leftover import from Step 2 — fix it.

Run: `cd cockpit && npm run build`
Expected: exit 0.

- [ ] **Step 5: Confirm the cockpit still works**

Run: `./cockpit/check-api-base.sh`
Expected: `ok: bundle calls the OMS same-origin`

Run: `cargo test cockpit::`
Expected: PASS — the embed tests still find a populated bundle.

- [ ] **Step 6: Record the bundle size after**

```bash
cd cockpit && du -sh dist && ls -la dist/assets/*.js | awk '{print $5, $9}' | sort -rn | head -5
```

Compare with Step 1 and put both in your report. Dropping mermaid should be clearly visible — roughly a megabyte. **If it is not, stop and say so**: something else is pulling mermaid in, and the dependency removal did not do what it claims.

- [ ] **Step 7: Commit**

```bash
git add -A cockpit/
git commit -m "refactor(cockpit): drop the docs pages, they live on the site now

The architecture page and the API reference sat behind an installed binary,
a provisioned database and a login — which is precisely backwards for the
two things someone reads before installing any of that. The navbar keeps one
link out to the published versions.

mermaid goes with them: ~1MB of JavaScript that every release binary was
carrying for a page no operator opens twice."
```

---

### Task 7: Link the docs from the landing page

**Files:**
- Modify: `site/index.html`

**Interfaces:**
- Consumes: `api.html` and `architecture.html` from Tasks 3-4.
- Produces: the finished site.

- [ ] **Step 1: Add a Docs section**

In `site/index.html`, add a section between "What you get" and "The cockpit":

```html
      <section>
        <h2>Docs</h2>
        <ul>
          <li><a href="architecture.html">Architecture</a> — how feeds, instruments and broker connections fit together.</li>
          <li><a href="api.html">API reference</a> — every endpoint, generated from the OMS's own OpenAPI spec.</li>
        </ul>
      </section>
```

- [ ] **Step 2: Add them to the footer**

Replace the existing `Docs` footer link (which points at the GitHub readme) so the footer reads:

```html
    <footer>
      <a href="https://github.com/maxkuttner/openoms">GitHub</a>
      <a href="architecture.html">Architecture</a>
      <a href="api.html">API reference</a>
      <a href="https://github.com/maxkuttner/openoms#readme">Readme</a>
      <a href="https://github.com/maxkuttner/openoms/tree/main/clients/python">Python client</a>
    </footer>
```

- [ ] **Step 3: Verify the links resolve**

Rebuild the site into a temp dir and check every internal link returns 200:

```bash
rm -rf /tmp/link-check && mkdir -p /tmp/link-check/assets /tmp/link-check/diagrams
cp site/index.html site/style.css /tmp/link-check/
cp install.sh /tmp/link-check/
cp assets/screenshot01.png assets/screenshot02.png /tmp/link-check/assets/
cp cockpit/public/favicon.svg /tmp/link-check/favicon.svg
for f in site/diagrams/*.mmd; do
  n=$(basename "$f" .mmd)
  /tmp/mmdc/node_modules/.bin/mmdc -i "$f" -o "/tmp/link-check/diagrams/$n.svg" -b transparent
done
node site/build-docs.mjs /tmp/link-check
rm -rf /tmp/link-check/diagrams
(cd /tmp/link-check && python3 -m http.server 8480 &)
sleep 2
for p in / /architecture.html /api.html /style.css /favicon.svg; do
  printf '%s -> ' "$p"; curl -sf -o /dev/null -w '%{http_code}\n' "localhost:8480$p"
done
pkill -f "http.server 8480"
```

Expected: all `200`. Kill the server.

- [ ] **Step 4: Commit**

```bash
git add site/index.html
git commit -m "feat(site): link the architecture and API docs from the landing page"
```

---

## Self-Review

**Spec coverage.** §1 `oms openapi` → Task 1. §2 committed spec + drift test → Task 2, including a step that proves the drift test can actually fail. §3 docs build: the API reference → Task 3; the diagrams and architecture page → Task 4; mermaid-cli in CI → Task 5. §4 cockpit removal (both pages, routes, `DOCS_NAV`, mermaid, the `apiTheme.tsx` and `Suspense`/`Loader` caveats, the bundle-size check) → Task 6. §5 landing page links and the `paths:` trigger → Task 7 and Task 5 Step 1 respectively. The spec's note that `pages.yml` omits `cockpit/public/favicon.svg` — a deferred minor from the previous plan — is folded into Task 5 Step 1.

**Ordering.** Tasks 3 and 4 read `ApiDocs.tsx` and `Architecture.tsx`; Task 6 deletes them. The dependency is stated in the Global Constraints and repeated at the head of Task 6.

**Type consistency.** `renderApi(spec)`, `inlineDiagrams(html, svgs)`, `escapeHtml(s)`, `typeLabel(spec, s)`, `curlFor(spec, method, path, op)` and `respColor(code)` are named identically in the tests that consume them (Tasks 3-4) and in the implementation steps that define them. `openapi_json()` is defined in Task 1 and used by Task 2's drift test. The `data-diagram` values (`overview`, `er`, `seed`, `runtime`) match the `.mmd` filenames in Task 4 and the mermaid-cli loop in Task 5.

**Known gap.** Task 3's port is described rather than reproduced, because the source is a 326-line file the implementer must read anyway and transcribing it here would be a second copy to drift. The tests pin the behaviour that matters: `$ref` resolution, curl generation, HTML escaping, and no external references in the output.
