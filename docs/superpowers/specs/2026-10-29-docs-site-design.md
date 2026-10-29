# Documentation site — design

Date: 2026-10-29
Status: approved by owner 2026-10-29

## Intent

openoms needs documentation a trader can follow from nothing to a working
install without opening the source. Today the material is split between the
README (install, configuration, credentials), a hand-built GitHub Pages site
(`site/`: landing page, architecture page, an API reference rendered by a
custom `site/build-docs.mjs`), and internal notes under `docs/`. There is no
navigable, searchable, task-ordered documentation.

The reference the owner pointed at is NautilusTrader's docs
(nautilustrader.io/docs/latest/) — valued for its structure: Getting Started →
Concepts → Tutorials → Integrations (one page per venue) → API Reference.

## Audience

Traders with enough technical ability to SSH into their own server and install
a binary. Not assumed to be Rust developers or ops specialists.

Their first ten minutes, in order:

1. **Install** — one command on their server.
2. **Configure** — Postgres, broker credentials, login. Where they get stuck;
   needs concrete, copy-pasteable `oms.toml` examples.
3. **Test it** — a paper order through to a broker as fast as possible.
4. **See the trading API** — submit/cancel/orders/positions with real request
   and response examples, runnable immediately (`curl`, the Python client in
   `clients/python`).

**Success:** a trader goes from nothing to a filled paper order and an
authenticated API call using only the docs.

## Decisions (owner's)

- **Framework: Starlight** (Astro). Chosen over GitBook because everything
  must live in the repo, be free, and be checkable in CI. MkDocs Material and
  Docusaurus were the runners-up.
- **The docs become the whole site.** The Starlight site replaces `site/`
  entirely and is served at `https://maxkuttner.github.io/openoms/`.
- **Structure modelled on NautilusTrader** (see Information architecture).
- **v1 scope** as listed below.
- **Python client distribution:** documented as
  `pip install "git+https://github.com/maxkuttner/openoms#subdirectory=clients/python"`.
  No PyPI publishing in this project. "Your first API call" shows both `curl`
  and the Python client.
- **`deploy/` stays internal.** It is the owner's test-server setup (fixed
  host, `/opt/oms`, host networking) and is not documented publicly.
  "Running as a service" shows a hand-written systemd unit for the installed
  binary, and states plainly that no Docker image is published yet.

## Hard constraints

- `https://maxkuttner.github.io/openoms/install.sh` must keep serving the
  repo's `install.sh`, byte-identical. The README, `install-smoke.yml`, and
  anyone's saved command depend on that URL. The repo copy stays canonical; the
  site build copies it in, as `pages.yml` does today.
- Published from `main` via GitHub Actions to GitHub Pages (replacing the
  current `pages.yml` job). No hosted docs service.
- The site's base path is `/openoms/` (project Pages site); all links and
  assets must work under it.
- The API reference is generated from the committed `docs/openapi.json`, never
  hand-written, so it cannot drift from the server. (`docs/openapi.json` is
  already kept current by the `the_committed_openapi_spec_is_current` test.)
- Docs must describe the product as it is on `main` — commands, flags, config
  keys and file locations must match the code. In particular, the config
  search order shipped in 3852f04 (`$OMS_CONFIG` → `./oms.toml` →
  `~/.config/oms/oms.toml` → `/etc/oms/oms.toml`, `oms init` writes the
  per-user location) and `oms update` (ff5a4d5).
- Supported install platforms are exactly those `release.yml` builds:
  `aarch64-apple-darwin` and `x86_64-unknown-linux-gnu` (glibc ≥ Ubuntu 22.04 /
  Debian 12, OpenSSL 3). Anything else is "build from source".

## Information architecture

