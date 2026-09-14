# Installer and Landing Page Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Install openoms with `curl -fsSL https://maxkuttner.github.io/openoms/install.sh | sh`, then two commands to a running OMS with a working web UI.

**Architecture:** A tag-triggered GitHub Actions workflow builds the `oms` binary on native macOS-arm64 and Linux-x86_64 runners, with the cockpit SPA compiled into the binary via `include_dir!`, and publishes checksummed tarballs to a GitHub Release. A POSIX `install.sh` at the repo root downloads, verifies and installs that binary into `~/.local/bin` and stops there. A static page on GitHub Pages carries the instructions and serves `install.sh` from its own origin.

**Tech Stack:** Rust (axum 0.7, `include_dir`, `mime_guess`, clap 4), Vite 8 + React 18 (cockpit), GitHub Actions, POSIX `sh`, hand-written HTML/CSS.

**Spec:** `docs/superpowers/specs/2026-09-14-install-landing-page-design.md`

## Global Constraints

- **Repo:** `maxkuttner/openoms`. **Binary name:** `oms`. **First tag:** `v0.1.0` (must equal `version` in `Cargo.toml`).
- **Targets, exactly two:** `aarch64-apple-darwin` on runner `macos-14`, `x86_64-unknown-linux-gnu` on runner `ubuntu-22.04`.
- **Never use `ubuntu-latest`** in any workflow added by this plan. It resolves to 24.04, whose glibc 2.39 produces a binary that will not start on Debian 12 / Ubuntu 22.04. Pin `ubuntu-22.04` everywhere.
- **Artifact names are a permanent contract:** `oms-<target>.tar.gz`, plus one `SHA256SUMS` per release. Installed copies resolve upgrades by these names; renaming them later breaks every existing install.
- **Node 22** in every workflow that builds the cockpit (Vite 8 requires Node 20.19+ / 22.12+).
- **The installer never uses sudo**, never writes outside `$OMS_INSTALL_DIR` (default `$HOME/.local/bin`), never starts Postgres, never writes `oms.toml`, never launches the server.
- **A checksum mismatch is always fatal.** Missing checksum tooling is also fatal, never a silent skip.
- **Colors (landing page):** background `#0d1014`, accent `#25b083` — taken from `cockpit/public/favicon.svg`.
- **No LICENSE file exists in this repo.** Do not invent one and do not add a license link to the page footer. If a `LICENSE` file has been added by the time Task 9 runs, link it; otherwise omit that line.

---

### Task 1: `oms --version`

The installer's smoke check and the release workflow's tag invariant both shell out to `oms --version`. It does not exist yet — `target/debug/oms --version` currently prints `error: unexpected argument '--version' found`. clap can supply it from the crate version with a single token.

**Files:**
- Modify: `src/main.rs:293`

**Interfaces:**
- Consumes: nothing.
- Produces: `oms --version` prints `oms <Cargo.toml version>` on stdout and exits 0. Tasks 6, 7 and 8 parse the last whitespace-separated field of that line as the version.

- [ ] **Step 1: Write the failing test**

The CLI is a clap derive, so the behaviour under test is clap's generated `--version`. Assert it through the public parser rather than by spawning a process. Add to the existing `mod tests` block at the bottom of `src/main.rs` (starts at `src/main.rs:1199`):

```rust
    #[test]
    fn cli_reports_its_version() {
        use clap::CommandFactory;
        let version = <super::Cli as CommandFactory>::command()
            .get_version()
            .expect("--version must be available: install.sh smoke-checks it");
        assert_eq!(version, env!("CARGO_PKG_VERSION"));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test cli_reports_its_version`
Expected: FAIL — panics on `expect` with "`--version` must be available", because `#[command(...)]` does not declare `version`.

- [ ] **Step 3: Write minimal implementation**

`src/main.rs:293`, add the `version` token:

```rust
#[command(name = "oms", version, about = "OMS server + setup CLI")]
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test cli_reports_its_version`
Expected: PASS

Then confirm the real binary agrees:

Run: `cargo run --quiet -- --version`
Expected: `oms 0.1.0`

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat(cli): add oms --version

