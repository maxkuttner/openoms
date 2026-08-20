# `oms database` — one CLI for setup, no psql, no python

Status: approved design, not yet implemented
Date: 2026-08-15

## Context

Setting up openoms from scratch takes more steps than it should, and the steps
mislead. `make help` lists nine targets, which reads like a nine-step install. It
isn't — boot-time bootstrap already runs provision → migrate → access → seed, so
`cargo run` alone works today. But that leaves two ways to provision the same
database, and neither is obviously the front door.

The friction that actually costs a new user is elsewhere:

* **Four prerequisites**: Rust, a Postgres server, the `psql` client, and Python 3
  (`db/scripts/seed_venues.py` parses the MIC registry).
* **A 22-line `.env` that must be hand-edited before anything runs**, containing at
  least three traps: `DATABASE_URL` is dead (nothing reads it — `main.rs` builds the
  URL from parts), `DB_NAME` and `ODS_DB` are two names for one database, and
  `DB_PASSWORD` must equal `OMS_USER_PASSWORD` with nothing enforcing it.

NautilusTrader solves the same problem with a smaller surface, and is worth copying
because it is the closest comparable system:

* `nautilus database init|drop` — two subcommands (`crates/cli/src/opt.rs`).
* Credentials merge **CLI flag → environment variable → built-in default**
  (`crates/infrastructure/src/sql/pg.rs::get_postgres_connect_options`), with
  defaults that work on localhost.
* `.env` is not the credential mechanism at all — their `.env.example` holds one
  unrelated variable.

**Outcome:** one documented command sets up a fresh clone, with Rust and a Postgres
server as the only prerequisites.

## Goals

1. One unambiguous setup path.
2. Drop the `psql` and `python3` prerequisites.
3. `.env` optional — defaults work on localhost, overridable by flag or env.
4. Failure modes are loud and specific, not silent.

## Non-goals

* **Configurable role names.** `mdm_master` and `oms_user` are written literally into
  all 43 migrations (`SET ROLE mdm_master`) and `db/access/ods.sql`
  (`GRANT … TO oms_user`). Templating those buys nothing while there is one
  deployment.
* **Docker/compose packaging.** Separate question; this spec assumes a Postgres
  server exists.
* **A reuse/force flag on `init`.** See "Partial failure" below.
* **Changing the schema itself.** Migrations are read and executed as-is.

## CLI surface

```
oms database init      create roles + database, migrate, grant, seed
oms database migrate   apply pending migrations (idempotent)
oms database drop      destroy the database (roles kept)
oms database status    what exists, what is applied, what is pending
oms setup sync-broker  unchanged
oms                    serve
```

`init --fixtures` also loads the development seed: `scripts/fixtures/minimal_seed.sql`
(the SPY instrument + `alpaca-paper` mapping) and `dev_identity.sql` (the
`test-trader-key` : `test-secret` principal chain). A flag rather than a phase,
because it is a development convenience, not part of provisioning.

This subsumes `OMS_DEV_IDENTITY`, which currently gates the same `dev_identity.sql`
at boot (`bootstrap::ensure_dev_identity`). Both are removed: a fixture that creates
a known API secret should be requested once, explicitly, at setup — not re-evaluated
on every start from an environment variable.

### `init` is strict; `migrate` is idempotent

`init` **fails** if the roles or the database already exist. This is the core
behavioural decision. A silent no-op hides the most common real mistake — being
pointed at the wrong server — and an automatic `ALTER ROLE … PASSWORD` would let a
bare `init` silently reset a working install's credentials to a shipped default.

```
error: role 'oms_user' already exists on localhost:5432
       database 'ods' already exists (owner mdm_master, 43 migrations applied)

       This database is already initialized. Did you mean:
         oms database migrate    apply pending migrations
         oms database status     show what is there
         oms database drop       destroy it and start over

       If you meant a different server, check POSTGRES_HOST / --host.
```

`migrate` is the repeatable one, and is what an existing install (including the
current dev database) uses from now on.

This also disposes of password rotation without needing a policy: if a role exists,
`init` refuses, so it can neither silently skip nor silently rotate. Changing a
password is a deliberate act — `drop` and recreate, or a later
`oms database set-password --role <name>` if it turns out to be wanted.

