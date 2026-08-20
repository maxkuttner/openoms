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

## Setup

```sh
git clone git@github.com:maxkuttner/openoms.git && cd openoms
cargo run -- database init     # create the database
cargo run                      # start the OMS on localhost:3001
```

Then, in a second terminal:

```sh
cd cockpit && npm install && npm run dev  # admin console on localhost:5173
```

That is the whole setup. The catalog starts empty — see
[Loading instruments](#loading-instruments) for filling it, and create your
portfolios, accounts and trading identities in the cockpit.

### What each step does

| Step | What happens |
|---|---|
| `database init` | Creates the two roles, the `ods` database, both schemas, all migrations, grants, and reference data (venues, calendars, MICs). |
| `cargo run` | Starts the server. It never creates or migrates anything — if the database is missing or stale, it says so and names the command to run. |

## Prerequisites

- **Rust** (stable) — `curl https://sh.rustup.rs -sSf \| sh`
- **PostgreSQL** running somewhere, with a superuser you know the password of
- **cmake** and a C++ compiler — the embedded FIX engine (`quickfix`) is C++
- **OpenSSL 3** on macOS — `brew install openssl@3` (Linux uses the system one)
- **Node** — only if you want the cockpit web UI

```sh
# macOS
brew install cmake openssl@3 postgresql@16 node
```

## Configuration

Nothing is required. A fresh clone works against a local Postgres with no config at
all. Every setting has a flag, an environment variable, and a default, in that
precedence order:

```sh
cargo run -- database init --host db.internal --username admin
POSTGRES_HOST=db.internal cargo run -- database init
```

| Setting | Flag | Environment | Default |
|---|---|---|---|
| Host | `--host` | `POSTGRES_HOST` | `localhost` |
| Port | `--port` | `POSTGRES_PORT` | `5432` |
| Superuser | `--username` | `POSTGRES_USERNAME` | `postgres` |
| Superuser password | `--password` | `POSTGRES_PASSWORD` | `postgres` |
| Database | `--database` | `POSTGRES_DATABASE` | `ods` |
| Catalog role password | `--mdm-password` | `MDM_MASTER_PASSWORD` | `openoms-dev` |
| Runtime role password | `--oms-password` | `OMS_USER_PASSWORD` | `openoms-dev` |

The superuser is used **only** to provision and tear down. The running server
connects as `oms_user` with the runtime role password.

`.env` is an override file, not a prerequisite — copy `.env.example` when you need
broker credentials, a real admin password, or a non-local database. For any host
that is not loopback, change both role passwords: the server refuses to start with
the built-in default against a remote database.

## Database commands

```sh
cargo run -- database init       # create everything; fails if it already exists
cargo run -- database migrate    # apply pending migrations (idempotent)
cargo run -- database status     # what exists, what is pending
cargo run -- database drop       # destroy the database (roles are kept)
```

`init` is deliberately strict. If the roles or database already exist it stops and
tells you to run `migrate` instead, rather than silently skipping steps or resetting
credentials on a database that already holds data.

Upgrading an existing install is `git pull && cargo run -- database migrate`.

## Loading instruments

The instrument catalog comes from brokers, not from a bundled list. With broker
credentials in `.env`:

```sh
cargo run -- setup sync-broker --broker alpaca
cargo run -- setup sync-broker --broker alpaca --underlyings SPY,QQQ
cargo run -- setup sync-broker --broker alpaca --dry-run
```

## Trading

Two ways in.

**Cockpit** (`localhost:5173`) — configuration, monitoring, minting tokens.

**Python** — for actually sending orders:

```sh
pip install -e clients/python
```

```python
from oms_client import OMS

oms = OMS("http://localhost:3001", token=os.environ["OMS_TRADING_TOKEN"])

pf  = oms.portfolios()[0]
oid = oms.submit(portfolio=pf.portfolio_id,
                 symbol="SPY260918C00770000@OPRA",
                 side="buy", quantity=1)
print(oms.wait_for(oid).status)

for row in oms.orders(status="routed"):
    print(row.order_id, row.instrument_symbol, row.cum_qty)
```

There is a CLI too — `oms orders list`, `oms positions`, `oms submit`. See
[`clients/python/README.md`](clients/python/README.md).

## Auth

- **Cockpit login** — one password, `OMS_ADMIN_PASSWORD` (enable with
  `OMS_ADMIN_AUTH_ENABLED=true`). Sent as a bearer to `/admin`. Use a strong random
  value for anything real.
- **Trading tokens** — minted on the cockpit's *Trading tokens* page or via
  `POST /admin/trading-tokens`, shown once. A token belongs to a **principal** (a
  trader, strategy or service); what it may trade comes from that principal's
  portfolio grants (`can_trade` / `can_view` / `can_allocate`). Mint several tokens
  under one principal to rotate credentials without re-permissioning. Revoke anytime.
- Trading tokens can reach only the trading routes. They can never touch `/admin`.

## Troubleshooting

| Symptom | Cause |
|---|---|
| `password authentication failed for user "oms_user"` | `OMS_USER_PASSWORD` doesn't match the role. Fix the value, or `ALTER ROLE oms_user PASSWORD '…'`. |
| `role "oms_user" already exists` on init | Something is already provisioned. Use `migrate`, or `drop` first. |
| Server exits naming a migration | Run `cargo run -- database migrate`. |
| Link error mentioning `-lssl` on macOS | `brew install openssl@3`, or set `OPENSSL_DIR`. |
