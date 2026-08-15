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

## Install

Prerequisites: **Rust** (cargo) and a running **PostgreSQL**. Optional: **Node** (cockpit).

```sh
git clone git@github.com:maxkuttner/openoms.git && cd openoms
cargo run -- database init --fixtures
cargo run
```

That is the whole setup. `database init` creates the roles, database, schema,
grants and reference data; `--fixtures` adds the SPY instrument and a dev trading
identity (`test-trader-key` : `test-secret`) so you can place a paper order
immediately.

Defaults assume Postgres on `localhost:5432` with superuser `postgres`. Override
per command or through the environment:

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

`.env` is optional — copy `.env.example` when you need broker credentials or an
admin password. The server refuses to start with the default role password against
a non-loopback host.

## Database commands

```sh
cargo run -- database init       # create everything; fails if it already exists
cargo run -- database migrate    # apply pending migrations (idempotent)
cargo run -- database status     # what exists, what is pending
cargo run -- database drop       # destroy the database (roles are kept)
```

`init` is deliberately strict: if the roles or database already exist it stops and
tells you to use `migrate` instead, rather than silently skipping or resetting
credentials.

## Run

```sh
cargo run                  # OMS on OMS_BIND_ADDR (default localhost:3001)
```

Starting the server never creates or migrates anything. If the database is missing
or out of date, it says so and names the command to run.

Instruments come from brokers:

```sh
cargo run -- setup sync-broker --broker alpaca
cargo run -- setup sync-broker --broker alpaca --underlyings SPY,QQQ
```

Admin webapp: `cd cockpit && npm install && npm run dev`.

## Auth

- **Cockpit login** — the console is gated by a single password: `OMS_ADMIN_TOKEN`
  (enabled via `OMS_ADMIN_AUTH_ENABLED=true`). Enter it on the login screen; it's sent
  as a bearer to `/admin`. Set a strong random value for any real deployment.
- **Trading tokens** — generate one on the cockpit's *Trading tokens* page (or
  `POST /admin/trading-tokens`). A token belongs to a **principal** (a trader /
  strategy / service) and is a single copy-once bearer string used by API clients as
  `Authorization: Bearer <token>`. What it can trade comes from the principal's
  portfolio grants (`can_trade`), so you can mint several tokens under one principal
  to rotate credentials without re-permissioning. Revoke any token anytime.
- The legacy HTTP Basic form (`key_id:secret`, e.g. the `test-trader` dev user) still
  works on the trading routes.
