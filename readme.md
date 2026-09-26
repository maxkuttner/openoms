# OpenOMS

<p align="center">
  <img alt="openOMS" src="cockpit/public/favicon.svg" width="72">
</p>
<p align="center">
  <a href="https://github.com/maxkuttner/openoms/actions/workflows/build.yml">
    <img alt="Build and Test" src="https://github.com/maxkuttner/openoms/actions/workflows/build.yml/badge.svg">
  </a>
</p>

*An* open source multi-client order management system.

![./assets/screenshot01.png](./assets/screenshot01.png)
![./assets/screenshot02.png](./assets/screenshot02.png)

---

## Install

```sh
curl -fsSL https://maxkuttner.github.io/openoms/install.sh | sh
```

Prebuilt binaries cover macOS (Apple Silicon) and Linux (x86_64, needs OpenSSL
3 — Debian 12 / Ubuntu 22.04+). `~/.local/bin` may need adding to `PATH`; the
installer prints the line if so.

```sh
oms database init
oms
```

Then open <http://localhost:3001/cockpit/>.

## Build from source

```sh
git clone git@github.com:maxkuttner/openoms.git && cd openoms
docker compose up -d  # a local Postgres; skip if you already have one
cargo run -- init     # prompts for your Postgres, writes oms.toml, creates the database
cargo run             # start the OMS on localhost:3001
```

With the bundled Postgres, press Enter through every `init` prompt and use
password `postgres`. For the cockpit UI in dev mode (hot-reload, proxied API):

```sh
cd cockpit && npm install && npm run dev  # localhost:5173
```

A released binary serves the cockpit itself (no second process needed) — the
bundle is embedded at build time, so a source build needs `npm run build` in
`cockpit/` before `cargo build` or `/cockpit/` 404s.

`init` is idempotent-safe but not re-runnable: if `oms.toml` exists it refuses
and points you at `database migrate` instead. If it fails partway (after
writing `oms.toml`), fix the cause and run `oms database init --resume`.

