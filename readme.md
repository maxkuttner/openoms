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

`~/.local/bin` is not on the default macOS `PATH`. The installer prints the one
`export` line to add when it isn't on yours — add it, then:

```sh
oms database init
oms
```

Then open <http://localhost:3001/cockpit/>. Prebuilt binaries cover macOS on Apple
Silicon and Linux on x86_64; the installer verifies a checksum, drops `oms` in
`~/.local/bin` and does nothing else. `oms database init` needs a Postgres 16, and
the Linux binary needs OpenSSL 3 — Debian 12 and Ubuntu 22.04+ have it, Ubuntu
20.04, RHEL 8 and Amazon Linux 2 do not. Everything below builds from source
instead.

## Setup

```sh
git clone git@github.com:maxkuttner/openoms.git && cd openoms
docker compose up -d  # a Postgres to run against; skip if you already have one
cargo run -- init     # prompts for your Postgres, generates oms.toml, creates the database
cargo run             # start the OMS on localhost:3001
```

The bundled `docker-compose.yml` runs Postgres 16 on `127.0.0.1:5432` with the
superuser `postgres` / `postgres`, which is exactly what `init` assumes — so with it
running you can press Enter through every prompt and type `postgres` for the
password. It deliberately does not pre-create the `ods` database: `init` creates
that itself and refuses if it already exists. Data lives in a named volume, so
`docker compose down` keeps it and `docker compose down -v` throws it away.

Then, in a second terminal:

```sh
cd cockpit && npm install && npm run dev  # admin console on localhost:5173
```

A released `oms` binary serves the cockpit itself at
<http://localhost:3001/cockpit/> — the bundle is compiled in, so there is no second
process to start. The `npm run dev` server above is for developing the cockpit: it
hot-reloads and proxies the API to a running OMS. A source build embeds nothing
unless you run `npm run build` in `cockpit/` before `cargo build`; until then
`/cockpit/` returns a 404 that says so.

`init` asks for your Postgres host, port, database name, superuser name and
password — pressing Enter through every prompt targets a default local Postgres
(`localhost:5432`, superuser `postgres`, database `ods`). A passed `--host`,
`--port`, etc. seeds what Enter takes instead of being ignored, and a passed
`--password` skips that prompt entirely. `init` then generates the `oms` role
password, an encryption master key and a cockpit login password, writes them to
`oms.toml` (mode 0600, and appends the filename to `.gitignore` if one already
exists in the current directory — it does not create a `.gitignore`), and creates
the role, database, schemas, migrations, grants and reference data. The cockpit
password is written to `oms.toml` either way; interactively it is also printed
once at the end, as the only mode where a human is there to read it.

Run it once. If `oms.toml` already exists, `init` refuses and tells you to use
`database migrate` to upgrade, or to delete the file to start over — but deleting it
throws away the master key, and with it anything it decrypts. If provisioning
itself fails partway (after `oms.toml` was written), fix the cause and run `oms
database init --resume` — it creates only whichever of the role/database is
still missing (the common case is the role existing but `CREATE DATABASE`
having failed) and finishes migrations, grants and seeding, all of which are
safe to re-run.