The installer smoke-checks the binary it just wrote by running it, and the
release workflow asserts the tag matches what the binary reports. Both need
a --version flag; clap generates one from the crate version."
```

---

### Task 2: Cockpit talks to the OMS without the dev proxy

`cockpit/src/api/client.ts:25` prefixes every request with `/api`, which only resolves through the Vite dev proxy (`cockpit/vite.config.ts`). Once the bundle is served by the OMS itself it is same-origin and must call `/admin/*` and `/health` directly. Three call sites.

`cockpit/vite.config.ts` (`base: "/cockpit/"` on build) and `cockpit/src/main.tsx:17` (router basename from `import.meta.env.BASE_URL`) are already correct and must not be touched.

**Files:**
- Modify: `cockpit/src/api/client.ts` (add `API_BASE`, use it at line 25)
- Modify: `cockpit/src/App.tsx:45`
- Modify: `cockpit/src/pages/ApiDocs.tsx:241`

**Interfaces:**
- Consumes: nothing.
- Produces: `export const API_BASE: string` from `cockpit/src/api/client.ts` — `"/api"` under `vite dev`, `""` in a production bundle. Task 4 relies on the production bundle issuing same-origin requests to `/health` and `/admin/*`.

- [ ] **Step 1: Write the failing check**

The cockpit has no test runner (no `vitest`, no `test` script in `cockpit/package.json`), and adding one is out of scope for this plan. The check is therefore an assertion over the built bundle, which is exactly the artifact that ships. Create `cockpit/check-api-base.sh`:

```sh
#!/bin/sh
# The bundled cockpit is served same-origin by the OMS, so a built bundle must not
# contain the "/api" dev-proxy prefix. Run after `npm run build`.
set -eu
cd "$(dirname "$0")"
[ -d dist ] || { echo "no dist/ — run npm run build first" >&2; exit 1; }

if grep -rq '"/api/' dist/assets; then
  echo "built bundle still calls the /api dev proxy:" >&2
  grep -ro '"/api/[a-z-]*' dist/assets | sort -u >&2
  exit 1
fi

grep -rq '"/health"' dist/assets \
  || { echo "built bundle does not call /health — API_BASE wiring is wrong" >&2; exit 1; }

echo "ok: bundle calls the OMS same-origin"
```

Make it executable: `chmod +x cockpit/check-api-base.sh`

- [ ] **Step 2: Run it to verify it fails**

Run: `cd cockpit && npm ci && npm run build && ./check-api-base.sh`
Expected: FAIL — prints `built bundle still calls the /api dev proxy:` followed by `"/api/health` and `"/api/api-docs`.

- [ ] **Step 3: Write minimal implementation**

In `cockpit/src/api/client.ts`, add the export below the existing header comment and change the `fetch` call:

```ts
// Dev runs behind Vite's proxy, which strips this prefix (see vite.config.ts).
// A production bundle is served by the OMS itself at /cockpit/, so it is
// same-origin and calls the API with no prefix at all.
export const API_BASE = import.meta.env.DEV ? "/api" : "";
```

```ts
  const res = await fetch(`${API_BASE}${path}`, {
```

In `cockpit/src/App.tsx`, import it and use it at line 45:

```ts
import { API_BASE } from "./api/client";
```

```ts
    queryFn: async () => (await fetch(`${API_BASE}/health`)).ok,
```

In `cockpit/src/pages/ApiDocs.tsx`, same import and at line 241:

```ts
      const r = await fetch(`${API_BASE}/api-docs/openapi.json`);
```

If `ApiDocs.tsx` already imports from `../api/client`, add `API_BASE` to that import rather than adding a second one.

- [ ] **Step 4: Run it to verify it passes**

Run: `cd cockpit && npm run build && ./check-api-base.sh`
Expected: `ok: bundle calls the OMS same-origin`

Run: `cd cockpit && npx tsc --noEmit`
Expected: no output (the `build` script already runs `tsc`, this is the isolated check)

- [ ] **Step 5: Commit**

```bash
git add cockpit/src/api/client.ts cockpit/src/App.tsx cockpit/src/pages/ApiDocs.tsx cockpit/check-api-base.sh
git commit -m "feat(cockpit): call the OMS same-origin in production builds

The /api prefix only exists to give Vite's dev proxy something to strip. A
bundled cockpit is served by the OMS at /cockpit/ and shares its origin, so
the prefix would 404 there. API_BASE keeps dev on the proxy and production
on the real paths."
```

---

### Task 3: Embed the cockpit bundle in the binary

**Files:**
- Create: `src/cockpit.rs`
- Modify: `build.rs` (create `cockpit/dist` when absent, before the existing macOS-only early return)
- Modify: `Cargo.toml` (add `mime_guess`)

**Interfaces:**
- Consumes: `cockpit/dist`, produced by `npm run build` in Task 2.
- Produces, from `crate::cockpit`:
  - `pub fn is_bundled() -> bool`
  - `pub struct Asset { pub body: &'static [u8], pub content_type: String, pub cache_control: &'static str }`
  - `pub fn asset_for(path: &str) -> Option<Asset>` — `path` is relative to `/cockpit/`, with or without a leading slash; `""` means the shell
  - `pub fn respond(path: &str) -> axum::response::Response`
  - Task 4 adds `pub fn router() -> Router<AppState>` to this same file.

- [ ] **Step 1: Add the dependency and the build-script guard**

`cockpit/dist` is gitignored (`cockpit/.gitignore:2`), so a fresh clone does not have it, and `include_dir!` on a missing directory is a **compile error** — the crate would not build at all. Do this first or nothing in this task compiles.

In `Cargo.toml`, below the existing `include_dir` line:

```toml
# Content types for the embedded cockpit bundle
mime_guess = "2"
```

In `build.rs`, call a new function as the very first statement of `main()` — it must run *before* the existing `if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") { return; }`, or Linux builds skip it:

```rust
fn main() {
    ensure_cockpit_dist();

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    // ... existing OpenSSL search, unchanged
}

/// `cockpit/dist` is a build output and is gitignored, but `include_dir!` on a
/// missing directory fails to compile — so a fresh clone would not build until
/// someone ran npm. Create it empty instead. An empty embed is the normal state of
/// a source build and is handled, not an error (see `src/cockpit.rs`).
fn ensure_cockpit_dist() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let dist = Path::new(&manifest).join("cockpit").join("dist");
    std::fs::create_dir_all(&dist).expect("create cockpit/dist");
    println!("cargo:rerun-if-changed=cockpit/dist");
}
```

- [ ] **Step 2: Write the failing tests**

Create `src/cockpit.rs` containing only the test module. The tests reference an API that does not exist yet, so the failure signal in Step 3 is a compile error — that is expected and is the point.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // Whether the embed has anything in it depends on whether `npm run build` ran
    // before `cargo build`. Both are legitimate: a contributor's source build has an
    // empty embed, CI and every release build a populated one. So each test asserts
    // the behaviour of the build it is actually running in, and CI runs both.

    #[test]
    fn a_source_build_says_where_the_dev_ui_is() {
        if is_bundled() {
            return;
        }
        assert!(asset_for("").is_none());
        let res = respond("");
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn a_bundled_build_serves_the_shell() {
        if !is_bundled() {
            return;
        }
        let shell = asset_for("").expect("index.html");
        assert!(shell.content_type.starts_with("text/html"));
        assert_eq!(shell.cache_control, NO_CACHE);
        assert_eq!(respond("").status(), StatusCode::OK);
    }

    #[test]
    fn deep_links_fall_back_to_the_shell() {
        if !is_bundled() {
            return;
        }
        let shell = asset_for("").unwrap();
        let deep = asset_for("orders").expect("client-side route must serve the shell");
        assert_eq!(shell.body, deep.body);
    }

    #[test]
    fn a_missing_asset_stays_missing() {
        if !is_bundled() {
            return;
        }
        // Has an extension, so it is a real file request, not a client-side route.
        assert!(asset_for("assets/nope-00000000.js").is_none());
        assert_eq!(respond("assets/nope-00000000.js").status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn hashed_assets_are_cached_forever() {
        if !is_bundled() {
            return;
        }
        let first = DIST
            .get_dir("assets")
            .expect("vite emits an assets/ dir")
            .files()
            .next()
            .expect("at least one hashed asset");
        let name = first.path().to_string_lossy().to_string();
        assert_eq!(asset_for(&name).unwrap().cache_control, IMMUTABLE);
    }
}
```

Declare the module so it is compiled. In `src/main.rs`, add alongside the other `mod` lines (after `mod reload;` at line 68):

```rust
mod cockpit;
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test cockpit::`
Expected: FAIL to compile — `cannot find function 'is_bundled' in this scope`, and the same for `asset_for`, `respond`, `NO_CACHE`, `IMMUTABLE`, `DIST`, `StatusCode`.

- [ ] **Step 4: Write the implementation**

Prepend to `src/cockpit.rs`, above the test module:

```rust
//! The cockpit single-page app, compiled into the binary.
//!
//! `include_dir!` rather than serving a directory off disk, so a released `oms`
//! serves its own UI from wherever it was installed — no checkout, no npm, no
//! second process. Mirrors how `setup/database/assets.rs` carries the migrations.
//!
//! Unlike the migrations embed, an *empty* embed here is legitimate: it is what
//! every source build produces, because `cockpit/dist` is a gitignored build
//! output. So there is no release assert — the routes explain where the dev UI is.

use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use include_dir::{include_dir, Dir};

static DIST: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/cockpit/dist");

/// Vite emits content-hashed asset filenames, so a given URL's bytes never change.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
/// `index.html` has a stable URL and changing contents — caching it hides upgrades.
const NO_CACHE: &str = "no-cache";

const NOT_BUNDLED: &str =
    "cockpit not bundled in this build — run `cd cockpit && npm run dev` for the dev UI on :5173\n";

pub struct Asset {
    pub body: &'static [u8],
    pub content_type: String,
    pub cache_control: &'static str,
}

/// Did this build embed a cockpit bundle?
pub fn is_bundled() -> bool {
    DIST.get_file("index.html").is_some()
}

/// Resolve a path *relative to* `/cockpit/` to an embedded file.
///
/// A miss on a path with no file extension is a client-side route such as
/// `/cockpit/orders`: only the SPA shell can answer it, so the shell is what a
/// reload of that URL must return. A miss on a path *with* an extension is a
/// genuinely absent file and stays a 404 — serving HTML there would hand the
/// browser an index.html labelled `application/javascript`.
pub fn asset_for(path: &str) -> Option<Asset> {
    let rel = path.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };

    if let Some(file) = DIST.get_file(rel) {
        let cache_control = if rel.starts_with("assets/") { IMMUTABLE } else { NO_CACHE };
        return Some(Asset {
            body: file.contents(),
            content_type: mime_guess::from_path(rel).first_or_octet_stream().to_string(),
            cache_control,
        });
    }

    if std::path::Path::new(rel).extension().is_none() {
        let shell = DIST.get_file("index.html")?;
        return Some(Asset {
            body: shell.contents(),
            content_type: "text/html".to_string(),
            cache_control: NO_CACHE,
        });
    }

    None
}

pub fn respond(path: &str) -> Response {
    match asset_for(path) {
        Some(asset) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, asset.content_type),
                (header::CACHE_CONTROL, asset.cache_control.to_string()),
            ],
            asset.body,
        )
            .into_response(),
        None if !is_bundled() => (StatusCode::NOT_FOUND, NOT_BUNDLED).into_response(),
        None => (StatusCode::NOT_FOUND, "not found\n").into_response(),
    }
}
```

- [ ] **Step 5: Run tests to verify they pass — both ways**

First as a source build (empty embed). Wipe any local bundle so the state is the one a fresh clone has:

Run: `rm -rf cockpit/dist && cargo test cockpit::`
Expected: PASS, 5 tests. Only `a_source_build_says_where_the_dev_ui_is` does real work; the rest return early.

Then as a bundled build:

Run: `(cd cockpit && npm run build) && cargo test cockpit::`
Expected: PASS, 5 tests, with the four bundled-path tests now doing real work.

Confirm the second run genuinely exercised them (the early returns make a green run uninformative on its own):

Run: `(cd cockpit && npm run build) && cargo test cockpit::a_bundled_build_serves_the_shell -- --exact --nocapture`
Expected: PASS. Then temporarily break it — change `NO_CACHE` to `"public"` in the assertion, re-run, confirm FAIL, and revert.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock build.rs src/cockpit.rs src/main.rs
git commit -m "feat(cockpit): compile the cockpit bundle into the binary

include_dir over cockpit/dist, the same mechanism the migrations already use,
so a released oms serves its own UI with no checkout and no npm.

cockpit/dist is a gitignored build output and include_dir! fails to compile on
a missing directory, so build.rs creates it empty. An empty embed is the normal
state of a source build: the routes 404 with a pointer at npm run dev rather
than asserting, which is why this differs from the migrations embed."
```

---

### Task 4: Serve the cockpit at `/cockpit/`

**Files:**
- Modify: `src/cockpit.rs` (add `router()` and its tests)
- Modify: `src/main.rs:1178-1191` (merge the router, log the URL)
- Modify: `readme.md` (note the bundled UI)

**Interfaces:**
- Consumes: `crate::cockpit::{respond, is_bundled}` from Task 3; `crate::app_state::AppState` (the app's state type — handlers take `State<AppState>`, see `src/handlers.rs:319`).
- Produces: `pub fn router() -> axum::Router<AppState>` serving `/cockpit`, `/cockpit/` and `/cockpit/*path`. Task 9's landing page sends visitors to `http://localhost:3001/cockpit/`.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `src/cockpit.rs`:

```rust
    #[tokio::test]
    async fn the_bare_path_redirects_to_the_trailing_slash() {
        // The SPA's asset URLs are relative to /cockpit/ (vite `base`), so without
        // the trailing slash the browser resolves them one level too high.
        let res = redirect_to_slash().await.into_response();
        assert_eq!(res.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(res.headers()[header::LOCATION], "/cockpit/");
    }

    #[tokio::test]
    async fn the_index_route_serves_the_shell_or_explains_itself() {
        let res = index().await;
        let expected = if is_bundled() { StatusCode::OK } else { StatusCode::NOT_FOUND };
        assert_eq!(res.status(), expected);
    }

    #[tokio::test]
    async fn the_wildcard_route_resolves_a_client_side_route() {
        let res = asset(axum::extract::Path("orders".to_string())).await;
        let expected = if is_bundled() { StatusCode::OK } else { StatusCode::NOT_FOUND };
        assert_eq!(res.status(), expected);
    }
```

These call the handlers directly rather than driving a `Router`: `tower`'s `util` feature (which `ServiceExt::oneshot` needs) is not enabled on this crate's `tower = "0.4"`, and turning it on to test three static routes is not worth a dependency change.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test cockpit::`
Expected: FAIL to compile — `cannot find function 'redirect_to_slash'`, `'index'`, `'asset'`.

- [ ] **Step 3: Write the implementation**

In `src/cockpit.rs`, extend the imports and append the routing layer above the test module:

```rust
use axum::{
    extract::Path as UrlPath,
    response::Redirect,
    routing::get,
    Router,
};

use crate::app_state::AppState;
```

```rust
async fn redirect_to_slash() -> Redirect {
    Redirect::temporary("/cockpit/")
}

async fn index() -> Response {
    respond("")
}

async fn asset(UrlPath(path): UrlPath<String>) -> Response {
    respond(&path)
}

/// Mounted outside the admin auth layer on purpose: the SPA shell has to load
/// before there is a token to send, and these routes carry no data — only the
/// static bundle. Authentication happens where it already did, on `/admin/*`.
pub fn router() -> Router<AppState> {
    Router::new()
        // axum's `*path` wildcard needs at least one character after the slash, so
        // `/cockpit/` itself gets its own route.
        .route("/cockpit", get(redirect_to_slash))
        .route("/cockpit/", get(index))
        .route("/cockpit/*path", get(asset))
}
```

In the test module, the redirect test needs `IntoResponse` for `Redirect`, already imported via `super::*`.

In `src/main.rs`, merge it into the app (the `let app = Router::new()` block at line 1178) directly after `.merge(admin_router)`:

```rust
        .merge(admin_router)
        .merge(cockpit::router())
```

And log the URL after the existing listening line (around `src/main.rs:1192`):

```rust
    info!("OMS listening on {}", host_url);
    if cockpit::is_bundled() {
        info!("Cockpit: {}/cockpit/", host_url);
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `rm -rf cockpit/dist && cargo test cockpit::`
Expected: PASS (8 tests)

Run: `(cd cockpit && npm run build) && cargo test cockpit::`
Expected: PASS (8 tests)

- [ ] **Step 5: Verify it end to end against a running server**

Requires a provisioned database; if `oms.toml` is absent, run `docker compose up -d` then `cargo run -- database init` first.

Run, in one terminal: `(cd cockpit && npm run build) && cargo run`
Expected: the startup log includes `Cockpit: http://localhost:3001/cockpit/`

In another terminal:

```bash
curl -si localhost:3001/cockpit | head -3          # 307, location: /cockpit/
curl -s  localhost:3001/cockpit/ | head -5          # the index.html shell
curl -si localhost:3001/cockpit/orders | head -3    # 200 text/html — deep link
curl -si localhost:3001/cockpit/assets/nope.js | head -2  # 404
```

Then open `http://localhost:3001/cockpit/` in a browser and confirm the UI loads, the connection dot goes green (that is `/health` resolving same-origin — the Task 2 change), and the API docs page renders.

- [ ] **Step 6: Document it**

In `readme.md`, in the Setup section after the existing cockpit instructions, add:

```markdown
A released `oms` binary serves the cockpit itself at
<http://localhost:3001/cockpit/> — the bundle is compiled in, so there is no second
process to start. The `npm run dev` server above is for developing the cockpit: it
hot-reloads and proxies the API to a running OMS. A source build embeds nothing
unless you run `npm run build` in `cockpit/` before `cargo build`; until then
`/cockpit/` returns a 404 that says so.
```

- [ ] **Step 7: Commit**

```bash
git add src/cockpit.rs src/main.rs readme.md
git commit -m "feat(cockpit): serve the embedded UI at /cockpit/

Three routes: /cockpit redirects to the trailing slash the SPA's relative asset
URLs need, /cockpit/ serves the shell, and the wildcard serves assets with a
fallback to the shell for extensionless paths so client-side routes survive a
reload.

Merged outside admin_middleware: the shell must load before the login gate can
ask for a token, and these routes serve only the static bundle."
```

---

### Task 5: Cover the embedded build in CI

Without this, the populated-embed path is only ever exercised on a release, which is the worst moment to find out it broke.

**Files:**
- Modify: `.github/workflows/build.yml`

**Interfaces:**
- Consumes: `cockpit/check-api-base.sh` (Task 2), the `cockpit::` tests (Tasks 3 and 4).
- Produces: nothing other tasks consume.

- [ ] **Step 1: Add the job**

Append to `.github/workflows/build.yml`, after the existing `database` job (keep the existing `build` job's `ubuntu-latest` as it is — this plan's pin applies to jobs that produce or test distributed binaries):

```yaml
  cockpit:
    name: cockpit bundle embeds and serves
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4

      - uses: actions/setup-node@v4
        with:
          node-version: "22"
          cache: npm
          cache-dependency-path: cockpit/package-lock.json

      - name: build the bundle
        working-directory: cockpit
        run: npm ci && npm run build

      - name: the bundle calls the OMS same-origin
        run: ./cockpit/check-api-base.sh

      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2

      - name: the embed is populated and served
        run: cargo test cockpit:: -- --nocapture

      - name: and the embed is genuinely non-empty
        run: |
          test -s cockpit/dist/index.html \
            || { echo "cockpit/dist/index.html missing — the embed tests all no-op'd"; exit 1; }
```

The last step matters: every bundled-path test returns early when the embed is empty, so a green `cargo test` proves nothing on its own.

- [ ] **Step 2: Verify the workflow is valid**

Run: `actionlint .github/workflows/build.yml`
Expected: no output. If `actionlint` is missing: `brew install actionlint`.

- [ ] **Step 3: Verify the job's commands work locally**

Run: `(cd cockpit && npm ci && npm run build) && ./cockpit/check-api-base.sh && cargo test cockpit:: && test -s cockpit/dist/index.html && echo ok`
Expected: ends with `ok`

- [ ] **Step 4: Commit and confirm CI is green**

```bash
git add .github/workflows/build.yml
git commit -m "ci: exercise the cockpit embed on every PR

Every bundled-path test returns early when the embed is empty, so the job also
asserts dist/index.html exists — otherwise a green run would prove only that
nothing ran."
git push
```

Run: `gh run watch`
Expected: all jobs pass, including `cockpit bundle embeds and serves`.

---

### Task 6: Release workflow

**Files:**
- Create: `.github/workflows/release.yml`

**Interfaces:**
- Consumes: `oms --version` (Task 1), `cockpit/dist` build (Task 2), the embed (Tasks 3-4).
- Produces: a GitHub Release per `v*` tag carrying exactly `oms-aarch64-apple-darwin.tar.gz`, `oms-x86_64-unknown-linux-gnu.tar.gz` and `SHA256SUMS`. Tasks 7 and 8 download these by name.

- [ ] **Step 1: Write the workflow**

Create `.github/workflows/release.yml`:

```yaml
name: Release

on:
  push:
    tags: ["v*"]

permissions:
  contents: write

jobs:
  build:
    name: ${{ matrix.target }}
    strategy:
      fail-fast: false
      matrix:
        include:
          - runner: macos-14
            target: aarch64-apple-darwin
          # 22.04, not ubuntu-latest: 24.04 links glibc 2.39 and the binary then
          # refuses to start on Debian 12 or Ubuntu 22.04, which is most servers.
          - runner: ubuntu-22.04
            target: x86_64-unknown-linux-gnu
    runs-on: ${{ matrix.runner }}
    steps:
      - uses: actions/checkout@v4

      - name: the tag must match Cargo.toml
        run: |
          tag="${GITHUB_REF_NAME#v}"
          crate="$(sed -n '/^\[package\]/,/^\[/p' Cargo.toml \
                   | sed -n 's/^version *= *"\(.*\)"/\1/p' | head -1)"
          echo "tag=$tag cargo=$crate"
          [ "$tag" = "$crate" ] || {
            echo "tag $GITHUB_REF_NAME does not match Cargo.toml version $crate"
            exit 1
          }

      - uses: actions/setup-node@v4
        with:
          node-version: "22"
          cache: npm
          cache-dependency-path: cockpit/package-lock.json

      - name: build the cockpit bundle
        working-directory: cockpit
        run: npm ci && npm run build

      - name: the bundle must exist before cargo embeds it
        run: test -s cockpit/dist/index.html

      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2

      # build.rs finds a Homebrew OpenSSL for the quickfix SSL link. cmake is
      # preinstalled on both runner images.
      - name: openssl
        if: runner.os == 'macOS'
        run: brew install openssl@3

      - run: cargo build --release

      - run: strip target/release/oms

      - name: the binary reports the tag's version
        run: |
          got="$(./target/release/oms --version | awk '{print $NF}')"
          [ "$got" = "${GITHUB_REF_NAME#v}" ] || {
            echo "binary reports $got, tag is $GITHUB_REF_NAME"; exit 1; }

      - name: package
        run: |
          tar czf "oms-${{ matrix.target }}.tar.gz" -C target/release oms
          if command -v sha256sum >/dev/null 2>&1; then
            sha256sum "oms-${{ matrix.target }}.tar.gz" > "oms-${{ matrix.target }}.sha256"
          else
            shasum -a 256 "oms-${{ matrix.target }}.tar.gz" > "oms-${{ matrix.target }}.sha256"
          fi
          cat "oms-${{ matrix.target }}.sha256"

      - uses: actions/upload-artifact@v4
        with:
          name: oms-${{ matrix.target }}
          path: |
            oms-${{ matrix.target }}.tar.gz
            oms-${{ matrix.target }}.sha256

  publish:
    needs: build
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/download-artifact@v4
        with:
          path: dist
          merge-multiple: true

      - name: one SHA256SUMS for the release
        working-directory: dist
        run: |
          cat ./*.sha256 > SHA256SUMS
          rm -f ./*.sha256
          cat SHA256SUMS
          test "$(wc -l < SHA256SUMS)" -eq 2

      - name: publish
        env:
          GH_TOKEN: ${{ github.token }}
        run: |
          gh release create "$GITHUB_REF_NAME" dist/* \
            --repo "$GITHUB_REPOSITORY" \
            --title "$GITHUB_REF_NAME" \
            --generate-notes
```

- [ ] **Step 2: Verify the workflow is valid**

Run: `actionlint .github/workflows/release.yml`
Expected: no output

- [ ] **Step 3: Verify the tag invariant catches a mismatch**

Run the check's logic locally against a deliberately wrong tag:

```bash
GITHUB_REF_NAME=v9.9.9
tag="${GITHUB_REF_NAME#v}"
crate="$(sed -n '/^\[package\]/,/^\[/p' Cargo.toml | sed -n 's/^version *= *"\(.*\)"/\1/p' | head -1)"
[ "$tag" = "$crate" ] && echo "WRONG: accepted a bad tag" || echo "ok: rejected v9.9.9 against $crate"
```

Expected: `ok: rejected v9.9.9 against 0.1.0`

Then the matching case:

```bash
GITHUB_REF_NAME=v0.1.0
tag="${GITHUB_REF_NAME#v}"
[ "$tag" = "$crate" ] && echo "ok: v0.1.0 matches" || echo "WRONG"
```

Expected: `ok: v0.1.0 matches`

- [ ] **Step 4: Commit and cut the first release**

```bash
git add .github/workflows/release.yml
git commit -m "ci: build and publish release binaries on a v* tag

Native runners per target — the quickfix C++ engine makes cross-compiling an
ordeal for no gain when GitHub hands out both architectures. Linux pins 22.04
rather than latest so the binary's glibc floor stays 2.35.

The tag is the version: the job refuses to build if it disagrees with
Cargo.toml, and refuses to package if the built binary disagrees with the tag."
git push

git tag v0.1.0
git push origin v0.1.0
```

Run: `gh run watch`
Expected: both matrix legs and `publish` succeed.

- [ ] **Step 5: Verify the release contents**

Run: `gh release view v0.1.0 --json assets -q '.assets[].name'`
Expected, exactly these three lines:

```
SHA256SUMS
oms-aarch64-apple-darwin.tar.gz
oms-x86_64-unknown-linux-gnu.tar.gz
```

Then verify the redirect the installer depends on resolves:

Run: `curl -sIL -o /dev/null -w '%{http_code}\n' https://github.com/maxkuttner/openoms/releases/latest/download/oms-aarch64-apple-darwin.tar.gz`
Expected: `200`

---

### Task 7: `install.sh`

**Files:**
- Create: `install.sh` (repo root)
- Modify: `.github/workflows/build.yml` (shellcheck job)

**Interfaces:**
- Consumes: the release assets from Task 6; `oms --version` from Task 1.
- Produces: the install one-liner Task 9's page prints. Honours `OMS_VERSION` (default `latest`) and `OMS_INSTALL_DIR` (default `$HOME/.local/bin`).

- [ ] **Step 1: Write the script**

Create `install.sh`:

```sh
#!/bin/sh
# openoms installer.
#
#   curl -fsSL https://maxkuttner.github.io/openoms/install.sh | sh
#
# Downloads the released `oms` binary for this platform, verifies it against the
# release's SHA256SUMS, and installs it into ~/.local/bin. That is all it does: no
# sudo, no package manager, no Postgres, no config file, no server started. It
# prints the commands to run next.
#
#   OMS_VERSION      release tag to install (default: latest)
#   OMS_INSTALL_DIR  where the binary lands (default: $HOME/.local/bin)

set -eu

REPO="maxkuttner/openoms"
VERSION="${OMS_VERSION:-latest}"
INSTALL_DIR="${OMS_INSTALL_DIR:-$HOME/.local/bin}"

die() {
	printf 'error: %s\n' "$1" >&2
	exit 1
}

detect_target() {
	os="$(uname -s)"
	arch="$(uname -m)"

	# An x86_64 shell under Rosetta reports x86_64 on an arm64 Mac. Install the
	# native build regardless — it is the one that machine should be running.
	if [ "$os" = "Darwin" ] && [ "$arch" = "x86_64" ] &&
		[ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = "1" ]; then
		arch="arm64"
	fi

	case "$os/$arch" in
	Darwin/arm64) echo "aarch64-apple-darwin" ;;
	Linux/x86_64) echo "x86_64-unknown-linux-gnu" ;;
	*) die "no prebuilt binary for $os/$arch — build from source: https://github.com/$REPO#setup" ;;
	esac
}

sha256_of() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | cut -d' ' -f1
	elif command -v shasum >/dev/null 2>&1; then
		shasum -a 256 "$1" | cut -d' ' -f1
	else
		die "need sha256sum or shasum to verify the download"
	fi
}

command -v curl >/dev/null 2>&1 || die "curl is required"
command -v tar >/dev/null 2>&1 || die "tar is required"

TARGET="$(detect_target)"
TARBALL="oms-$TARGET.tar.gz"

if [ "$VERSION" = "latest" ]; then
	BASE="https://github.com/$REPO/releases/latest/download"
else
	BASE="https://github.com/$REPO/releases/download/$VERSION"
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM

printf 'downloading %s (%s)\n' "$TARBALL" "$VERSION"
curl -fsSL "$BASE/$TARBALL" -o "$TMP/$TARBALL" || die "download failed: $BASE/$TARBALL"
curl -fsSL "$BASE/SHA256SUMS" -o "$TMP/SHA256SUMS" || die "download failed: $BASE/SHA256SUMS"

expected="$(grep " $TARBALL\$" "$TMP/SHA256SUMS" | cut -d' ' -f1 || true)"
[ -n "$expected" ] || die "$TARBALL is not listed in SHA256SUMS"
actual="$(sha256_of "$TMP/$TARBALL")"
[ "$expected" = "$actual" ] || die "checksum mismatch for $TARBALL
  expected $expected
  got      $actual"
printf 'checksum ok\n'

tar xzf "$TMP/$TARBALL" -C "$TMP"
[ -f "$TMP/oms" ] || die "$TARBALL did not contain an oms binary"

mkdir -p "$INSTALL_DIR"
if [ -e "$INSTALL_DIR/oms" ]; then
	printf 'replacing the existing %s/oms\n' "$INSTALL_DIR"
fi
install -m 755 "$TMP/oms" "$INSTALL_DIR/oms"

"$INSTALL_DIR/oms" --version >/dev/null 2>&1 ||
	die "the installed binary will not run: $INSTALL_DIR/oms --version failed"
printf 'installed %s to %s\n' "$("$INSTALL_DIR/oms" --version)" "$INSTALL_DIR/oms"

case ":$PATH:" in
*":$INSTALL_DIR:"*) ;;
*)
	printf '\n%s is not on your PATH. Add it:\n\n    export PATH="%s:$PATH"\n\n(then put that line in ~/.zshrc or ~/.bashrc)\n' \
		"$INSTALL_DIR" "$INSTALL_DIR"
	;;
esac

cat <<'EOF'

next:

    oms database init     # creates the database and oms.toml — needs a Postgres 16
    oms                   # starts the OMS on localhost:3001

then open http://localhost:3001/cockpit/

No Postgres yet? From a clone of the repo, `docker compose up -d` brings one up on
127.0.0.1:5432 with the defaults `oms database init` already assumes.
EOF
```

Note the two `set -eu` traps this avoids deliberately: the "already installed" notice is an `if` block rather than `[ -e … ] && printf`, whose non-zero result would exit the script when nothing is installed yet; and the `grep` for the checksum line ends in `|| true` so a missing entry reaches its own error message instead of dying silently.

Make it executable: `chmod +x install.sh`

- [ ] **Step 2: Lint it**

Run: `shellcheck install.sh`
Expected: no output. If `shellcheck` is missing: `brew install shellcheck`.

- [ ] **Step 3: Run it against the real release**

Run:

```bash
dir="$(mktemp -d)"
OMS_INSTALL_DIR="$dir" sh install.sh
```

Expected: `checksum ok`, then `installed oms 0.1.0 to <dir>/oms`, then the PATH notice (the temp dir is not on PATH) and the next-steps block.

Verify the binary is real and carries the UI:

```bash
"$dir/oms" --version        # oms 0.1.0
"$dir/oms" --help | head -3
```

Expected: version prints `oms 0.1.0`.

- [ ] **Step 4: Verify the failure paths**

Pinned version:

```bash
dir2="$(mktemp -d)"
OMS_VERSION=v0.1.0 OMS_INSTALL_DIR="$dir2" sh install.sh >/dev/null && echo "ok: pinned install"
```

Expected: `ok: pinned install`

A version that does not exist:

```bash
dir3="$(mktemp -d)"
OMS_VERSION=v99.0.0 OMS_INSTALL_DIR="$dir3" sh install.sh; echo "exit=$?"
```

Expected: `error: download failed: …/releases/download/v99.0.0/oms-…tar.gz`, `exit=1`, and `$dir3` left empty.

Checksum enforcement — the important one. Temporarily point the script at a tampered tarball by verifying the comparison directly:

```bash
tmp="$(mktemp -d)"
curl -fsSL https://github.com/maxkuttner/openoms/releases/latest/download/SHA256SUMS -o "$tmp/SHA256SUMS"
cat "$tmp/SHA256SUMS"
printf 'not a tarball' > "$tmp/oms-x86_64-unknown-linux-gnu.tar.gz"
expected="$(grep " oms-x86_64-unknown-linux-gnu.tar.gz\$" "$tmp/SHA256SUMS" | cut -d' ' -f1 || true)"
actual="$(shasum -a 256 "$tmp/oms-x86_64-unknown-linux-gnu.tar.gz" | cut -d' ' -f1)"
[ "$expected" = "$actual" ] && echo "WRONG: a corrupt file passed" || echo "ok: mismatch detected"
```

Expected: `ok: mismatch detected`, and `$expected` is non-empty (proving the `grep` pattern matches the real `SHA256SUMS` format the release publishes).

- [ ] **Step 5: Add the shellcheck job**

Append to `.github/workflows/build.yml`:

```yaml
  shellcheck:
    name: shellcheck install.sh
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
      # shellcheck is preinstalled on the GitHub Ubuntu images.
      - run: shellcheck install.sh cockpit/check-api-base.sh
```

Run: `actionlint .github/workflows/build.yml`
Expected: no output

- [ ] **Step 6: Commit**

```bash
git add install.sh .github/workflows/build.yml
git commit -m "feat: add the one-line installer

Detects the platform, downloads the matching release tarball, verifies it
against the release SHA256SUMS, installs into ~/.local/bin and smoke-runs the
binary. A checksum mismatch or missing checksum tooling is fatal.

It stops at PATH on purpose. A script piped from the internet into a shell gets
the smallest blast radius that is still useful, so it starts no database, writes
no config and launches nothing — it prints the two commands that do."
git push
```

Run: `gh run watch`
Expected: green, including the shellcheck job.

---

### Task 8: Post-release install smoke test

Task 7 verified the installer from a working copy on one machine. This verifies the *published* script against the *published* artifacts on both supported platforms, which is the thing a visitor actually runs.

**Files:**
- Create: `.github/workflows/install-smoke.yml`

**Interfaces:**
- Consumes: the Pages-hosted `install.sh` (Task 9) and the release assets (Task 6).
- Produces: nothing other tasks consume.

Ordering note: this workflow curls the Pages URL, which does not exist until Task 9 deploys. Write it now, but its first real run happens after Task 9 — the verification step below says so explicitly.

- [ ] **Step 1: Write the workflow**

Create `.github/workflows/install-smoke.yml`:

```yaml
name: Install smoke test

on:
  workflow_run:
    workflows: ["Release"]
    types: [completed]
  workflow_dispatch:

permissions:
  contents: read

jobs:
  install:
    if: >-
      github.event_name == 'workflow_dispatch' ||
      github.event.workflow_run.conclusion == 'success'
    strategy:
      fail-fast: false
      matrix:
        runner: [macos-14, ubuntu-22.04]
    runs-on: ${{ matrix.runner }}
    steps:
      - name: install exactly the way the landing page says to
        run: curl -fsSL https://maxkuttner.github.io/openoms/install.sh | sh

      - name: the installed binary runs and matches the latest release
        env:
          GH_TOKEN: ${{ github.token }}
        run: |
          got="$("$HOME/.local/bin/oms" --version | awk '{print $NF}')"
          tag="$(gh release view --repo "$GITHUB_REPOSITORY" --json tagName -q .tagName)"
          echo "installed $got, latest release $tag"
          [ "$got" = "${tag#v}" ] || { echo "installed version does not match the latest release"; exit 1; }

      - name: the binary carries the cockpit
        run: |
          "$HOME/.local/bin/oms" --help >/dev/null
          # The embed is bytes inside the binary, so grep for a string only the
          # built bundle contributes. A UI-less release would pass every other
          # check in this workflow.
          grep -qa 'oms-cockpit\|<div id="root">' "$HOME/.local/bin/oms" \
            || { echo "no cockpit bundle found in the released binary"; exit 1; }
```

- [ ] **Step 2: Verify the workflow is valid**

Run: `actionlint .github/workflows/install-smoke.yml`
Expected: no output

- [ ] **Step 3: Verify the cockpit-in-binary grep actually discriminates**

Against a local release build with a bundle:

```bash
(cd cockpit && npm run build) && cargo build --release
grep -qa '<div id="root">' target/release/oms && echo "ok: found the shell in the binary"
```

Expected: `ok: found the shell in the binary`

And against one without, to prove the check can fail:

```bash
rm -rf cockpit/dist && cargo build --release
grep -qa '<div id="root">' target/release/oms && echo "WRONG: matched an empty embed" || echo "ok: absent when unbundled"
```

Expected: `ok: absent when unbundled`

Rebuild the bundle afterwards so the working tree is left in the useful state: `(cd cockpit && npm run build)`

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/install-smoke.yml
git commit -m "ci: smoke-test the published installer on both platforms

Runs the exact command the landing page prints, against the real release, on
both supported runners. Also greps the installed binary for the cockpit shell:
a UI-less release passes every other check here."
git push
```

- [ ] **Step 5: Run it — after Task 9 is deployed**

This workflow curls `https://maxkuttner.github.io/openoms/install.sh`, which only exists once Pages is live. Come back after Task 9:

Run: `gh workflow run install-smoke.yml && sleep 5 && gh run watch`
Expected: both matrix legs pass.

---

### Task 9: Landing page

**Files:**
- Create: `site/index.html`
- Create: `site/style.css`
- Create: `.github/workflows/pages.yml`

**Interfaces:**
- Consumes: `install.sh` (Task 7), `assets/screenshot01.png`, `assets/screenshot02.png`, `cockpit/public/favicon.svg`.
- Produces: `https://maxkuttner.github.io/openoms/` and `https://maxkuttner.github.io/openoms/install.sh` — the URL Task 8 smoke-tests.

- [ ] **Step 1: Write the page**

Create `site/index.html`:

```html
<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>openoms — open source order management</title>
    <meta
      name="description"
      content="An open source multi-client order management system. One command to install."
    />
    <link rel="icon" href="favicon.svg" />
    <link rel="stylesheet" href="style.css" />
  </head>
  <body>
    <header>
      <img src="favicon.svg" alt="" width="48" height="48" />
      <h1>openoms</h1>
      <p class="tagline">An open source multi-client order management system.</p>
    </header>

    <main>
      <section class="install">
        <h2>Install</h2>
        <pre id="install-cmd"><code>curl -fsSL https://maxkuttner.github.io/openoms/install.sh | sh
oms database init
oms</code></pre>
        <button type="button" id="copy">Copy</button>
        <p class="after">
          Then open <code>http://localhost:3001/cockpit/</code>.
          <a href="install.sh">Read the script</a> before you pipe it into a shell —
          it downloads one checksummed binary into <code>~/.local/bin</code> and stops
          there. No sudo, nothing started, nothing configured.
        </p>
        <p class="note">
          <code>oms database init</code> needs a Postgres 16 to talk to. Don't have one?
          Clone the repo and run <code>docker compose up -d</code> — it brings up
          exactly the database the defaults assume.
        </p>
      </section>

      <section>
        <h2>What you get</h2>
        <ul>
          <li>Order routing across multiple brokers, over FIX and REST.</li>
          <li>A web cockpit compiled into the binary — blotter, positions, connections.</li>
          <li>Trading tokens scoped to a principal's portfolio grants.</li>
          <li>Risk checks and order reconciliation on reconnect.</li>
          <li>A Python client and CLI for everything the API exposes.</li>
        </ul>
      </section>

      <section>
        <h2>The cockpit</h2>
        <img src="assets/screenshot01.png" alt="The openoms order blotter" />
        <img src="assets/screenshot02.png" alt="An openoms detail view" />
      </section>

      <section>
        <h2>Requirements</h2>
        <ul>
          <li>macOS on Apple Silicon, or Linux on x86_64, for the prebuilt binary.</li>
          <li>Postgres 16.</li>
          <li>Anything else: build from source — see the readme.</li>
        </ul>
      </section>
    </main>

    <footer>
      <a href="https://github.com/maxkuttner/openoms">GitHub</a>
      <a href="https://github.com/maxkuttner/openoms#readme">Docs</a>
      <a href="https://github.com/maxkuttner/openoms/tree/main/clients/python">Python client</a>
    </footer>

    <script>
      // The install block is the whole point of the page; make it one click.
      document.getElementById("copy").addEventListener("click", async (e) => {
        await navigator.clipboard.writeText(
          document.getElementById("install-cmd").innerText,
        );
        e.target.textContent = "Copied";
        setTimeout(() => (e.target.textContent = "Copy"), 1500);
      });
    </script>
  </body>
</html>
```

Create `site/style.css`:

```css
/* Colors from cockpit/public/favicon.svg, so the page looks like the product. */
:root {
  --bg: #0d1014;
  --panel: #151a20;
  --border: #2a2f38;
  --text: #d7dce3;
  --dim: #8b949e;
  --accent: #25b083;
}