The instrument catalog starts empty — see [Loading instruments](#loading-instruments).

## Prerequisites

- **Rust** (stable) — `curl https://sh.rustup.rs -sSf | sh`
- **cmake** and a C++ compiler — the embedded FIX engine (`quickfix`) is C++
- **OpenSSL 3** on macOS — `brew install openssl@3` (Linux uses the system one)
- **PostgreSQL** — Docker (bundled `docker-compose.yml`) or your own server
- **Node** — only for the cockpit web UI

```sh
brew install cmake openssl@3 node               # + postgresql@16 if not using Docker
```

## Configuration

Everything lives in `oms.toml`, written by `oms init`. Flags and env vars
override it, in that order:

| Setting | `oms.toml` | Flag | Environment |
|---|---|---|---|
| Host | `database.host` | `--host` | `POSTGRES_HOST` |
| Port | `database.port` | `--port` | `POSTGRES_PORT` |
| Superuser name | `database.username` | `--username` | `POSTGRES_USERNAME` |
| Superuser password | *(never stored)* | `--password` | `POSTGRES_PASSWORD` |
| Database | `database.database` | `--database` | `POSTGRES_DATABASE` |
| `oms` role password | `oms.password` | `--oms-password` | `OMS_PASSWORD` |
| Master key | `oms.master_key` | — | — |
| Bind address | `server.bind_addr` | — | `OMS_BIND_ADDR` |
| Cockpit password | `server.admin_password` | — | `OMS_ADMIN_PASSWORD` |

Unset falls back to a built-in default (`localhost`/`5432`/`postgres`/`ods`,
`openoms-dev` for both app passwords) — loopback only; a default password is
refused against anything else.

**Back up `oms.toml`.** Its master key is the only thing that decrypts stored
broker/feed credentials — losing the file loses them, with no recovery path.
The Postgres superuser password is never stored in it.

`.env` is an override file, not a prerequisite — copy `.env.example` for a
real admin password or a remote database. Broker/feed credentials don't go
here; see below.

## Broker and feed credentials

API keys live encrypted in Postgres, sealed with `oms.toml`'s master key —
**not read from the environment** at boot (only `*_ENV`/`*_TRANSPORT` routing
vars are). Configure them:

- **Cockpit** — each Broker connection / Data feed row has a Credentials
  panel: shows `configured`/`unconfigured`/`error` status, edits one field at
  a time (secrets stay blank unless changed), **Test** checks against the
  provider before **Save** writes anything.
- **One-shot import** from old `.env` vars: `cargo run -- config import-env`.
- **Rotate the master key**: `cargo run -- config rotate-key` re-seals every
  credential and prints the new key — save it into `oms.toml` before
  discarding the old one.

After a credential change, `POST /admin/connections/reload` applies it
without a restart — except IBKR/Binance FIX sessions, which report
`RestartRequired` (a live FIX session can't be swapped without one; trading
through it is unaffected in the meantime). The response names every
connection with its outcome; a 200 with mixed outcomes is normal.

## Database commands

```sh
cargo run -- database init             # create everything; fails if it already exists
cargo run -- database init --resume    # finish an init that failed partway through
cargo run -- database migrate          # apply pending migrations (idempotent)
cargo run -- database status           # what exists, what is pending
cargo run -- database drop             # destroy the database (roles are kept)
```

`init`/`migrate`/`drop` connect as the superuser; `status` connects as the
`oms` role (falling back to the superuser if that's never been set up).
Upgrading an existing install: `git pull && cargo run -- database migrate`.

One role, `oms`, owns everything: `public` schema for shared reference data
(instruments, venues, currencies), `oms` schema for this app's own tables
(orders, portfolios, principals, accounts, ...).

## Loading instruments

The catalog comes from brokers, not a bundled list.

- **Automatic** — with credentials imported, an empty catalog populates on
  boot (can take minutes for full option chains). Opt out with
  `OMS_SYNC_ON_BOOT=never`, or narrow it with `OMS_SYNC_UNDERLYINGS=SPY,QQQ`.
- **Explicit**:
  ```sh
  cargo run -- setup sync-broker --broker alpaca [--underlyings SPY,QQQ] [--dry-run]
  ```

## Trading

**Cockpit** (`localhost:5173`) — configuration, monitoring, minting tokens.

**Python**, for actually sending orders:

```sh
pip install -e clients/python
```

```python
from oms_client import OMS

oms = OMS("http://localhost:3001", token=os.environ["OMS_TRADING_TOKEN"])

pf  = oms.portfolios()[0]
oid = oms.submit(portfolio=pf.portfolio_id, symbol="SPY260918C00770000@OPRA", side="buy", quantity=1)
print(oms.wait_for(oid).status)

for row in oms.orders(status="routed"):
    print(row.order_id, row.instrument_symbol, row.cum_qty)
```

There's a CLI too — `oms orders list`, `oms positions`, `oms submit`. See
[`clients/python/README.md`](clients/python/README.md).

## Auth

- **Cockpit login** — one password, `OMS_ADMIN_PASSWORD` (enable with
  `OMS_ADMIN_AUTH_ENABLED=true`), sent as a bearer to `/admin`.
- **Trading tokens** — minted in the cockpit or via `POST
  /admin/trading-tokens`, shown once. Belongs to a **principal**; what it can
  trade comes from that principal's portfolio grants. Mint several to rotate
  without re-permissioning. Can never reach `/admin`.

### Login for traders (OIDC)

Off by default — with no `[auth.oidc]` block, `/auth/*` and `/trade/` all
404, nothing else changes. To enable:

```toml
[auth.oidc]
issuer = "https://idp.example.com/realms/oms"
client_id = "oms"
public_base_url = "https://oms.example.com"
```

Register a **confidential client** at the provider with redirect URI
`{public_base_url}/auth/callback`. Set `OMS_OIDC_CLIENT_SECRET` in the
environment (never in `oms.toml`). A misconfigured provider just leaves
login off for that run — order routing and the cockpit are unaffected.

**Non-loopback bind requires `https://`** in `public_base_url`, or the server
refuses to start (the session cookie needs `Secure`). See
[`docker-compose.yml`](docker-compose.yml)'s `oidc` profile for a disposable
Keycloak to test against.

With OIDC on, `/trade/` serves a separate trader-facing app (as distinct from
the admin `/cockpit/`) — a trader signs in, and sees only the portfolios
their principal is granted, never the full list an admin sees.

## Troubleshooting

| Symptom | Cause |
|---|---|
| `refusing to start: OMS_ADMIN_PASSWORD is not set` | `OMS_BIND_ADDR` is not loopback. Set a real admin password in `.env`. |
| `password authentication failed for user "oms"` | `OMS_PASSWORD` doesn't match the role. Fix the value, or `ALTER ROLE oms PASSWORD '…'`. |
| `role "oms" already exists` on init | Something is already provisioned. Use `migrate`, or `drop` first. |
| Server exits naming a migration | Run `cargo run -- database migrate`. |
| Link error mentioning `-lssl` on macOS | `brew install openssl@3`, or set `OPENSSL_DIR`. |
