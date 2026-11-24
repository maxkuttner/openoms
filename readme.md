# OpenOMS

<p align="center">
  <img alt="openOMS" src="brand/logo.svg" width="72">
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
oms init     # database (it can start PostgreSQL in Docker), listen address
oms          # starts the OMS on localhost:3001
```

Then open <http://localhost:3001/app/> and create the admin account: on a fresh install the
first account signed up is the admin. Everyone else signs up and waits for your approval, or
you add them on the Users page; an authenticator app and single sign-on with your own identity
provider are optional. See
[Sign-in and users](https://openoms.dev/configuration/login/). The old shared admin password
(`OMS_ADMIN_PASSWORD`, `[server].admin_password`) no longer works.

Prebuilt binaries cover macOS (Apple Silicon) and Linux (x86_64, OpenSSL 3).

## Documentation

**[openoms.dev](https://openoms.dev)**

## Develop

```sh
git clone git@github.com:maxkuttner/openoms.git && cd openoms
docker compose -f compose.dev.yaml up -d      # a local Postgres 16
(cd cockpit && npm ci && npm run build)       # the cockpit bundle the binary embeds
cargo run -- init                             # press Enter through the prompts; password `postgres`
cargo run                                     # the OMS on localhost:3001
```

Needs Rust (stable), cmake and a C++ compiler (the embedded FIX engine), OpenSSL 3
(`brew install openssl@3` on macOS) and Node 22. For the cockpit with hot reload:
`cd cockpit && npm run dev` (localhost:5173).

The documentation site lives in [`docs-site/`](docs-site/) (Starlight):
`cd docs-site && npm ci && npm run dev`.