* {
  box-sizing: border-box;
}

body {
  margin: 0;
  padding: 4rem 1.25rem 6rem;
  background: var(--bg);
  color: var(--text);
  font: 16px/1.65 -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
}

header,
main,
footer {
  max-width: 46rem;
  margin: 0 auto;
}

header {
  text-align: center;
  margin-bottom: 3.5rem;
}

h1 {
  margin: 0.75rem 0 0.25rem;
  font-size: 2.25rem;
  letter-spacing: -0.02em;
}

.tagline {
  margin: 0;
  color: var(--dim);
}

h2 {
  font-size: 1.05rem;
  text-transform: uppercase;
  letter-spacing: 0.08em;
  color: var(--dim);
  margin: 3rem 0 0.75rem;
}

pre {
  background: var(--panel);
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 1.1rem 1.25rem;
  overflow-x: auto;
  margin: 0;
}

code {
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 0.9rem;
}

pre code {
  color: var(--accent);
}

#copy {
  margin-top: 0.6rem;
  background: transparent;
  color: var(--dim);
  border: 1px solid var(--border);
  border-radius: 6px;
  padding: 0.3rem 0.8rem;
  font-size: 0.85rem;
  cursor: pointer;
}

#copy:hover {
  color: var(--text);
  border-color: var(--accent);
}

