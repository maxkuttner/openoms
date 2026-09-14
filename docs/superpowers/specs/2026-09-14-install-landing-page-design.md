# Landing page and one-line installer

**Date:** 2026-09-14
**Status:** approved design, not yet implemented

## Problem

Installing openoms today means cloning the repo and building it:

```sh
git clone git@github.com:maxkuttner/openoms.git && cd openoms
docker compose up -d
cargo run -- init
cargo run
```

That asks a visitor for a Rust toolchain, cmake, and (on macOS) `openssl@3` before
they see anything. The FIX engine is C++, so a cold build is minutes long and fails
in ways that have nothing to do with the OMS. The admin console is worse: it is a
separate vite app, so a fourth and fifth command (`npm install && npm run dev`) stand
between the visitor and the only part of the system that looks like anything.

We want the familiar `curl … | sh` install: a prebuilt binary on PATH in one command,
a running OMS with a working UI in three, and a page that says so.

## Scope

In scope:

1. A tag-driven release workflow producing signed-by-checksum binary tarballs for
   macOS arm64 and Linux x86_64.
2. `install.sh` at the repo root — downloads, verifies, installs, stops.
3. The cockpit bundle embedded in the `oms` binary and served at `/cockpit/`.
4. A static landing page on GitHub Pages that also serves `install.sh`.

Out of scope, deliberately:

- **Provisioning.** The installer does not start Postgres, write `oms.toml`, create a
  database, or launch the server. It installs a binary and prints what to run next.
  A script piped from the internet gets the smallest blast radius that still helps.
- **Homebrew tap / cargo-binstall.** Worth adding later on top of this release
  pipeline — the tap would point at the same artifacts — but a second repo to
  maintain is not what makes the install one line.
- **macOS notarization.** The binary is unsigned. Files fetched with `curl` carry no
  quarantine attribute, so Gatekeeper does not block them when run from a terminal.
  Notarization only matters if we ever ship a double-clickable artifact.
- **Windows.** `install.sh` exits with a clear message pointing at the from-source
  instructions.

## Architecture

Four pieces, each independently testable:

```
tag v0.1.0 ──▶ release.yml ──▶ GitHub Release
                  │              (oms-<target>.tar.gz ×2, SHA256SUMS)
                  │                        ▲
                  │                        │ downloads + verifies
      npm run build │                 install.sh
                  ▼                        ▲
        cockpit/dist embedded              │ served from
        into the oms binary          pages.yml ──▶ GitHub Pages (site/ + install.sh)
```

The release workflow is the only producer. `install.sh` is the only consumer of the
release artifacts. The Pages workflow publishes the page and republishes `install.sh`
from the repo root, so the script has one source of truth and two URLs.

## 1. Release pipeline

New `.github/workflows/release.yml`, triggered by `push: tags: ['v*']`. The existing
`build.yml` is left alone; it keeps doing PR and `main` verification.

### Target matrix

| Runner | Target triple | Artifact |
|---|---|---|
| `macos-14` | `aarch64-apple-darwin` | `oms-aarch64-apple-darwin.tar.gz` |
| `ubuntu-22.04` | `x86_64-unknown-linux-gnu` | `oms-x86_64-unknown-linux-gnu.tar.gz` |

Each target builds on its own native runner. The `quickfix` crate is C++ and links
OpenSSL, which makes real cross-compilation an ordeal for no benefit when GitHub
hands out both architectures.

`ubuntu-22.04` is pinned rather than `ubuntu-latest` on purpose. The runner label
`latest` currently resolves to 24.04, whose glibc 2.39 produces a binary that refuses
to start on Debian 12 or Ubuntu 22.04 — a large share of the servers this would
actually be installed on. Building against glibc 2.35 costs nothing and runs
everywhere newer. Pinning also stops a future runner-image bump from silently
narrowing compatibility.

Intel Macs (`x86_64-apple-darwin`) and ARM servers (`aarch64-unknown-linux-gnu`) are
not built. Adding a leg later is a matrix entry plus a case in the installer's triple
detection; the design does not have to change.

