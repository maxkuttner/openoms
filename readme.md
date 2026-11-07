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

---

## Install

```sh
curl -fsSL https://openoms.dev/install.sh | sh
oms init     # database (it can start PostgreSQL in Docker), admin password, listen address
oms          # starts the OMS on localhost:3001
```

Then open <http://localhost:3001/cockpit/>. Prebuilt binaries cover macOS (Apple
Silicon) and Linux (x86_64, OpenSSL 3).

## Documentation

**[openoms.dev](https://openoms.dev/)**

- [Getting started](https://openoms.dev/getting-started/): install, `oms init`, first login
- [Your first paper trade](https://openoms.dev/getting-started/first-paper-trade/) — Alpaca paper, end to end
- [Your first API call](https://openoms.dev/getting-started/first-api-call/) — curl and the Python client
- [Configuration](https://openoms.dev/configuration/reference/) — `oms.toml`, environment variables, [login (OIDC)](https://openoms.dev/configuration/login/)
- [Integrations](https://openoms.dev/integrations/alpaca/) — Alpaca, Binance, IBKR, Bybit, Databento
- [Operations](https://openoms.dev/operations/updating/) — updating, running as a service, database, backups, [troubleshooting](https://openoms.dev/operations/troubleshooting/)
- [API reference](https://openoms.dev/api/)

## Develop

```sh
git clone git@github.com:maxkuttner/openoms.git && cd openoms
docker compose up -d                          # a local Postgres 16
(cd cockpit && npm ci && npm run build)       # the cockpit bundle the binary embeds
cargo run -- init                             # press Enter through the prompts; password `postgres`
cargo run                                     # the OMS on localhost:3001
```

Needs Rust (stable), cmake and a C++ compiler (the embedded FIX engine), OpenSSL 3
(`brew install openssl@3` on macOS) and Node 22. For the cockpit with hot reload:
`cd cockpit && npm run dev` (localhost:5173).

The documentation site lives in [`docs-site/`](docs-site/) (Starlight):
`cd docs-site && npm ci && npm run dev`.