.after,
.note {
  color: var(--dim);
  font-size: 0.92rem;
}

.note {
  border-left: 2px solid var(--border);
  padding-left: 0.9rem;
}

a {
  color: var(--accent);
}

ul {
  padding-left: 1.1rem;
}

li {
  margin: 0.35rem 0;
}

img {
  max-width: 100%;
  border: 1px solid var(--border);
  border-radius: 8px;
  margin-top: 1rem;
  display: block;
}

header img {
  border: 0;
  margin: 0 auto;
}

footer {
  margin-top: 4rem;
  padding-top: 1.5rem;
  border-top: 1px solid var(--border);
  display: flex;
  gap: 1.5rem;
  font-size: 0.9rem;
}
```

- [ ] **Step 2: Check it locally before deploying**

Assemble the published layout exactly as the workflow will, then serve it:

```bash
rm -rf /tmp/oms-site && mkdir -p /tmp/oms-site/assets
cp site/index.html site/style.css /tmp/oms-site/
cp install.sh /tmp/oms-site/install.sh
cp assets/screenshot01.png assets/screenshot02.png /tmp/oms-site/assets/
cp cockpit/public/favicon.svg /tmp/oms-site/favicon.svg
(cd /tmp/oms-site && python3 -m http.server 8099)
```

In another terminal:

```bash
curl -sf localhost:8099/ >/dev/null && echo "ok: page"
curl -sf localhost:8099/style.css >/dev/null && echo "ok: css"
curl -sf localhost:8099/favicon.svg >/dev/null && echo "ok: favicon"
curl -sf localhost:8099/assets/screenshot01.png >/dev/null && echo "ok: shot 1"
curl -sf localhost:8099/assets/screenshot02.png >/dev/null && echo "ok: shot 2"
curl -sf localhost:8099/install.sh | head -1   # #!/bin/sh
```

Expected: five `ok:` lines and `#!/bin/sh`.

