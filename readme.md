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
oms init     # database (it can start PostgreSQL in Docker), first admin, listen address
oms          # starts the OMS on localhost:3001
```

Then open <http://localhost:3001/cockpit/> and sign in as the admin you created: `oms init`
prints a one-time link to choose a password and set up an authenticator app. Everyone else
is invited from the Users page; single sign-on with your own identity provider is optional.
See [Sign-in and users](https://openoms.dev/configuration/login/). The old shared admin
password (`OMS_ADMIN_PASSWORD`, `[server].admin_password`) is deprecated and only for
upgrades.

Prebuilt binaries cover macOS (Apple Silicon) and Linux (x86_64, OpenSSL 3).

## Documentation

**[openoms.dev](https://openoms.dev)**

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