**Partial failure.** If `init` creates the roles and then a migration fails, a re-run
errors on the existing roles. The remedy is `drop` then `init`. No `--reuse-existing`
flag: with one user and no production data, an escape hatch is not worth the extra
state to reason about.

## Credential model

Three-tier merge per value, copying Nautilus:

```rust
let host = host                                      // 1. CLI flag
    .or_else(|| std::env::var("POSTGRES_HOST").ok())  // 2. environment
    .unwrap_or(defaults.host);                        // 3. default
```

| Purpose | Flag | Env | Default |
|---|---|---|---|
| Superuser host | `--host` | `POSTGRES_HOST` | `localhost` |
| Superuser port | `--port` | `POSTGRES_PORT` | `5432` |
| Superuser name | `--username` | `POSTGRES_USERNAME` | `postgres` |
| Superuser password | `--password` | `POSTGRES_PASSWORD` | `postgres` |
| Database | `--database` | `POSTGRES_DATABASE` | `ods` |
| Catalog role password | `--mdm-password` | `MDM_MASTER_PASSWORD` | dev default |
| Runtime role password | `--oms-password` | `OMS_USER_PASSWORD` | dev default |

The superuser credentials are used only by `init` and `drop`.

**The runtime pool stops having its own credentials.** It connects as `oms_user`
with `OMS_USER_PASSWORD` to `POSTGRES_DATABASE`, derived from the same values. This
deletes `DB_USER`, `DB_PASSWORD`, `DB_NAME`, `DB_HOST`, `DB_PORT`, `DATABASE_URL`,
and `ODS_DB`, and with them the trap where two variables had to agree by hand.

`.env` keeps only what has no sane default: broker API keys, `OMS_ADMIN_PASSWORD`,
`OMS_BIND_ADDR`, Kafka settings. It becomes an override file, not a prerequisite.

**Default passwords are for localhost only.** Preflight refuses to serve when a role
still has a built-in default password and the host is not loopback.

## Implementation

New module `src/setup/database/`, one file per phase, mirroring the scripts each
replaces:

| File | Replaces | Work |
|---|---|---|
| `config.rs` | — | Three-tier merge; administrator vs runtime variants |
| `provision.rs` | `provision.sh` | Roles + database, as superuser on the `postgres` database |
| `migrate.rs` | `migrate.sh` | Embedded `.sql`, per-schema, tracked |
| `access.rs` | `access.sh` | `db/access/{roles,ods}.sql` |
| `seed.rs` | `seed.sh`, `seed_venues.py` | Currencies, venues (CSV), crypto venues, calendars |
| `mod.rs` | `db-setup` target | `init` / `migrate` / `drop` / `status` orchestration |

### Assets are embedded

Migrations (23 `public`, 20 `oms`), `db/access/*.sql`, the seed SQL, and
`db/data/ISO10383_MIC.csv` (580 KB) are compiled in with `include_dir!`, so a
released binary provisions without a repo checkout. They remain ordinary files in the
tree; only the loading changes.

### Migration runner

Keeps today's semantics exactly, so an already-migrated database sees no change:

* Same tracking table, `public._mdm_migrations (target, filename)` — the 43 applied
  rows are honoured and nothing re-runs.
* Two targets with distinct owners: `public` → `mdm_master`, `oms` → `oms_user`.
* Each file in one transaction: `SET ROLE <owner>; SET search_path TO <schema>;`,
  the file's SQL, then the tracking insert.

`SET ROLE` is plain SQL and sqlx's simple query protocol runs multi-statement
strings, so `\i` becomes reading an embedded file. Verified: no migration uses
`CREATE INDEX CONCURRENTLY`, `VACUUM`, or `CREATE DATABASE`, so all 43 are
transaction-safe.

### Provisioning

`\gexec` was doing conditional DDL. In Rust that is an existence query followed by a
statement:

```
SELECT 1 FROM pg_roles    WHERE rolname = $1
SELECT 1 FROM pg_database WHERE datname = $1
```