Then open `http://localhost:8099/` in a browser: the screenshots render, the Copy button turns to "Copied" and the clipboard holds all three commands. Stop the server.

- [ ] **Step 3: Write the deploy workflow**

Create `.github/workflows/pages.yml`:

```yaml
name: Pages

on:
  push:
    branches: [main]
    paths:
      - "site/**"
      - "install.sh"
      - "assets/screenshot*.png"
      - ".github/workflows/pages.yml"
  workflow_dispatch:

permissions:
  contents: read
  pages: write
  id-token: write

concurrency:
  group: pages
  cancel-in-progress: true

jobs:
  deploy:
    runs-on: ubuntu-22.04
    environment:
      name: github-pages
      url: ${{ steps.deploy.outputs.page_url }}
    steps:
      - uses: actions/checkout@v4

      - name: assemble the site
        run: |
          mkdir -p _site/assets
          cp site/index.html site/style.css _site/
          # Served from the same origin as the page telling people to pipe it into
          # a shell. The repo copy stays canonical; this is a second URL for it.
          cp install.sh _site/install.sh
          cp assets/screenshot01.png assets/screenshot02.png _site/assets/
          cp cockpit/public/favicon.svg _site/favicon.svg

      - uses: actions/configure-pages@v5

      - uses: actions/upload-pages-artifact@v3
        with:
          path: _site

      - id: deploy
        uses: actions/deploy-pages@v4
```