```
Home (splash)                      ← replaces site/index.html
Getting Started
  Installation                     ← install.sh, platforms, PATH, build from source
  First run                        ← Postgres, `oms init`, start, open the cockpit
  Your first paper trade           ← Alpaca paper end to end (credentials → order → fill)
  Your first API call              ← auth, submit/cancel/list via curl and Python client
Configuration
  oms.toml reference               ← every key, env var and flag; precedence
  Config file locations            ← search order, .env beside config, OMS_CONFIG
  Login (OIDC)                     ← [auth.oidc], Keycloak dev realm, https rule
Concepts
  Architecture                     ← ported from site/architecture.html + diagrams
  (stubs allowed in v1: orders & lifecycle, portfolios & permissions,
   positions, risk checks, marks & prices)
Integrations
  Alpaca                           ← full
  Binance                          ← full
  Interactive Brokers (FIX)        ← short
  Bybit                            ← short
  Databento / OPRA                 ← short
Tutorials
  OPRA marks & P&L                 ← ported from docs/tutorials/opra-marks-pnl.md
Operations
  Updating                         ← `oms update`, --check, --version rollback,
                                     migrate + restart afterwards
  Running as a service             ← hand-written systemd unit; no Docker image yet
  Master key & backups             ← what to back up, rotate-key
API Reference                      ← generated from docs/openapi.json
```

"Full" pages are complete walkthroughs; "short" pages cover credentials,
supported asset classes, and known limits, and may be brief.

## v1 scope

Must ship:

1. Splash home page (hero, screenshots from `assets/screenshot0*.png`, the
   install one-liner, "Get started" link).
2. All of Getting Started.
3. Configuration (all three pages).
4. Integrations: Alpaca and Binance full; IBKR, Bybit, Databento short.
5. Operations (all three pages).
6. API Reference, generated.
7. Architecture page and OPRA tutorial ported.
8. `install.sh` served at its existing URL.
9. Built-in search, dark mode, mobile layout (Starlight defaults).

Not in v1: Concepts pages beyond Architecture (stub or omit), versioned docs,
i18n, a blog/changelog page, custom domain.

## Non-goals and anti-goals

- **Not** a second copy of the README. Once the site exists, the README
  shrinks to a pitch, the install one-liner, and links into the docs; the docs
  are the single source for install/config prose.
- **Anti-goal:** docs that promise features that don't exist. Gaps (no equity
  price feed, no systemd installer, no published Docker image) are stated
  plainly where a reader would hit them, not glossed.
- **Anti-goal:** an API reference that can drift — anything hand-maintained
  about endpoints beyond tutorials' worked examples.
- No hosted service, no paid dependency, no analytics/tracking.

## What exists to reuse

- `site/index.html`, `site/style.css` — landing copy and visual identity
  (favicon at `cockpit/public/favicon.svg`).
- `site/architecture.html` + `site/diagrams/*.mmd` (+ `mermaid-config.json`).
- `site/build-docs.mjs` — current custom OpenAPI renderer; replaced, not kept.
- `.github/workflows/pages.yml` — current deploy (assemble `_site`, render
  diagrams with mermaid-cli in CI, run build-docs, upload).
- README sections: Build from source, Prerequisites, Configuration table,
  credentials, Loading instruments, OIDC.
- `docs/tutorials/opra-marks-pnl.md`.
- `clients/python` (examples/, `oms_client`) for API-call examples.
- `docker/keycloak/oms-dev-realm.json` (dev login `trader`/`trader`).

## Verification of walkthroughs

Every Getting Started and "full" Integrations walkthrough is verified by
actually running it, not by reading code. The owner's dev environment has
working Alpaca paper and Binance paper credentials (both adapters register on
the test server; Alpaca paper orders were submitted and cancelled end to end on
2026-10-28), so the builder runs the walkthroughs against those.

*Builder's call (2026-10-29):* "Your first paper trade" uses **1 share of AAPL
on Alpaca paper** — a plain equity order needs no market-data subscription, so
the reader is not sent to Databento/OPRA first. If the Binance walkthrough
cannot be completed end to end on testnet, Binance drops from "full" to
"short" rather than shipping an unverified guide.

## Left to the builder

- Where the Starlight project lives (suggestion: top-level `docs-site/`;
  `docs/` stays internal — specs, plans, `openapi.json`).
- Astro/Starlight versions and the OpenAPI plugin (e.g. `starlight-openapi`).
- How Mermaid diagrams render (a Starlight/remark plugin rendering client-side,
  or keep the CI mermaid-cli step) — either is fine if the diagrams look right
  in light and dark mode.
- Visual theme details, as long as it reads as the same product as the
  cockpit.
- Whether to add CI checks for broken internal links and for commands/flags in
  the docs matching `oms --help` (desirable, not required for v1).
- What happens to `site/` files after the port (delete in the same change is
  the expected outcome).
- The README trim, as part of the same work or a follow-up.