`CREATE DATABASE` cannot run inside a transaction, so each statement is issued
standalone.

### Venue seeding

`seed_venues.py` parsed `ISO10383_MIC.csv` and loaded it via `\copy`. In Rust: parse
the CSV, then `COPY` through sqlx (`copy_in_raw`) into a temp table and upsert, which
is what the Python already did in SQL.

## Removals

| Removed | Replacement |
|---|---|
| `make db-provision/db-migrate/db-access/db-seed/db-fixtures/db-setup/db-reset` | `oms database init` / `migrate` / `drop` |
| `db/scripts/{provision,migrate,access,seed,fixtures}.sh` | `src/setup/database/` |
| `db/scripts/seed_venues.py` | `seed.rs` |
| `bootstrap::ensure_ready` + `run_script`, `OMS_BOOTSTRAP`, `OMS_DB_SCRIPTS_DIR` | nothing — `init` is explicit |
| `bootstrap::ensure_fixture_if_no_brokers` | `init --fixtures` |
| `bootstrap::ensure_dev_identity`, `OMS_DEV_IDENTITY` | `init --fixtures` |
| `psql`, `python3` prerequisites | — |

**`bootstrap.rs` is not deleted wholesale.** Two of its functions are runtime
concerns, not provisioning, and stay exactly as they are:

* `ensure_broker_connections` — creates a `broker_connection` row for each broker
  whose credentials are present. Depends on runtime environment, runs as `oms_user`
  after the pool connects, and a credentialed broker without one cannot take an
  order.
* `spawn_sync` — background instrument catalog sync, after the server is listening.

`serve()` loses only its provisioning step. Boot becomes: connect → preflight →
serve. When the database is missing or unmigrated, preflight fails with a message
naming `oms database init`.

`make` is deleted. Every target either moves into `oms database` or is a one-word
alias for a cargo command, and keeping a Makefile whose only job is to alias
`cargo run` reintroduces the ambiguity this spec exists to remove. The README
documents `cargo run -- database init` directly.

## Migration path for the existing dev database

It is already initialized, so `init` will correctly refuse. The sequence is:

1. `.env` continues to override the new defaults, so the existing role passwords keep
   working. Rename its DB keys to `POSTGRES_*`.
2. `oms database status` — expect 43 applied, 0 pending.
3. `oms database migrate` from then on.

`init` only matters for fresh clones.

## Risks

1. **Silent auth failure on renamed keys.** If `.env` still says `DB_PASSWORD` after
   the rename, the runtime falls back to the default `OMS_USER_PASSWORD` and fails to
   connect. Mitigation: on startup, error explicitly when a removed key
   (`DB_PASSWORD`, `DATABASE_URL`, `ODS_DB`, …) is present, naming its replacement.
2. **`cargo run` on a fresh clone no longer works alone.** Accepted, and the point:
   one path instead of two. Preflight must say exactly what to run.
3. **Default passwords escaping localhost.** Mitigated by the loopback check above.

## Verification

1. `cargo build`, `cargo test` clean.
2. Unit tests, matching the repo's inline `#[cfg(test)]` style over pure functions:
   flag → env → default precedence; migration discovery and ordering from the
   embedded directory; removed-key detection.
3. CI: add a Postgres service container to `.github/workflows/build.yml`, then
   * `oms database init` on an empty server — expect success,
   * `oms database init` again — expect the strict error and a non-zero exit,
   * `oms database migrate` twice — expect success both times, 0 applied the second,
   * `oms database status` — expect 43 applied, 0 pending.

   (Nautilus does the equivalent in `scripts/ci/test-postgres-bootstrap.bash`.)
4. Against a scratch database: `init --fixtures`, boot, confirm preflight is clean
   and a paper order can be placed with the fixture credentials.
5. Against the existing dev database: `status` reports 43/0 and `migrate` is a no-op —
   proving the tracking table stayed compatible.
6. Confirm `psql` and `python3` are absent from the install path: grep the tree for
   invocations, and check the README prerequisites list is down to Rust + Postgres.
7. Confirm the surviving runtime helpers still fire: boot with broker credentials
   present and check `broker_connection` rows are created as before.