Run: `actionlint .github/workflows/pages.yml`
Expected: no output

- [ ] **Step 4: Enable Pages and deploy**

GitHub Pages must be set to the Actions source once, by hand:

Run: `gh api -X POST repos/maxkuttner/openoms/pages -f 'build_type=workflow'`
Expected: JSON describing the Pages site. If it returns `409 Bad Request: already exists`, run `gh api -X PUT repos/maxkuttner/openoms/pages -f 'build_type=workflow'` instead.

```bash
git add site/index.html site/style.css .github/workflows/pages.yml
git commit -m "feat(site): landing page with the one-line install

Static, hand-written, no framework and no CDN. The workflow republishes the
repo-root install.sh into the site, so the curl URL shares an origin with the
page that prints it — the difference between a command that looks legitimate
and one that looks like a supply-chain attack."
git push
```

Run: `gh run watch`
Expected: the Pages job succeeds.

- [ ] **Step 5: Verify the published site**

```bash
curl -sf https://maxkuttner.github.io/openoms/ | grep -o '<title>.*</title>'
curl -sf https://maxkuttner.github.io/openoms/install.sh | head -1
curl -sf -o /dev/null -w '%{http_code}\n' https://maxkuttner.github.io/openoms/assets/screenshot01.png
diff <(curl -sf https://maxkuttner.github.io/openoms/install.sh) install.sh && echo "ok: published script matches the repo"
```