### Per-target job steps

1. `actions/checkout@v4`
2. `actions/setup-node@v4` (node 20), then `npm ci && npm run build` in `cockpit/`.
   This populates `cockpit/dist`, which the Rust build embeds (see §3). It must run
   before `cargo build`.
3. `dtolnay/rust-toolchain@stable` and `Swatinem/rust-cache@v2`, matching `build.yml`.
4. macOS only: `brew install openssl@3`. `build.rs` already locates a Homebrew
   OpenSSL and adds its lib dir to the linker search path. `cmake` is preinstalled on
   both runner images.
5. `cargo build --release`
6. `strip target/release/oms`
7. `tar czf oms-<target>.tar.gz -C target/release oms` — the tarball contains the
   single binary at its root, nothing else.
8. Compute the SHA-256 of the tarball into `<artifact>.sha256`.
9. `actions/upload-artifact@v4` for the tarball and its checksum file.

### Publish job

`needs` both matrix legs. Downloads every artifact, concatenates the per-target
checksum files into one `SHA256SUMS` in `sha256sum` format, and publishes a GitHub
Release on the tag with both tarballs and `SHA256SUMS` attached, using
`--generate-notes` for a first draft of the release notes.

### Version invariant

The tag is the source of truth for the version. Before building, each job asserts
that the tag (with the leading `v` stripped) equals the `version` field in
`Cargo.toml`, and fails the workflow otherwise. Without this, `oms --version` can
disagree with the release it shipped in, and nobody notices until a bug report cites
a version that never existed.

`Cargo.toml` currently declares `0.1.0`, so the first release tag is `v0.1.0`.

## 2. `install.sh`

Lives at the repository root. POSIX `sh` — no bashisms, no sudo, no package manager.
Roughly 120 lines. Begins with `set -eu` and a comment block stating what it does and
what it deliberately does not do.

### Behaviour

1. **Detect the platform.** `uname -s` and `uname -m` map to a target triple:
   `Darwin`/`arm64` → `aarch64-apple-darwin`, `Linux`/`x86_64` →
   `x86_64-unknown-linux-gnu`. Anything else exits 1 naming the detected platform and
   pointing at the from-source instructions in the readme.

   Rosetta caveat: an x86_64 shell on Apple Silicon reports `x86_64`. When
   `sysctl -n sysctl.proc_translated` returns `1`, the script selects the arm64 build
   anyway — the native binary is what the machine should run.

2. **Resolve the download URL.** Default is GitHub's stable latest-release redirect:

   ```
   https://github.com/maxkuttner/openoms/releases/latest/download/oms-<target>.tar.gz
   ```

   This needs no API call, so there is no unauthenticated rate limit to hit behind a
   shared NAT. Setting `OMS_VERSION=v0.1.0` switches to the pinned form
   `…/releases/download/v0.1.0/oms-<target>.tar.gz`.

3. **Download and verify.** Fetch the tarball and `SHA256SUMS` with `curl -fsSL` into
   a `mktemp -d`, removed by a `trap` on `EXIT`. Verify with whichever of
   `sha256sum` or `shasum -a 256` exists. A mismatch aborts with a loud message and a
   non-zero exit; the script never installs an unverified binary. Neither tool being
   present is also a hard failure, not a silent skip.

4. **Install.** Extract, then `install -m 755` the binary into `$OMS_INSTALL_DIR`
   (default `$HOME/.local/bin`, created if needed). No sudo, nothing written outside
   the user's home. If a binary is already there it is replaced, and the script says
   so before doing it.

5. **Smoke check.** Run `"$OMS_INSTALL_DIR/oms" --version` and echo the result. An
   install that produces a binary which cannot execute — wrong glibc, truncated
   download — should fail here, not at the user's first real command.

6. **Report.** If `$OMS_INSTALL_DIR` is not on `$PATH`, print the exact `export PATH=…`
   line and name the likely shell rc file. Then print the next steps:

   ```
   oms database init     # creates the database and oms.toml
   oms                   # starts the OMS on localhost:3001
   ```

   plus the cockpit URL and a one-line note that `oms database init` needs a Postgres
   to talk to.