The instrument catalog starts empty. Import broker credentials with `oms config
import-env` and it fills itself on the next boot; see [Loading
instruments](#loading-instruments) and [Broker and feed
credentials](#broker-and-feed-credentials). Create portfolios, accounts and
trading identities in the cockpit.

### What each step does

| Step | What happens |
|---|---|
| `init` | Prompts for the Postgres connection, generates the `oms` role password, the master key and a cockpit login password, writes `oms.toml` (mode 0600), then creates the role, database, schemas, migrations, grants and reference data. `--non-interactive` takes the connection from flags/env instead of prompting. The cockpit password is written to `oms.toml` in both modes; `--non-interactive` just doesn't also print it. |
| `cargo run` | Starts the server. It never creates or migrates anything — if the database is missing or stale, it says so and names the command to run. |

## Prerequisites

- **Rust** (stable) — `curl https://sh.rustup.rs -sSf \| sh`
- **cmake** and a C++ compiler — the embedded FIX engine (`quickfix`) is C++
- **OpenSSL 3** on macOS — `brew install openssl@3` (Linux uses the system one)
- **PostgreSQL** — either Docker, for the bundled `docker-compose.yml`, or an
  existing server with a superuser you know the password of
- **Node** — only if you want the cockpit web UI

```sh
# macOS, using the bundled Postgres
brew install cmake openssl@3 node

# macOS, bringing your own Postgres instead
brew install cmake openssl@3 node postgresql@16
```

## Configuration

Everything lives in `oms.toml`, generated by `oms init`. Flags and environment
variables override it, in that order — that is how Docker and CI inject settings
without touching the file:

```sh
cargo run -- database init --host db.internal --username admin
POSTGRES_HOST=db.internal cargo run -- database init
```

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

Anything left unset at every tier falls back to a built-in default — `localhost`,
`5432`, `postgres`/`postgres`, `ods`, and `openoms-dev` for both the `oms` role and
cockpit passwords (loopback only; a still-default password is refused against
anything else).

There is one application role, `oms`. It owns the database, both schemas and every
table in them, and it is what the server connects as.

**Back up `oms.toml`.** The master key in it is the only thing that can decrypt
stored credentials — losing the file loses them. The superuser password is
deliberately not in it: `oms init` prompts for it once, to provision the database,
and never writes it down. `database init`, `database migrate` and `database drop`
each still take it the same way as before, via `--password`/`POSTGRES_PASSWORD`.
`database status` no longer needs it — it authenticates as the `oms` role instead,
falling back to the superuser whenever the `oms` role connection fails: the server
has never been initialized, or an install from before this fallback existed has an
`oms` role password this command has no way to reconstruct.

`.env` is an override file, not a prerequisite — copy `.env.example` when you need
a real admin password or a non-local database. Broker and feed credentials do not
go here; see [Broker and feed credentials](#broker-and-feed-credentials).

## Broker and feed credentials

Broker and feed API keys live encrypted in Postgres (`oms.broker_connection` and
`oms.feed_connection`), sealed with the master key `oms init` wrote to
`oms.toml`. **The environment is no longer read for them** — `ALPACA_*`,
`BINANCE_*` and `DATABENTO_API_KEY` are not consulted at boot, only `*_ENV`
(which environment to route to), `*_TRANSPORT` and `BINANCE_FEED_WS_URL`.

Get credentials into the store with:

```sh
cargo run -- config import-env
```

a one-shot migration off the old `{BROKER}_{ENV}_*` variables in `.env`: it seals
whatever it finds there into the store and reports what it imported (and what it
skipped, and why — e.g. a key set without its matching secret). Run it once, then
delete the credential lines from `.env`; they do nothing there any more.

**The cockpit's Broker connections and Data feeds pages configure credentials
directly, without going through `.env` at all.** Each connection row has a
"Credentials" button opening a panel that:

- shows whether a credential is `configured`, `unconfigured` ("needs setup"),
  or `error` (stored, but the master key doesn't open it — the panel surfaces
  the reason so it can be fixed rather than mistaken for a fresh install);
- renders one field per credential (host, port, key id, ...) with secret
  fields (API secret, FIX password, private key) shown as password inputs
  that default to blank and are placeholdered "leave blank to keep" once a
  credential is configured — you can change a host or key id without
  re-typing a secret the cockpit never has to show;
- **Test** re-checks the credential currently stored (not unsaved form
  input) against the provider — for Alpaca and Databento this is a real
  authenticated round trip; for IBKR/Binance FIX it reports "not testable
  before save", since the only way to validate a FIX credential is a session
  logon, and this process already owns that session;
- **Save** tests the *submitted* credential (merged over whatever is already
  stored) before writing anything — a failing test is refused with the
  provider's own rejection reason and nothing is persisted or reloaded. FIX
  credentials skip this pre-write test the same way Test does, and save
  regardless;
- **Clear** deletes the stored credential (behind a confirmation) and
  disarms the connection's adapter, same as reload disabling it.

A successful save reports the reload outcome inline — "applied immediately"
for Alpaca and Databento, "restart required" for IBKR/Binance FIX — see
[Applying a credential change](#applying-a-credential-change) below for what
that means operationally.

To change the master key itself, `cargo run -- config rotate-key` re-wraps every
stored credential under a freshly generated key and prints it — the rows are
already re-wrapped by the time the command returns, so the printed key is the
only remaining copy outside the database until you save it into `oms.toml` as
`oms.master_key`. Keep the old key around until that edit is saved.

**Losing `oms.toml` loses every stored credential** — there is no recovery path
that doesn't involve re-entering them. Back it up, and back it up again after
`rotate-key`.

With no master key at all: a fresh install with nothing stored yet still starts
(there is nothing to decrypt). Once any credential has been imported, starting
without a master key — or with one that decrypts *none* of what's stored — is
refused, naming the problem, rather than starting with adapters silently
unregistered. A key that decrypts some rows but not others still starts: one
stale or wrong credential must not be able to disarm every other one — the
unusable rows are logged (`credentials unusable: ...`) so they can be fixed.

### Applying a credential change

After changing a stored broker or feed credential (`import-env` or a later
write endpoint), `POST /admin/connections/reload` applies it — the store is
read once at boot and is not otherwise watched for changes, so this is what
picks up an edit without restarting the process. What "applies" means depends
on the connection:

- **Alpaca and the Databento feed apply immediately.** Both are plain REST/WS
  clients behind a supervised task; the reload builds a fresh one (and, for
  Alpaca, restarts its execution-report stream too) and drops the old one.
- **IBKR and Binance FIX sessions need a process restart.** A FIX session owns
  a thread that parks forever with no stop path, so a second session dialing
  the same venue would collide with the first on logon and sequence numbers.
  The reload leaves the running session exactly as it is and reports
  `RestartRequired` for that connection in the response — trading through it
  is unaffected, it just is not running the new credential yet. This holds for
  a Binance connection even when its transport is REST rather than FIX:
  narrowing that is a follow-up, not done today.

The response names every connection with its outcome (`Registered`,
`RestartRequired`, `Unconfigured`, `Disabled`, or `Failed` with a reason) —
never the credential itself. **A 200 with a mix of outcomes is the normal
case**, not an error: reloading after rotating one Alpaca key while an IBKR
session sits untouched returns 200, `Registered` for one and `RestartRequired`
for the other.

`Registered` means the adapter/task was installed, **not** that the remote
service has authenticated it. Stream health reports the subsequent connection
state. Reload itself does not test credentials before applying them — that
gate lives one step earlier, in `PUT .../credentials` (the save the cockpit's
credential panel calls): it tests the merged submission before writing
anything, and a failing test is refused with the provider's message, with no
write and no reload triggered.

Reloads are serialized from the store read through stream replacement. Both
credential tables are read from one repeatable-read snapshot, and a replacement
waits for the old task to exit before starting. Once accepted, a reload finishes
even if its HTTP client disconnects. Order readers remain lock-free; this is not
an atomic broker-side cutover or a guarantee of uninterrupted streaming during
reconnection. Alpaca's existing reconciliation sweep recovers missed fills.

Disabling a connection and reloading stops its execution stream or feed task
so it stops acting on the old credential. This includes Binance REST execution
streams and deleted connection rows. A FIX session, again, keeps running
regardless until a restart, since there is no way to stop it from inside the
process.

The reload refuses with 500 and changes nothing in three cases: a database
error reading either credential store; a master key that is configured but
does not parse; or every stored credential across both stores failing to
decrypt under the resolved key while nothing at all decoded — the same
refusal boot itself makes on startup, applied here to what the reload just
read. That last case is this endpoint's headline scenario: `rotate-key` run
from another process re-seals every row under a new key while this process
still holds the old one in memory. A key that opens some rows but not others
still reloads — the bad rows are reported `Failed`, and if a broker or the
feed already had a working adapter running, that adapter (or feed task) is
left alone rather than torn down over one unreadable row.

**`config::load()` is memoized for the life of the process** — whatever master
key it resolved at boot is what every reload keeps using, even after
`oms.toml` is edited. This is why a reload right after `rotate-key` is this
endpoint's headline refusal case, not just an edge case: `rotate-key` re-seals
every row under a *new* key from a separate, short-lived process, but this
server is still holding the *old* key in memory, so the reload decrypts
nothing and correctly refuses with 500 rather than swap in an empty registry.
Applying a rotated master key — as opposed to a rotated broker or feed
credential — needs an actual restart; a reload cannot do it, no matter how
promptly `oms.toml` is updated with the new key first.

**Kafka and OpenFIGI stay environment-only** — `KAFKA_BROKER`, `KAFKA_TOPIC`,
`KAFKA_CLIENT_ID`, `KAFKA_PROJECTOR_GROUP_ID` and `OPENFIGI_API_KEY` are plain
`env::var` reads (`src/kafka.rs`, `src/main.rs`), not part of the encrypted
credential store and not configurable from the cockpit. Same shape as broker
and feed credentials, deliberately deferred.

## Roles and schemas

One role, `oms`, created by `database init`. It owns the database, both schemas and
everything in them, and it is what the server connects as — so there is exactly one
password to set. The superuser only creates and destroys it.

```
role  oms

  schema public   instrument, instrument_derivative, broker_instrument,
                  venue, currency, calendar, calendar_holiday
  schema oms      orders, portfolios, principals, accounts, api_keys, …
```

The split is for consumers, not permissions: `public` holds master data another
service can point at, `oms` holds this application's operational tables.

## Database commands

`oms init` runs `database init` for you on a fresh machine. These are the
subcommands underneath it, and what you use directly afterwards — for CI,
non-interactive provisioning, or to inspect and maintain a database you already have:

```sh
cargo run -- database init       # create everything; fails if it already exists
cargo run -- database init --resume   # finish an init that failed partway through
cargo run -- database migrate    # apply pending migrations (idempotent)
cargo run -- database status     # what exists, what is pending
cargo run -- database drop       # destroy the database (roles are kept)
```

`init` is deliberately strict. If the roles or database already exist it stops and
tells you to run `migrate` instead, rather than silently skipping steps or resetting
credentials on a database that already holds data. The one exception is `--resume`:
if a previous `init` created the role and/or database but failed before finishing
migrations, grants or seeding, `oms database init --resume` creates only whichever
of the role/database is still missing — never touching the credentials of one that
already exists — and re-runs the rest, every step of which is safe to re-run. Because
it skips the wrong-server refusal, double-check `--host`/`--database` before passing
it: pointed at the wrong database, resume's migrations (some of which are `DROP …`)
would run there instead.

`init`, `migrate` and `drop` connect as the superuser (`--password`/
`POSTGRES_PASSWORD`, defaulting to `postgres` on loopback) because they create,
alter or destroy the role and the database itself. `status` is read-only and
connects as the `oms` role instead, so it needs no superuser credential in the
normal case; it falls back to the superuser whenever that connection fails — most
commonly because the server has never been initialized, but also for an install
whose `oms` role password predates this fallback and isn't recorded anywhere
`status` can read it.

Upgrading an existing install is `git pull && cargo run -- database migrate`.

## Loading instruments

The instrument catalog comes from brokers, not from a bundled list. There are two
ways in, and they run the same code.

**Automatic.** With broker credentials imported into the store (see [Broker and
feed credentials](#broker-and-feed-credentials)), an empty catalog is populated
in the background on boot. This is the normal path after `database init` and
`oms config import-env` — start the server, and instruments appear. The sync can
take minutes for full option chains.

```sh
# OMS_SYNC_ON_BOOT=never       # opt out entirely
# OMS_SYNC_UNDERLYINGS=SPY,QQQ # only these option chains, instead of all
```

**Explicit**, when you want to re-sync or see what would change:

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

### Enabling login (OIDC)

**Off by default.** With no `[auth.oidc]` block in `oms.toml`, the OMS behaves
exactly as it does today — `/auth/login`, `/auth/callback`, `/auth/logout` and
`/auth/me` all fall through to a plain 404, and nothing else changes.

To turn it on, add an `[auth.oidc]` block (see the sample in `src/config.rs`, or
`oms.toml` after `init`):

```toml
[auth.oidc]
issuer = "https://idp.example.com/realms/oms"
client_id = "oms"
public_base_url = "https://oms.example.com"
```

At the identity provider, register a **confidential client** whose redirect URI is
`{public_base_url}/auth/callback` — the one value above drives both the redirect
sent to the provider and the `Origin` check on the callback, so there is only ever
one URI to get right on both ends.

**The client secret does not go in `oms.toml`.** Set it in the environment as
`OMS_OIDC_CLIENT_SECRET` instead (`.env` works, same as `OMS_ADMIN_PASSWORD`) — it
is read at boot and never written to any file or table.

A misconfigured or unreachable provider — bad issuer, discovery failure, missing
`OMS_OIDC_CLIENT_SECRET` — logs an error and leaves login off for that run; order
routing and the admin console come up regardless. Fix it and restart.

**A non-loopback bind requires `https://`.** If `[auth.oidc]` is configured and
`OMS_BIND_ADDR` is not loopback, `public_base_url` must start with `https://` or
the server refuses to start — the session cookie cannot carry `Secure` otherwise,
which would send it over the wire in clear text.

See [`docker-compose.yml`](docker-compose.yml) for a disposable Keycloak to test
against end to end, under the `oidc` profile.

### The trade app

With `[auth.oidc]` configured, a second, separate single-page app is served at
<http://localhost:3001/trade/> — for traders, as distinct from `/cockpit/`, which
stays the admin console. **It only exists when OIDC is configured**: with no
`[auth.oidc]` block, `/trade/` 404s exactly like `/auth/*` does, and `/cockpit/`
is unaffected either way.

A trader visiting `/trade/` unauthenticated is sent to `/auth/login`, signs in at
the identity provider, and is returned to `/trade/` — not `/cockpit/` or `/`. Once
signed in, the app shows only the portfolios that principal has been granted
(`can_trade` / `can_view` / `can_allocate` from `principal_portfolio_grant`, the
same grants `/auth/me` reports) — never the full portfolio list an admin sees in
the cockpit.

### The desktop shell

`desktop/` is a thin Tauri v2 window around `/trade/` — not a separate client,
just a native shell for it. Build and run it with `cargo tauri dev` /
`cargo tauri build` from `desktop/src-tauri`; it has its own `Cargo.toml` and
`Cargo.lock` and is excluded from the workspace, so it never touches the `oms`
binary build.

On first run it shows a bundled connection page asking for the OMS server
address; once that address is validated and probed, the window navigates to
that server's `/trade/` and remembers the address for next launch. A "Change
server…" menu item comes back to this page.

**Enter the server's canonical public address** — the same origin the OMS is
configured to serve as `public_base_url` — not an IP address or an alternate
DNS name that merely happens to reach it. Login relocates the window to that
canonical origin, and the session's CSRF check is exact string equality, so
connecting through a non-canonical address stores an address that will fail
writes with 403 after every sign-in.

**Login happens inside the webview** — the identity provider's login page
renders in the same window, not a system browser. An IdP that refuses to be
embedded in an iframe/webview will not work here: Keycloak is fine, Google is
not.

**The identity provider must be served over HTTPS**, including in development.
Keycloak issues its login-flow cookies (`AUTH_SESSION_ID`, `KC_RESTART`) with
`SameSite=None`, which forces `Secure`, and the webview discards a `Secure`
cookie delivered over `http://` — login then fails with Keycloak's "Cookie not
found" page. A browser tab does not show this, because Chrome treats
`http://localhost` as a secure context and the webview does not. A plain-HTTP
`start-dev` Keycloak is therefore fine for the cockpit and the browser trade
app, and unusable from the desktop shell.

## Troubleshooting

| Symptom | Cause |
|---|---|
| `refusing to start: OMS_ADMIN_PASSWORD is not set` | `OMS_BIND_ADDR` is not loopback. Set a real admin password in `.env`. |
| `password authentication failed for user "oms"` | `OMS_PASSWORD` doesn't match the role. Fix the value, or `ALTER ROLE oms PASSWORD '…'`. |
| `role "oms" already exists` on init | Something is already provisioned. Use `migrate`, or `drop` first. |
| Server exits naming a migration | Run `cargo run -- database migrate`. |
| Link error mentioning `-lssl` on macOS | `brew install openssl@3`, or set `OPENSSL_DIR`. |