Expected: the title, `#!/bin/sh`, `200`, and `ok: published script matches the repo`.

Then the real thing, end to end, on your own machine:

```bash
dir="$(mktemp -d)"
OMS_INSTALL_DIR="$dir" sh -c "$(curl -fsSL https://maxkuttner.github.io/openoms/install.sh)"
"$dir/oms" --version
```

Expected: `oms 0.1.0`

- [ ] **Step 6: Run the smoke workflow from Task 8**

Now that the Pages URL exists:

Run: `gh workflow run install-smoke.yml && sleep 10 && gh run watch`
Expected: both `macos-14` and `ubuntu-22.04` legs pass.

- [ ] **Step 7: Point the readme at it**

In `readme.md`, insert above the existing `## Setup` heading:

````markdown
## Install

```sh
curl -fsSL https://maxkuttner.github.io/openoms/install.sh | sh
oms database init
oms
```

Then open <http://localhost:3001/cockpit/>. Prebuilt binaries cover macOS on Apple
Silicon and Linux on x86_64; the installer verifies a checksum, drops `oms` in
`~/.local/bin` and does nothing else. Everything below builds from source instead.
````

- [ ] **Step 8: Commit**

```bash
git add readme.md
git commit -m "docs: lead the readme with the one-line install"
git push
```