### Prerequisite: `oms --version`

The smoke check and the release workflow's tag assertion both shell out to
`oms --version`, which the CLI does not currently accept — `#[command(...)]` at
`src/main.rs:293` declares `name` and `about` but not `version`. Adding the `version`
token makes clap generate it from the crate version.

### Environment variables

| Variable | Default | Purpose |
|---|---|---|
| `OMS_VERSION` | `latest` | Pin a specific release tag. |
| `OMS_INSTALL_DIR` | `$HOME/.local/bin` | Where the binary lands. |

### Testing

- `shellcheck` runs against `install.sh` in `build.yml` on every pull request.
- A post-release smoke job runs on both `macos-14` and `ubuntu-22.04`: it curls the
  *published* script from Pages, runs it, and asserts `oms --version` matches the tag
  just released. It triggers on `workflow_dispatch` and on completion of the release
  workflow, so it exercises the real artifacts over the real network rather than a
  local copy of the script.

## 3. Cockpit embedded in the binary

Without this, the headline install ends at a bare JSON API and the screenshots on the
landing page are unreachable without cloning the repo.

### Client changes

`cockpit/src/api/client.ts:25` hardcodes a `/api` prefix on every request, which only resolves
through the vite dev proxy. Three call sites need a shared base:

- `cockpit/src/api/client.ts:25`
- `cockpit/src/App.tsx:45` (`/api/health`)
- `cockpit/src/pages/ApiDocs.tsx:241` (`/api/api-docs/openapi.json`)

All three use `const API_BASE = import.meta.env.DEV ? "/api" : ""`, exported from the
api module. In dev the proxy keeps working unchanged; in the bundled build the
cockpit is same-origin with the OMS and calls `/admin/*` directly.

Nothing else in the cockpit needs to change. `vite.config.ts` already sets
`base: "/cockpit/"` for builds and `cockpit/src/main.tsx:17` already feeds
`import.meta.env.BASE_URL` to the router basename — both were written in
anticipation of this and are correct as they stand.

### Server changes

New `src/cockpit.rs`, modelled on `src/setup/database/assets.rs`:

- `static DIST: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/cockpit/dist");`
- A router merged into the main `Router` at `src/main.rs:1178`:
  - `/cockpit` → 307 redirect to `/cockpit/`, so the trailing slash the SPA expects is
    not the visitor's problem.
  - `/cockpit/*path` → serve the embedded file. On a miss, if the path has no file
    extension, serve `index.html` — this is what makes deep links like
    `/cockpit/orders` survive a page reload.
  - Content type from the file extension via `mime_guess`.
  - Cache headers: vite emits content-hashed asset filenames, so `assets/*` gets
    `cache-control: public, max-age=31536000, immutable`; `index.html` gets
    `cache-control: no-cache` so a new build is picked up immediately.
- The router is merged **outside** `auth::admin_middleware` (applied at
  `src/main.rs:1171` to `admin_router` only). The SPA shell has to load before there is
  a token to send; the cockpit's own login gate then authenticates against `/admin`
  exactly as it does today. No admin data is served by these routes — only the static
  bundle.
- Startup logging gains `Cockpit: http://<bind_addr>/cockpit/` alongside the existing
  Scalar and OpenAPI lines, printed only when the embed is non-empty.

### The missing-`dist` problem

`cockpit/dist` is gitignored (`cockpit/.gitignore:2`), so a fresh clone does not have
it, and `include_dir!` on a missing directory is a compile error. `build.rs` creates
the directory when absent and emits `cargo:rerun-if-changed=cockpit/dist`.

This keeps a single code path. A cargo feature flag (`--features cockpit`) was
considered and rejected: it adds a build mode that is only ever exercised in CI, and
the failure mode is a release built without the flag, shipping a binary whose UI
silently 404s.

