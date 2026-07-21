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

Prerequisites: **Rust** (cargo), **PostgreSQL**. Optional: **Python 3** (live universe seeders), **Node** (cockpit).

```sh
git clone git@github.com:maxkuttner/openoms.git && cd openoms
cp .env.example .env        # then edit passwords / bind addr
```

## Setup

One-shot DB bootstrap — roles, schema, reference data, and a no-creds SPY fixture (needs the `ADMIN_*` superuser creds in `.env`):

```sh
make db-setup
```

Optional live data (on-demand, needs vendor creds in `.env`):

```sh
make sync-broker BROKER=alpaca               # seed instruments + broker mapping from Alpaca (ALPACA_PAPER_*)
make sync-broker BROKER=alpaca UNDERLYINGS=SPY,QQQ  # also seed those option chains
```

## Run

```sh
cargo run                  # OMS on OMS_BIND_ADDR (default localhost:3001)
```

The SPY fixture seeds `alpaca-paper` + a test user (`test-trader-key` : `test-secret`), so you can place a paper order immediately. Admin webapp: `cd cockpit && npm install && npm run dev`.