---

## Self-Review

**Spec coverage.** §1 release pipeline → Task 6 (matrix, 22.04 pin, per-target steps, publish job, version invariant). §2 `install.sh` → Task 7 (platform detection with the Rosetta case, URL resolution, checksum verification, install, smoke check, PATH report, both env vars) and Task 8 (post-release smoke on both runners); `shellcheck` on PRs → Task 7 Step 5. §3 cockpit embed → Task 2 (client changes, all three call sites), Task 3 (`include_dir`, missing-`dist` guard in `build.rs`, empty-embed message, cache headers, tests), Task 4 (routes, merge outside the auth layer, startup log, readme). §4 landing page → Task 9 (content order, colors, Pages workflow, `install.sh` republished into the artifact). Spec open item (no `LICENSE`) → Global Constraints; the page ships no license link and Task 9 does not invent one.

One requirement was missing from the spec and is now Task 1: `oms --version` does not exist today (`error: unexpected argument '--version' found`), yet the spec's installer smoke check and release invariant both depend on it.

**Type consistency.** `asset_for`, `is_bundled`, `respond`, `router`, `Asset { body, content_type, cache_control }`, `IMMUTABLE`, `NO_CACHE`, `DIST` are defined in Task 3 and used with the same names and signatures in Task 4's tests and handlers. `API_BASE` is exported from `cockpit/src/api/client.ts` in Task 2 and imported under that name in `App.tsx` and `ApiDocs.tsx`. Artifact names `oms-<target>.tar.gz` and `SHA256SUMS` are identical in Tasks 6, 7 and 8. `Router<AppState>` matches the state type the existing handlers take (`State<AppState>`, `src/handlers.rs:319`).

**Ordering.** Task 8's workflow depends on a URL Task 9 creates; the plan states this in the task and defers its run to Task 9 Step 6.