An empty embed is legitimate — it is what every source build produces. So, unlike the
migrations embed (which asserts non-empty at release, `src/setup/database/assets.rs:91`),
an empty cockpit embed is not an error. The route returns 404 with a body reading:

```
cockpit not bundled in this build — run `cd cockpit && npm run dev` for the dev UI on :5173
```

### Testing

- Rust tests assert: `/cockpit` redirects to `/cockpit/`; `/cockpit/` serves
  `index.html`; an extensionless deep link serves `index.html`; a missing asset with an
  extension 404s. Because embed contents depend on whether `npm run build` ran, the
  tests branch on `DIST.entries().is_empty()` and assert the correct behaviour for
  whichever build is under test — including the "not bundled" message for source
  builds.
- `build.yml` gains a job that runs `npm ci && npm run build` before `cargo build` and
  then runs those tests, so the populated-embed path is covered on every pull request
  rather than only at release time.

### Documentation

`readme.md` gains a short note: the released binary serves the cockpit at
`/cockpit/`; developing the cockpit still means `npm run dev` against a running OMS.
The existing from-source setup section stays as it is — it is still the right path for
contributors and for unsupported platforms.

## 4. Landing page

`site/index.html` and `site/style.css`. Hand-written, no framework, no build step, no
CDN, no analytics, no external fonts. The only JavaScript is a copy-to-clipboard
button on the command block.

### Content

In order:

1. Name, the favicon mark, and a one-line description ("an open source multi-client
   order management system").
2. **The install block** — the reason the page exists:

   ```sh
   curl -fsSL https://maxkuttner.github.io/openoms/install.sh | sh
   oms database init
   oms
   ```

   followed by "then open http://localhost:3001/cockpit/". Next to it, a plain link to
   the script itself, labelled so that reading it before piping it to a shell is the
   obvious move.
3. A short aside for people without a Postgres: `docker compose up -d` from a clone,
   or any Postgres 16 they already have.
4. Four or five bullets on what the thing is — multi-broker order routing, FIX and
   REST adapters, the cockpit, trading tokens, the Python client.
5. The two existing screenshots, `assets/screenshot01.png` and `assets/screenshot02.png`.
6. Requirements: macOS arm64 or Linux x86_64 for the prebuilt binary, Postgres 16,
   everything else builds from source.
7. Links: GitHub repo, readme, `clients/python/README.md`.

### Visual direction

Dark, consistent with the cockpit's Mantine dark theme and the favicon: background
`#0d1014`, accent `#25b083`, system font stack, one column, generous line height.
Monospace only inside command blocks. It should look like the product, not like a
template.

### Deployment

`.github/workflows/pages.yml`, triggered on pushes to `main` touching `site/**` or
`install.sh`, plus `workflow_dispatch`. It assembles the artifact from `site/`, copies
the repo-root `install.sh` into it, copies in the two screenshots from `assets/`, then
uses `actions/upload-pages-artifact` and `actions/deploy-pages`. Pages is served from
the default `maxkuttner.github.io/openoms` domain; no custom domain, no DNS.

Publishing `install.sh` through Pages means the curl URL is same-origin with the page
the visitor is reading, which is the difference between a command that looks
legitimate and one that looks like a supply-chain attack. The repo copy stays
canonical and its `raw.githubusercontent.com` URL keeps working as a fallback.

## Open items

- **No `LICENSE` file exists in the repo.** The readme calls the project open source
  and the landing page will want a license in the footer. Pick one and add it; this
  design does not choose for you.

## Consequences

- Releases become a deliberate act: tag, wait for CI, verify the smoke job. There is
  no longer a path where `main` ships to strangers automatically, which is the point.
- The release build now depends on node. A contributor's `cargo build` does not — they
  get an empty embed and a useful 404 — but a release built without running
  `npm run build` first would ship a UI-less binary. The release workflow is the only
  place that matters, and it always runs the build.
- `install.sh` and the release artifact naming are a contract. Renaming the binary or
  the tarballs breaks every installed copy's upgrade path, so the names are fixed from
  the first tag onwards.
