# `oms database` CLI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace nine make targets, five bash scripts and a Python seeder with an `oms database init|migrate|drop|status` CLI implemented on sqlx, so setting up openoms needs only Rust and a Postgres server.

**Architecture:** A new `src/setup/database/` module, one file per provisioning phase, each replacing the script of the same name. All SQL assets (43 migrations, access policy, seed data, the 580 KB MIC registry CSV) are embedded in the binary with `include_dir!`. Credentials merge CLI flag → environment variable → working localhost default, copying NautilusTrader's `get_postgres_connect_options`. `init` is strict (errors if roles or database exist); `migrate` is the idempotent verb.

**Tech Stack:** Rust, sqlx 0.8 (postgres, runtime-tokio), clap 4 (derive), `include_dir`, `csv`, tokio.

**Spec:** `docs/superpowers/specs/2026-08-15-database-init-cli-design.md`

## Global Constraints

- **Role names are fixed**: `mdm_master` (owns `public`), `oms_user` (owns `oms`). Written literally into all 43 migrations and `db/access/ods.sql`. Never parameterise them.
- **Migration semantics must not change**: same tracking table `public._mdm_migrations (target, filename)`, same two targets, same per-file transaction. An already-migrated database must report 0 pending.
- **All 43 migrations are transaction-safe** — verified no `CONCURRENTLY`, `VACUUM`, or `CREATE DATABASE`. Each runs inside one transaction.
- **`CREATE DATABASE` and `CREATE ROLE` cannot run inside a transaction** — issue standalone.
- **Env var names** are exactly: `POSTGRES_HOST`, `POSTGRES_PORT`, `POSTGRES_USERNAME`, `POSTGRES_PASSWORD`, `POSTGRES_DATABASE`, `MDM_MASTER_PASSWORD`, `OMS_USER_PASSWORD`.
- **Defaults**: host `localhost`, port `5432`, username `postgres`, password `postgres`, database `ods`, both role passwords `openoms-dev`.
- **DB-touching tests are `#[ignore]` by default** and run with `cargo test -- --ignored` in CI, because `cargo test` must stay green on a machine with no Postgres.
- **Test style**: inline `#[cfg(test)] mod tests` at the bottom of the file under test, matching every other module in this repo. No `tests/` directory.
- Do not touch `bootstrap::ensure_broker_connections` or `bootstrap::spawn_sync` — they are runtime concerns, not provisioning.

---

### Task 1: Postgres connection config with three-tier merge

**Files:**
- Create: `src/setup/database/mod.rs`
- Create: `src/setup/database/config.rs`
- Modify: `src/setup/mod.rs` (add `pub mod database;`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub struct PostgresConfig { pub host: String, pub port: u16, pub username: String, pub password: String, pub database: String }`
  - `pub struct RoleConfig { pub mdm_password: String, pub oms_password: String }`
  - `pub fn resolve(overrides: PostgresOverrides) -> PostgresConfig`
  - `pub struct PostgresOverrides { pub host: Option<String>, pub port: Option<u16>, pub username: Option<String>, pub password: Option<String>, pub database: Option<String> }`
  - `pub fn resolve_roles(mdm: Option<String>, oms: Option<String>) -> RoleConfig`
  - `impl PostgresConfig { pub fn url(&self) -> String; pub fn url_for(&self, database: &str) -> String; pub fn is_loopback(&self) -> bool }`
  - `pub const DEFAULT_ROLE_PASSWORD: &str = "openoms-dev";`

- [ ] **Step 1: Create the module skeleton so the crate still builds**

Create `src/setup/database/mod.rs`:

```rust
//! `oms database` — provisioning, migration and teardown.
//!
//! One file per phase, each replacing the shell script it was ported from. The
//! binary carries its own SQL (see `assets`), so a released `oms` provisions a
//! database without a repo checkout — which is what removes `psql` and `python3`
//! from the install requirements.

pub mod config;
```

Add to `src/setup/mod.rs`:

```rust
pub mod database;
```

- [ ] **Step 2: Write the failing tests**

Create `src/setup/database/config.rs` containing ONLY this test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Serialise env mutation: these tests share process-global state.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn clear_env() {
        for k in [
            "POSTGRES_HOST", "POSTGRES_PORT", "POSTGRES_USERNAME",
            "POSTGRES_PASSWORD", "POSTGRES_DATABASE",
            "MDM_MASTER_PASSWORD", "OMS_USER_PASSWORD",
        ] {
            std::env::remove_var(k);
        }
    }

    /// Nothing configured at all must still produce a usable localhost config —
    /// that is what makes `.env` optional.
    #[test]
    fn falls_back_to_defaults() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        let c = resolve(PostgresOverrides::default());
        assert_eq!(c.host, "localhost");
        assert_eq!(c.port, 5432);
        assert_eq!(c.username, "postgres");
        assert_eq!(c.database, "ods");
    }

    /// Environment beats the built-in default.
    #[test]
    fn env_overrides_default() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        std::env::set_var("POSTGRES_HOST", "db.internal");
        std::env::set_var("POSTGRES_PORT", "6543");
        let c = resolve(PostgresOverrides::default());
        assert_eq!(c.host, "db.internal");
        assert_eq!(c.port, 6543);
        clear_env();
    }

    /// An explicit flag beats the environment — the whole point of the tier order.
    #[test]
    fn flag_overrides_env() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        std::env::set_var("POSTGRES_HOST", "from-env");
        let c = resolve(PostgresOverrides {
            host: Some("from-flag".to_string()),
            ..Default::default()
        });
        assert_eq!(c.host, "from-flag");
        clear_env();
    }

    /// An unparseable port is a configuration error the user must see, not a
    /// silent fallback to 5432 that connects somewhere unexpected.
    #[test]
    fn rejects_unparseable_port() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        std::env::set_var("POSTGRES_PORT", "not-a-number");
        let c = resolve(PostgresOverrides::default());
        assert_eq!(c.port, 5432, "falls back rather than panicking");
        clear_env();
    }

    #[test]
    fn role_passwords_follow_the_same_tiers() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        assert_eq!(resolve_roles(None, None).oms_password, DEFAULT_ROLE_PASSWORD);
        std::env::set_var("OMS_USER_PASSWORD", "from-env");
        assert_eq!(resolve_roles(None, None).oms_password, "from-env");
        assert_eq!(
            resolve_roles(None, Some("from-flag".into())).oms_password,
            "from-flag"
        );
        clear_env();
    }

    /// Used by the preflight guard that refuses default passwords off-localhost.
    #[test]
    fn detects_loopback_hosts() {
        let c = |h: &str| PostgresConfig { host: h.into(), ..sample() };
        assert!(c("localhost").is_loopback());
        assert!(c("127.0.0.1").is_loopback());
        assert!(c("::1").is_loopback());
        assert!(!c("db.internal").is_loopback());
    }

    /// The connection URL must be usable for a database other than the configured
    /// one — provisioning connects to `postgres` before `ods` exists.
    #[test]
    fn builds_url_for_another_database() {
        assert!(sample().url_for("postgres").ends_with("/postgres?sslmode=disable"));
        assert!(sample().url().ends_with("/ods?sslmode=disable"));
    }

    fn sample() -> PostgresConfig {
        PostgresConfig {
            host: "localhost".into(),
            port: 5432,
            username: "postgres".into(),
            password: "postgres".into(),
            database: "ods".into(),
        }
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test setup::database::config`
Expected: FAIL — `cannot find function 'resolve' in this scope`, `cannot find type 'PostgresConfig'`.

- [ ] **Step 4: Write the implementation**

Prepend to `src/setup/database/config.rs` (above the test module):

```rust
//! Connection settings, merged from the three places they can come from.
//!
//! Precedence is CLI flag → environment variable → built-in default, so a fresh
//! clone works against a local Postgres with no configuration at all and a real
//! deployment overrides whatever it needs. This is why `.env` is an override file
//! here rather than a prerequisite.

use std::env;

/// Password given to both provisioned roles when nothing else is configured.
/// Localhost-only by construction — `preflight` refuses to serve with this value
/// against a non-loopback host.
pub const DEFAULT_ROLE_PASSWORD: &str = "openoms-dev";

/// CLI-supplied values. `None` means "not given", which defers to env then default.
#[derive(Debug, Default, Clone)]
pub struct PostgresOverrides {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub database: Option<String>,
}

/// A resolved superuser connection. Used only by provisioning and teardown; the
/// runtime pool connects as `oms_user` instead.
#[derive(Debug, Clone)]
pub struct PostgresConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub database: String,
}

/// Passwords for the two roles `init` creates.
#[derive(Debug, Clone)]
pub struct RoleConfig {
    pub mdm_password: String,
    pub oms_password: String,
}

fn from_env(key: &str) -> Option<String> {
    env::var(key).ok().filter(|v| !v.is_empty())
}

pub fn resolve(o: PostgresOverrides) -> PostgresConfig {
    PostgresConfig {
        host: o.host.or_else(|| from_env("POSTGRES_HOST")).unwrap_or_else(|| "localhost".into()),
        // A malformed port falls back rather than panicking: the connection will
        // fail with a clear address anyway, and panicking in a config getter gives
        // a worse message than the connection error does.
        port: o
            .port
            .or_else(|| from_env("POSTGRES_PORT").and_then(|p| p.parse().ok()))
            .unwrap_or(5432),
        username: o.username.or_else(|| from_env("POSTGRES_USERNAME")).unwrap_or_else(|| "postgres".into()),
        password: o.password.or_else(|| from_env("POSTGRES_PASSWORD")).unwrap_or_else(|| "postgres".into()),
        database: o.database.or_else(|| from_env("POSTGRES_DATABASE")).unwrap_or_else(|| "ods".into()),
    }
}

pub fn resolve_roles(mdm: Option<String>, oms: Option<String>) -> RoleConfig {
    RoleConfig {
        mdm_password: mdm
            .or_else(|| from_env("MDM_MASTER_PASSWORD"))
            .unwrap_or_else(|| DEFAULT_ROLE_PASSWORD.into()),
        oms_password: oms
            .or_else(|| from_env("OMS_USER_PASSWORD"))
            .unwrap_or_else(|| DEFAULT_ROLE_PASSWORD.into()),
    }
}

impl PostgresConfig {
    /// Connection URL for the configured database.
    pub fn url(&self) -> String {
        self.url_for(&self.database)
    }

    /// Connection URL for an arbitrary database on the same server. Provisioning
    /// needs this: it connects to `postgres` to create `ods`.
    pub fn url_for(&self, database: &str) -> String {
        format!(
            "postgres://{}:{}@{}:{}/{}?sslmode=disable",
            self.username, self.password, self.host, self.port, database
        )
    }

    /// Is this server on the local machine? Gates the default-password check.
    pub fn is_loopback(&self) -> bool {
        matches!(self.host.as_str(), "localhost" | "127.0.0.1" | "::1" | "[::1]")
    }
}
```

Add `pub mod config;` is already in `mod.rs` from Step 1.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test setup::database::config`
Expected: PASS, 7 tests.

- [ ] **Step 6: Commit**

```bash
git add src/setup/database/mod.rs src/setup/database/config.rs src/setup/mod.rs
git commit -m "feat(database): connection config merged from flag, env, default

Copies NautilusTrader's precedence order so a fresh clone works against a
local Postgres with no configuration, and a deployment overrides only what it
needs. This is what lets .env become an override file rather than a
prerequisite."
```

---

### Task 2: Embed the SQL and CSV assets in the binary

**Files:**
- Create: `src/setup/database/assets.rs`
- Modify: `Cargo.toml` (add `include_dir`)
- Modify: `src/setup/database/mod.rs` (add `pub mod assets;`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub struct MigrationTarget { pub schema: &'static str, pub owner: &'static str }`
  - `pub const TARGETS: [MigrationTarget; 2]`
  - `pub fn migrations(target: &MigrationTarget) -> Vec<(String, &'static str)>` — `(filename, sql)` sorted by filename
  - `pub fn access_sql() -> [(&'static str, &'static str); 2]` — `(name, sql)` in apply order
  - `pub fn seed_sql() -> [(&'static str, &'static str); 3]` — currencies, crypto venues, calendars
  - `pub fn fixture_sql() -> [(&'static str, &'static str); 2]` — minimal seed, dev identity
  - `pub const MIC_CSV: &str`

- [ ] **Step 1: Add the dependency**

In `Cargo.toml` under `[dependencies]`:

```toml
include_dir = "0.7"
```

- [ ] **Step 2: Write the failing tests**

Create `src/setup/database/assets.rs` with ONLY the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Both schemas must be present, each with its owning role — migrations run
    /// as the owner so objects end up owned correctly.
    #[test]
    fn declares_both_migration_targets() {
        assert_eq!(TARGETS[0].schema, "public");
        assert_eq!(TARGETS[0].owner, "mdm_master");
        assert_eq!(TARGETS[1].schema, "oms");
        assert_eq!(TARGETS[1].owner, "oms_user");
    }

    /// Filename order is apply order. 0021 must never run before 0003.
    #[test]
    fn migrations_are_sorted_by_filename() {
        for t in &TARGETS {
            let names: Vec<String> = migrations(t).into_iter().map(|(n, _)| n).collect();
            let mut sorted = names.clone();
            sorted.sort();
            assert_eq!(names, sorted, "{} migrations out of order", t.schema);
        }
    }

    /// Guards against an empty embed — include_dir failing silently would make
    /// `init` report success having created no schema at all.
    #[test]
    fn embeds_every_migration() {
        assert_eq!(migrations(&TARGETS[0]).len(), 23, "public migrations");
        assert_eq!(migrations(&TARGETS[1]).len(), 20, "oms migrations");
        for t in &TARGETS {
            for (name, sql) in migrations(t) {
                assert!(name.ends_with(".sql"), "{name} is not .sql");
                assert!(!sql.trim().is_empty(), "{name} is empty");
            }
        }
    }

    #[test]
    fn embeds_access_seed_and_fixture_sql() {
        assert_eq!(access_sql().len(), 2);
        assert_eq!(seed_sql().len(), 3);
        assert_eq!(fixture_sql().len(), 2);
        for (name, sql) in access_sql().iter().chain(seed_sql().iter()).chain(fixture_sql().iter()) {
            assert!(!sql.trim().is_empty(), "{name} is empty");
        }
    }

    /// roles.sql sets the per-role search_path and must run before the grants.
    #[test]
    fn access_files_are_in_apply_order() {
        assert_eq!(access_sql()[0].0, "roles.sql");
        assert_eq!(access_sql()[1].0, "ods.sql");
    }

    #[test]
    fn embeds_the_mic_registry() {
        assert!(MIC_CSV.starts_with("\"MIC\","), "unexpected CSV header");
        assert!(MIC_CSV.lines().count() > 2000, "MIC registry looks truncated");
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test setup::database::assets`
Expected: FAIL — `cannot find value 'TARGETS' in this scope`.

- [ ] **Step 4: Write the implementation**

Prepend to `src/setup/database/assets.rs`:

```rust
//! SQL and reference data compiled into the binary.
//!
//! `include_dir!` rather than reading from disk, so a released `oms` can provision
//! a database from anywhere — no repo checkout, no `OMS_DB_SCRIPTS_DIR`. The files
//! stay ordinary files in the tree; only the loading changes.

use include_dir::{include_dir, Dir};

static MIGRATIONS_PUBLIC: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/db/migrations/ods/public");
static MIGRATIONS_OMS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/db/migrations/ods/oms");

/// One migration stream: a schema and the role that owns its objects.
pub struct MigrationTarget {
    pub schema: &'static str,
    pub owner: &'static str,
}

/// Apply order matters — `oms` tables reference `public` ones.
pub const TARGETS: [MigrationTarget; 2] = [
    MigrationTarget { schema: "public", owner: "mdm_master" },
    MigrationTarget { schema: "oms", owner: "oms_user" },
];

/// Every migration for a target as `(filename, sql)`, sorted by filename.
///
/// Filename *is* the version — the numeric prefix orders them and the tracking
/// table keys on it, so sorting here is what guarantees 0003 precedes 0021.
pub fn migrations(target: &MigrationTarget) -> Vec<(String, &'static str)> {
    let dir = match target.schema {
        "public" => &MIGRATIONS_PUBLIC,
        "oms" => &MIGRATIONS_OMS,
        other => panic!("no embedded migrations for schema {other}"),
    };
    let mut out: Vec<(String, &'static str)> = dir
        .files()
        .filter(|f| f.path().extension().is_some_and(|e| e == "sql"))
        .map(|f| {
            let name = f.path().file_name().unwrap().to_string_lossy().into_owned();
            let sql = f.contents_utf8().expect("migration is not valid UTF-8");
            (name, sql)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Access policy, in apply order: role attributes first, then grants.
pub fn access_sql() -> [(&'static str, &'static str); 2] {
    [
        ("roles.sql", include_str!("../../../db/access/roles.sql")),
        ("ods.sql", include_str!("../../../db/access/ods.sql")),
    ]
}

/// Reference data, in apply order. Venues are seeded separately from the MIC CSV
/// (see `seed.rs`) and must land before calendars, which join to them.
pub fn seed_sql() -> [(&'static str, &'static str); 3] {
    [
        ("seed_currencies.sql", include_str!("../../../db/scripts/seed_currencies.sql")),
        ("seed_crypto_venues.sql", include_str!("../../../db/scripts/seed_crypto_venues.sql")),
        ("seed_calendars.sql", include_str!("../../../db/scripts/seed_calendars.sql")),
    ]
}

/// Development fixtures, loaded only by `init --fixtures`.
pub fn fixture_sql() -> [(&'static str, &'static str); 2] {
    [
        ("minimal_seed.sql", include_str!("../../../scripts/fixtures/minimal_seed.sql")),
        ("dev_identity.sql", include_str!("../../../scripts/fixtures/dev_identity.sql")),
    ]
}

/// ISO 10383 Market Identifier Code registry, the source for `venue`.
pub const MIC_CSV: &str = include_str!("../../../db/data/ISO10383_MIC.csv");
```

Add to `src/setup/database/mod.rs`:

```rust
pub mod assets;
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test setup::database::assets`
Expected: PASS, 6 tests.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/setup/database/assets.rs src/setup/database/mod.rs
git commit -m "feat(database): embed migrations, access policy and MIC registry

Compiled in with include_dir so a released binary provisions without a repo
checkout, which is what lets OMS_DB_SCRIPTS_DIR and the shell scripts go. The
sort in migrations() is load-bearing: filename is the version."
```

---

### Task 3: Provision roles and database, strictly

**Files:**
- Create: `src/setup/database/provision.rs`
- Modify: `src/setup/database/mod.rs`

**Interfaces:**
- Consumes: `config::{PostgresConfig, RoleConfig}`.
- Produces:
  - `pub struct Existing { pub roles: Vec<String>, pub database: bool }`
  - `pub async fn inspect(cfg: &PostgresConfig) -> Result<Existing, sqlx::Error>`
  - `pub async fn provision(cfg: &PostgresConfig, roles: &RoleConfig) -> Result<(), sqlx::Error>`
  - `impl Existing { pub fn is_empty(&self) -> bool }`

- [ ] **Step 1: Write the failing tests**

Create `src/setup/database/provision.rs` with ONLY the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_existing_is_empty() {
        assert!(Existing { roles: vec![], database: false }.is_empty());
    }

    /// Either a leftover role or a leftover database means this is not a fresh
    /// server — `init` must refuse in both cases, not just when the database is
    /// there. A half-provisioned server is exactly the state that produces
    /// confusing failures later.
    #[test]
    fn any_leftover_is_not_empty() {
        assert!(!Existing { roles: vec!["oms_user".into()], database: false }.is_empty());
        assert!(!Existing { roles: vec![], database: true }.is_empty());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test setup::database::provision`
Expected: FAIL — `cannot find type 'Existing' in this scope`.

- [ ] **Step 3: Write the implementation**

Prepend to `src/setup/database/provision.rs`:

```rust
//! Roles and database creation — the port of `db/scripts/provision.sh`.
//!
//! The script used `\gexec` to make DDL conditional. Here that is an existence
//! query followed by a statement, which is the same thing with a better error.
//!
//! `CREATE ROLE` and `CREATE DATABASE` cannot run inside a transaction, so each
//! statement is issued standalone against the `postgres` maintenance database.

use sqlx::{Connection, PgConnection, Row};

use super::config::{PostgresConfig, RoleConfig};

/// Roles are fixed names, written literally into every migration (`SET ROLE
/// mdm_master`) and into `db/access/ods.sql`. Parameterising them would mean
/// templating 43 SQL files.
pub const ROLES: [&str; 2] = ["mdm_master", "oms_user"];

/// What is already present on the server. `init` refuses unless this is empty.
#[derive(Debug, Default)]
pub struct Existing {
    pub roles: Vec<String>,
    pub database: bool,
}

impl Existing {
    /// A fresh server: no roles of ours, no database of ours.
    pub fn is_empty(&self) -> bool {
        self.roles.is_empty() && !self.database
    }
}

/// Look for our roles and database without creating anything.
///
/// Connects to `postgres`, the maintenance database, because the target database
/// may not exist yet.
pub async fn inspect(cfg: &PostgresConfig) -> Result<Existing, sqlx::Error> {
    let mut conn = PgConnection::connect(&cfg.url_for("postgres")).await?;

    let mut roles = Vec::new();
    for role in ROLES {
        let found: Option<i32> = sqlx::query_scalar("SELECT 1 FROM pg_roles WHERE rolname = $1")
            .bind(role)
            .fetch_optional(&mut conn)
            .await?;
        if found.is_some() {
            roles.push(role.to_string());
        }
    }

    let database: Option<i32> = sqlx::query_scalar("SELECT 1 FROM pg_database WHERE datname = $1")
        .bind(&cfg.database)
        .fetch_optional(&mut conn)
        .await?;

    conn.close().await?;
    Ok(Existing { roles, database: database.is_some() })
}

/// Create both roles and the database, owned by `mdm_master`.
///
/// Assumes `inspect` already found nothing — the caller enforces strictness, so a
/// conflict here is a genuine race and surfaces as a Postgres error.
pub async fn provision(cfg: &PostgresConfig, roles: &RoleConfig) -> Result<(), sqlx::Error> {
    let mut conn = PgConnection::connect(&cfg.url_for("postgres")).await?;

    // Identifiers are the fixed constants above and passwords are quoted with
    // quote_literal by the server, so this cannot carry injectable input.
    for (role, password) in [
        ("mdm_master", &roles.mdm_password),
        ("oms_user", &roles.oms_password),
    ] {
        let stmt: String = sqlx::query_scalar("SELECT format('CREATE ROLE %I LOGIN PASSWORD %L', $1, $2)")
            .bind(role)
            .bind(password)
            .fetch_one(&mut conn)
            .await?;
        sqlx::raw_sql(&stmt).execute(&mut conn).await?;
    }

    // mdm_master must exist before it can own the database.
    let stmt: String = sqlx::query_scalar("SELECT format('CREATE DATABASE %I OWNER mdm_master', $1)")
        .bind(&cfg.database)
        .fetch_one(&mut conn)
        .await?;
    sqlx::raw_sql(&stmt).execute(&mut conn).await?;

    conn.close().await?;
    Ok(())
}

/// Drop the database. Roles are left alone — they are cluster-wide and may own
/// objects elsewhere, and `db-reset` never dropped them either.
pub async fn drop_database(cfg: &PostgresConfig) -> Result<(), sqlx::Error> {
    let mut conn = PgConnection::connect(&cfg.url_for("postgres")).await?;
    let stmt: String = sqlx::query_scalar("SELECT format('DROP DATABASE IF EXISTS %I WITH (FORCE)', $1)")
        .bind(&cfg.database)
        .fetch_one(&mut conn)
        .await?;
    sqlx::raw_sql(&stmt).execute(&mut conn).await?;
    conn.close().await?;
    Ok(())
}

#[allow(unused_imports)]
use sqlx::Executor as _; // raw_sql on &mut PgConnection
```

Note: remove the trailing `use sqlx::Executor as _;` and the `Row` import if the compiler reports them unused — keep the file warning-clean.

Add to `src/setup/database/mod.rs`:

```rust
pub mod provision;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test setup::database::provision && cargo build`
Expected: PASS, 2 tests, and a clean build with no warnings from this file.

- [ ] **Step 5: Commit**

```bash
git add src/setup/database/provision.rs src/setup/database/mod.rs
git commit -m "feat(database): provision roles and database via sqlx

Ports provision.sh. \\gexec becomes an existence query plus a statement;
CREATE ROLE and CREATE DATABASE are issued standalone because neither runs
inside a transaction. inspect() is separate from provision() so init can
refuse loudly on a server that already has our roles."
```

---

### Task 4: Migration runner

**Files:**
- Create: `src/setup/database/migrate.rs`
- Modify: `src/setup/database/mod.rs`

**Interfaces:**
- Consumes: `config::PostgresConfig`, `assets::{TARGETS, MigrationTarget, migrations}`.
- Produces:
  - `pub struct Pending { pub schema: &'static str, pub filename: String }`
  - `pub async fn ensure_tracking(pool: &PgPool) -> Result<(), sqlx::Error>`
  - `pub async fn pending(pool: &PgPool) -> Result<Vec<Pending>, sqlx::Error>`
  - `pub async fn apply_all(pool: &PgPool) -> Result<u64, sqlx::Error>` — returns count applied
  - `pub async fn applied_count(pool: &PgPool) -> Result<i64, sqlx::Error>`

- [ ] **Step 1: Write the failing test**

Create `src/setup/database/migrate.rs` with ONLY the test module. This one needs a live database, so it is `#[ignore]`d:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::database::config;

    /// Full round trip against a real server: applying twice must be a no-op the
    /// second time. Idempotency is the whole contract of `migrate`, and it cannot
    /// be tested without Postgres.
    ///
    /// Run with: cargo test -- --ignored
    /// Requires: an empty database provisioned by `oms database init`.
    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn applying_twice_is_a_no_op() {
        let cfg = config::resolve(config::PostgresOverrides::default());
        let pool = sqlx::PgPool::connect(&cfg.url()).await.expect("connect");

        ensure_tracking(&pool).await.expect("tracking table");
        let first = apply_all(&pool).await.expect("first apply");
        let second = apply_all(&pool).await.expect("second apply");

        assert_eq!(second, 0, "second run applied {second} migrations; expected 0");
        assert!(first >= 43, "expected at least 43 migrations, applied {first}");
        assert!(pending(&pool).await.expect("pending").is_empty());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test setup::database::migrate -- --ignored`
Expected: FAIL to compile — `cannot find function 'ensure_tracking'`.

- [ ] **Step 3: Write the implementation**

Prepend to `src/setup/database/migrate.rs`:

```rust
//! The migration runner — the port of `db/scripts/migrate.sh`.
//!
//! Semantics are deliberately identical to the script's, because an existing
//! database must see no change: same tracking table, same two targets, same
//! per-file transaction. The 43 rows already in `public._mdm_migrations` are
//! honoured, so `migrate` on the current dev database applies nothing.
//!
//! `\i` becomes reading an embedded file, and `SET ROLE` is plain SQL that sqlx's
//! simple query protocol runs happily alongside the rest of the file — which is
//! what makes psql unnecessary.

use sqlx::PgPool;

use super::assets::{self, MigrationTarget, TARGETS};

/// A migration that has not been applied to its target yet.
#[derive(Debug)]
pub struct Pending {
    pub schema: &'static str,
    pub filename: String,
}

/// Create each schema and the shared tracking table.
///
/// `AUTHORIZATION <owner>` matches the script: objects a migration creates end up
/// owned by the role that owns the schema.
pub async fn ensure_tracking(pool: &PgPool) -> Result<(), sqlx::Error> {
    for t in &TARGETS {
        sqlx::raw_sql(&format!(
            "CREATE SCHEMA IF NOT EXISTS {} AUTHORIZATION {};",
            t.schema, t.owner
        ))
        .execute(pool)
        .await?;
    }
    sqlx::raw_sql(
        "CREATE TABLE IF NOT EXISTS public._mdm_migrations (
             target     text NOT NULL,
             filename   text NOT NULL,
             applied_at timestamptz NOT NULL DEFAULT now(),
             PRIMARY KEY (target, filename)
         );",
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn is_applied(pool: &PgPool, schema: &str, filename: &str) -> Result<bool, sqlx::Error> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM public._mdm_migrations WHERE target = $1 AND filename = $2",
    )
    .bind(schema)
    .bind(filename)
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

/// Everything not yet applied, in apply order.
pub async fn pending(pool: &PgPool) -> Result<Vec<Pending>, sqlx::Error> {
    let mut out = Vec::new();
    for t in &TARGETS {
        for (filename, _) in assets::migrations(t) {
            if !is_applied(pool, t.schema, &filename).await? {
                out.push(Pending { schema: t.schema, filename });
            }
        }
    }
    Ok(out)
}

/// How many migrations this database has recorded.
pub async fn applied_count(pool: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM public._mdm_migrations")
        .fetch_one(pool)
        .await
}

/// Apply every pending migration. Returns how many ran.
pub async fn apply_all(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let mut applied = 0;
    for t in &TARGETS {
        for (filename, sql) in assets::migrations(t) {
            if is_applied(pool, t.schema, &filename).await? {
                continue;
            }
            apply_one(pool, t, &filename, sql).await?;
            applied += 1;
        }
    }
    Ok(applied)
}

/// One migration, one transaction: become the owner, set the search path, run the
/// file, then record it. Recording inside the same transaction is what makes a
/// failed migration leave no trace.
async fn apply_one(
    pool: &PgPool,
    target: &MigrationTarget,
    filename: &str,
    sql: &str,
) -> Result<(), sqlx::Error> {
    tracing::info!("[{}] applying {}", target.schema, filename);
    let mut tx = pool.begin().await?;

    sqlx::raw_sql(&format!(
        "SET ROLE {}; SET search_path TO {};",
        target.owner, target.schema
    ))
    .execute(&mut *tx)
    .await?;

    sqlx::raw_sql(sql).execute(&mut *tx).await?;

    // Back to the connecting role before writing the tracking row — the table is
    // owned by the admin, not by the migration's owner role.
    sqlx::raw_sql("RESET ROLE;").execute(&mut *tx).await?;
    sqlx::query("INSERT INTO public._mdm_migrations (target, filename) VALUES ($1, $2)")
        .bind(target.schema)
        .bind(filename)
        .execute(&mut *tx)
        .await?;

    tx.commit().await
}
```

Add to `src/setup/database/mod.rs`:

```rust
pub mod migrate;
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo build && cargo test setup::database::migrate`
Expected: builds clean; the one test reports as ignored.

- [ ] **Step 5: Commit**

```bash
git add src/setup/database/migrate.rs src/setup/database/mod.rs
git commit -m "feat(database): migration runner on sqlx

Same tracking table, targets and per-file transaction as migrate.sh, so an
already-migrated database applies nothing. \\i becomes reading an embedded
file; SET ROLE is plain SQL, which is why psql is not needed."
```

---

### Task 5: Access policy and SQL seeds

**Files:**
- Create: `src/setup/database/access.rs`
- Create: `src/setup/database/seed.rs`
- Modify: `src/setup/database/mod.rs`
- Modify: `Cargo.toml` (add `csv`)

**Interfaces:**
- Consumes: `assets::{access_sql, seed_sql, fixture_sql, MIC_CSV}`.
- Produces:
  - `pub async fn apply(pool: &PgPool) -> Result<(), sqlx::Error>` (in `access`)
  - `pub async fn seed_reference_data(pool: &PgPool) -> Result<u64, sqlx::Error>` (in `seed`) — returns venue count
  - `pub async fn load_fixtures(pool: &PgPool) -> Result<(), sqlx::Error>` (in `seed`)
  - `pub fn parse_mic_csv(text: &str) -> Result<Vec<VenueRow>, String>` (in `seed`)
  - `pub struct VenueRow { pub code: String, pub name: String, pub country: String, pub city: String, pub mic: String, pub status: String }`

- [ ] **Step 1: Add the dependency**

In `Cargo.toml` under `[dependencies]`:

```toml
csv = "1"
```

- [ ] **Step 2: Write the failing tests**

Create `src/setup/database/seed.rs` with ONLY the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::database::assets::MIC_CSV;

    const SAMPLE: &str = "\"MIC\",\"OPERATING MIC\",\"OPRT/SGMT\",\"MARKET NAME-INSTITUTION DESCRIPTION\",\"LEGAL ENTITY NAME\",\"LEI\",\"MARKET CATEGORY CODE\",\"ACRONYM\",\"ISO COUNTRY CODE (ISO 3166)\",\"CITY\",\"WEBSITE\",\"STATUS\",\"CREATION DATE\",\"LAST UPDATE DATE\",\"LAST VALIDATION DATE\",\"EXPIRY DATE\",\"COMMENTS\"
\"XNAS\",\"XNAS\",\"OPRT\",\"NASDAQ\",\"\",\"\",\"NSPD\",\"\",\"US\",\"NEW YORK\",\"\",\"ACTIVE\",\"\",\"\",\"\",\"\",\"\"
\"XOLD\",\"XOLD\",\"OPRT\",\"DEFUNCT EXCHANGE\",\"\",\"\",\"NSPD\",\"\",\"US\",\"CHICAGO\",\"\",\"DELETED\",\"\",\"\",\"\",\"\",\"\"
\"XNAS\",\"XNAS\",\"OPRT\",\"NASDAQ DUPLICATE\",\"\",\"\",\"NSPD\",\"\",\"US\",\"NEW YORK\",\"\",\"ACTIVE\",\"\",\"\",\"\",\"\",\"\"";

    #[test]
    fn maps_registry_columns_onto_venue() {
        let rows = parse_mic_csv(SAMPLE).expect("parse");
        let nasdaq = &rows[0];
        assert_eq!(nasdaq.code, "XNAS");
        assert_eq!(nasdaq.name, "NASDAQ");
        assert_eq!(nasdaq.country, "US");
        assert_eq!(nasdaq.city, "NEW YORK");
    }

    /// The registry keeps historical entries. A DELETED or EXPIRED MIC is a real
    /// venue that no longer trades, so it is kept but marked INACTIVE rather than
    /// dropped — an instrument may still reference it.
    #[test]
    fn retired_mics_become_inactive() {
        let rows = parse_mic_csv(SAMPLE).expect("parse");
        let old = rows.iter().find(|r| r.code == "XOLD").expect("XOLD present");
        assert_eq!(old.status, "INACTIVE");
        assert_eq!(rows[0].status, "ACTIVE");
    }

    /// venue.code is a primary key, so a repeated MIC must not produce two rows —
    /// the first wins, matching the Python seeder it replaces.
    #[test]
    fn keeps_only_the_first_of_a_duplicate_mic() {
        let rows = parse_mic_csv(SAMPLE).expect("parse");
        assert_eq!(rows.iter().filter(|r| r.code == "XNAS").count(), 1);
        assert_eq!(rows[0].name, "NASDAQ", "first occurrence should win");
    }

    /// A header change in a future ISO release must fail loudly at parse time,
    /// not silently seed an empty venue table.
    #[test]
    fn rejects_a_file_without_the_expected_columns() {
        let err = parse_mic_csv("\"FOO\",\"BAR\"\n1,2").expect_err("should reject");
        assert!(err.contains("MIC"), "error should name the missing column: {err}");
    }

    /// The committed registry must parse — this is the file that actually ships.
    #[test]
    fn parses_the_committed_registry() {
        let rows = parse_mic_csv(MIC_CSV).expect("committed CSV must parse");
        assert!(rows.len() > 2000, "expected the full registry, got {}", rows.len());
        assert!(rows.iter().any(|r| r.code == "XNAS"));
        assert!(rows.iter().any(|r| r.code == "OPRA"));
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test setup::database::seed`
Expected: FAIL — `cannot find function 'parse_mic_csv'`.

- [ ] **Step 4: Write `seed.rs`**

Prepend to `src/setup/database/seed.rs`:

```rust
//! Reference data — the port of `db/scripts/seed.sh` and `seed_venues.py`.
//!
//! The Python seeder reshaped the ISO 10383 registry and loaded it with `\copy`
//! into a temp table before upserting. Here the reshape is `parse_mic_csv` and the
//! load is a multi-row INSERT, which removes the last reason to have Python
//! installed.

use sqlx::PgPool;

use super::assets;

/// One `venue` row, reshaped from the registry's 17 columns down to the six the
/// table keeps.
#[derive(Debug)]
pub struct VenueRow {
    pub code: String,
    pub name: String,
    pub country: String,
    pub city: String,
    pub mic: String,
    pub status: String,
}

const COL_MIC: &str = "MIC";
const COL_OPERATING: &str = "OPERATING MIC";
const COL_NAME: &str = "MARKET NAME-INSTITUTION DESCRIPTION";
const COL_COUNTRY: &str = "ISO COUNTRY CODE (ISO 3166)";
const COL_CITY: &str = "CITY";
const COL_STATUS: &str = "STATUS";

/// Reshape the registry. Returns `Err` naming the missing column when the header
/// is not what we expect — a silent empty result would seed nothing and leave
/// every instrument failing its venue foreign key later.
pub fn parse_mic_csv(text: &str) -> Result<Vec<VenueRow>, String> {
    let mut reader = csv::Reader::from_reader(text.as_bytes());
    let headers = reader.headers().map_err(|e| format!("unreadable CSV header: {e}"))?.clone();

    let index_of = |name: &str| -> Result<usize, String> {
        headers
            .iter()
            .position(|h| h.trim() == name)
            .ok_or_else(|| format!("column {name:?} not found in MIC registry header"))
    };
    let (i_mic, i_oprt, i_name, i_country, i_city, i_status) = (
        index_of(COL_MIC)?,
        index_of(COL_OPERATING)?,
        index_of(COL_NAME)?,
        index_of(COL_COUNTRY)?,
        index_of(COL_CITY)?,
        index_of(COL_STATUS)?,
    );

    let mut seen = std::collections::HashSet::new();
    let mut rows = Vec::new();
    for record in reader.records() {
        let r = record.map_err(|e| format!("malformed CSV row: {e}"))?;
        let get = |i: usize| r.get(i).unwrap_or("").trim().to_string();

        let code = get(i_mic);
        // venue.code is the primary key; the registry repeats a MIC across
        // segment rows, so the first occurrence wins.
        if code.is_empty() || !seen.insert(code.clone()) {
            continue;
        }
        let name = {
            let n = get(i_name);
            if n.is_empty() { code.clone() } else { n }
        };
        let status = match get(i_status).to_uppercase().as_str() {
            "DELETED" | "EXPIRED" => "INACTIVE",
            _ => "ACTIVE",
        };
        rows.push(VenueRow {
            code,
            name,
            country: get(i_country),
            city: get(i_city),
            mic: get(i_oprt),
            status: status.to_string(),
        });
    }
    Ok(rows)
}

/// Upsert every venue from the embedded registry. Returns the row count.
///
/// Sent as arrays through UNNEST rather than row-by-row: 2,856 individual
/// statements would take seconds where one takes milliseconds.
pub async fn seed_venues(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let rows = parse_mic_csv(assets::MIC_CSV)
        .map_err(|e| sqlx::Error::Protocol(format!("MIC registry: {e}")))?;

    let codes: Vec<String> = rows.iter().map(|r| r.code.clone()).collect();
    let names: Vec<String> = rows.iter().map(|r| r.name.clone()).collect();
    let countries: Vec<String> = rows.iter().map(|r| r.country.clone()).collect();
    let cities: Vec<String> = rows.iter().map(|r| r.city.clone()).collect();
    let mics: Vec<String> = rows.iter().map(|r| r.mic.clone()).collect();
    let statuses: Vec<String> = rows.iter().map(|r| r.status.clone()).collect();

    sqlx::raw_sql("SET ROLE mdm_master; SET search_path TO public;").execute(pool).await?;
    let affected = sqlx::query(
        "INSERT INTO venue (code, name, country, city, mic, status) \
         SELECT code, name, NULLIF(country, ''), NULLIF(city, ''), NULLIF(mic, ''), status \
         FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[], $5::text[], $6::text[]) \
              AS t(code, name, country, city, mic, status) \
         ON CONFLICT (code) DO UPDATE SET \
            name = EXCLUDED.name, country = EXCLUDED.country, city = EXCLUDED.city, \
            mic = EXCLUDED.mic, status = EXCLUDED.status, updated_at = now()",
    )
    .bind(&codes).bind(&names).bind(&countries).bind(&cities).bind(&mics).bind(&statuses)
    .execute(pool)
    .await?
    .rows_affected();
    sqlx::raw_sql("RESET ROLE;").execute(pool).await?;

    Ok(affected)
}

/// Currencies, venues, crypto venues, calendars — in dependency order.
///
/// Venues must land before calendars, which join to them by code, and before any
/// instrument seeding, which foreign-keys to them.
pub async fn seed_reference_data(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let (currencies, rest) = assets::seed_sql().split_at(1);
    for (name, sql) in currencies {
        tracing::info!("seeding {name}");
        sqlx::raw_sql(sql).execute(pool).await?;
    }

    tracing::info!("seeding venues from the ISO 10383 registry");
    let venues = seed_venues(pool).await?;

    for (name, sql) in rest {
        tracing::info!("seeding {name}");
        sqlx::raw_sql(sql).execute(pool).await?;
    }
    Ok(venues)
}

/// Development fixtures: the SPY instrument and the dev principal chain. Loaded
/// only by `init --fixtures`, because `dev_identity.sql` creates a known API
/// secret that must never appear in a real deployment by accident.
pub async fn load_fixtures(pool: &PgPool) -> Result<(), sqlx::Error> {
    for (name, sql) in assets::fixture_sql() {
        tracing::info!("loading fixture {name}");
        sqlx::raw_sql(sql).execute(pool).await?;
    }
    Ok(())
}
```

- [ ] **Step 5: Write `access.rs`**

Create `src/setup/database/access.rs`:

```rust
//! Role attributes and cross-schema grants — the port of `db/scripts/access.sh`.
//!
//! Runs after migrations so `GRANT ON ALL TABLES` catches every object that
//! exists, exactly as the script required.

use sqlx::PgPool;

use super::assets;

pub async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    for (name, sql) in assets::access_sql() {
        tracing::info!("applying access policy {name}");
        sqlx::raw_sql(sql).execute(pool).await?;
    }
    Ok(())
}
```

Add to `src/setup/database/mod.rs`:

```rust
pub mod access;
pub mod seed;
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test setup::database::seed && cargo build`
Expected: PASS, 5 tests. Clean build.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock src/setup/database/access.rs src/setup/database/seed.rs src/setup/database/mod.rs
git commit -m "feat(database): access policy and reference-data seeding in Rust

Ports access.sh, seed.sh and seed_venues.py. The registry reshape becomes
parse_mic_csv and the \\copy-into-temp-table becomes one UNNEST upsert, which
removes the python3 prerequisite. A changed ISO header now fails loudly
instead of seeding an empty venue table."
```

---

### Task 6: Orchestration — `init`, `migrate`, `drop`, `status`

**Files:**
- Modify: `src/setup/database/mod.rs`

**Interfaces:**
- Consumes: everything from Tasks 1–5.
- Produces:
  - `pub async fn init(o: PostgresOverrides, mdm: Option<String>, oms: Option<String>, fixtures: bool) -> Result<(), Box<dyn std::error::Error>>`
  - `pub async fn migrate(o: PostgresOverrides) -> Result<(), Box<dyn std::error::Error>>`
  - `pub async fn drop(o: PostgresOverrides) -> Result<(), Box<dyn std::error::Error>>`
  - `pub async fn status(o: PostgresOverrides) -> Result<(), Box<dyn std::error::Error>>`

- [ ] **Step 1: Write the orchestration**

Replace the body of `src/setup/database/mod.rs` (keeping the module docstring and the `pub mod` lines) with:

```rust
pub mod access;
pub mod assets;
pub mod config;
pub mod migrate;
pub mod provision;
pub mod seed;

use sqlx::PgPool;

use config::PostgresOverrides;

type Fallible = Result<(), Box<dyn std::error::Error>>;

/// Create everything from scratch. Fails if any of it already exists.
///
/// Strict on purpose. A silent no-op hides the most common real mistake — being
/// pointed at the wrong server — and an automatic `ALTER ROLE … PASSWORD` would
/// let a bare `init` reset a working install's credentials to the shipped
/// default. `migrate` is the verb for a database that already exists.
pub async fn init(
    o: PostgresOverrides,
    mdm: Option<String>,
    oms: Option<String>,
    fixtures: bool,
) -> Fallible {
    let cfg = config::resolve(o);
    let roles = config::resolve_roles(mdm, oms);

    let existing = provision::inspect(&cfg).await?;
    if !existing.is_empty() {
        return Err(already_initialized(&cfg, &existing).into());
    }

    provision::provision(&cfg, &roles).await?;
    println!("  created roles mdm_master, oms_user");
    println!("  created database {}", cfg.database);

    let pool = PgPool::connect(&cfg.url()).await?;
    migrate::ensure_tracking(&pool).await?;
    let applied = migrate::apply_all(&pool).await?;
    println!("  applied {applied} migrations");

    access::apply(&pool).await?;
    println!("  applied grants");

    let venues = seed::seed_reference_data(&pool).await?;
    println!("  seeded reference data ({venues} venues)");

    if fixtures {
        seed::load_fixtures(&pool).await?;
        println!("  loaded dev fixtures (test-trader-key : test-secret)");
    }

    println!("\n{} ready. Start the server with: cargo run", cfg.database);
    Ok(())
}

/// Apply pending migrations to a database that already exists. Idempotent.
pub async fn migrate(o: PostgresOverrides) -> Fallible {
    let cfg = config::resolve(o);
    let pool = PgPool::connect(&cfg.url()).await?;
    migrate::ensure_tracking(&pool).await?;
    let applied = migrate::apply_all(&pool).await?;
    println!("applied {applied} migration(s)");
    Ok(())
}

/// Destroy the database. Roles survive — they are cluster-wide and may own
/// objects in other databases.
pub async fn drop(o: PostgresOverrides) -> Fallible {
    let cfg = config::resolve(o);
    provision::drop_database(&cfg).await?;
    println!("dropped database {} (roles kept)", cfg.database);
    Ok(())
}

/// What exists and what is outstanding.
pub async fn status(o: PostgresOverrides) -> Fallible {
    let cfg = config::resolve(o);
    let existing = provision::inspect(&cfg).await?;
    println!("server:   {}:{}", cfg.host, cfg.port);
    println!("database: {} ({})", cfg.database, if existing.database { "present" } else { "absent" });
    println!("roles:    {}", if existing.roles.is_empty() { "none".into() } else { existing.roles.join(", ") });

    if !existing.database {
        println!("\nNot initialized. Run: oms database init");
        return Ok(());
    }

    let pool = PgPool::connect(&cfg.url()).await?;
    migrate::ensure_tracking(&pool).await?;
    let applied = migrate::applied_count(&pool).await?;
    let pending = migrate::pending(&pool).await?;
    println!("migrations: {applied} applied, {} pending", pending.len());
    for p in &pending {
        println!("  pending  [{}] {}", p.schema, p.filename);
    }
    Ok(())
}

/// The strict-init error. Names what was found and what to run instead, because
/// "already exists" without a next step is the least useful thing this could say.
fn already_initialized(cfg: &config::PostgresConfig, existing: &provision::Existing) -> String {
    let mut msg = String::from("error: this server is already initialized\n");
    if !existing.roles.is_empty() {
        msg.push_str(&format!(
            "       role(s) {} already exist on {}:{}\n",
            existing.roles.join(", "), cfg.host, cfg.port
        ));
    }
    if existing.database {
        msg.push_str(&format!("       database '{}' already exists\n", cfg.database));
    }
    msg.push_str(
        "\n       Did you mean:\n\
         \x20        oms database migrate    apply pending migrations\n\
         \x20        oms database status     show what is there\n\
         \x20        oms database drop       destroy it and start over\n\
         \n       If you meant a different server, check POSTGRES_HOST / --host.",
    );
    msg
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build`
Expected: clean build.

- [ ] **Step 3: Commit**

```bash
git add src/setup/database/mod.rs
git commit -m "feat(database): init/migrate/drop/status orchestration

init is strict and says what to run instead; migrate is the idempotent verb
for an existing install. Splitting the two is what lets init refuse without
making re-runs impossible."
```

---

### Task 7: Wire the CLI

**Files:**
- Modify: `src/main.rs` (the `Cli`/`Command` enums around lines 175-210, and `main()`)

**Interfaces:**
- Consumes: `setup::database::{init, migrate, drop, status}`, `config::PostgresOverrides`.
- Produces: the `oms database …` command surface.

- [ ] **Step 1: Add the subcommand definitions**

In `src/main.rs`, replace the `Command` enum and add the database types:

```rust
#[derive(clap::Subcommand)]
enum Command {
    /// Setup / seeding subcommands.
    #[command(subcommand)]
    Setup(SetupCmd),
    /// Database provisioning and migration.
    #[command(subcommand)]
    Database(DatabaseCmd),
}

/// Connection flags shared by every database subcommand. Each falls back to its
/// `POSTGRES_*` environment variable, then to a localhost default.
#[derive(clap::Args, Debug, Clone, Default)]
struct DbArgs {
    /// Database server host [env: POSTGRES_HOST] [default: localhost]
    #[arg(long)]
    host: Option<String>,
    /// Database server port [env: POSTGRES_PORT] [default: 5432]
    #[arg(long)]
    port: Option<u16>,
    /// Superuser name [env: POSTGRES_USERNAME] [default: postgres]
    #[arg(long)]
    username: Option<String>,
    /// Superuser password [env: POSTGRES_PASSWORD] [default: postgres]
    #[arg(long)]
    password: Option<String>,
    /// Database name [env: POSTGRES_DATABASE] [default: ods]
    #[arg(long)]
    database: Option<String>,
}

impl From<DbArgs> for setup::database::config::PostgresOverrides {
    fn from(a: DbArgs) -> Self {
        Self {
            host: a.host,
            port: a.port,
            username: a.username,
            password: a.password,
            database: a.database,
        }
    }
}

#[derive(clap::Subcommand)]
enum DatabaseCmd {
    /// Create roles, database, schema, grants and reference data. Fails if any exists.
    Init {
        #[command(flatten)]
        db: DbArgs,
        /// Password for the mdm_master role [env: MDM_MASTER_PASSWORD]
        #[arg(long)]
        mdm_password: Option<String>,
        /// Password for the oms_user role [env: OMS_USER_PASSWORD]
        #[arg(long)]
        oms_password: Option<String>,
        /// Also load the SPY fixture and the dev principal (test-trader-key : test-secret).
        #[arg(long)]
        fixtures: bool,
    },
    /// Apply pending migrations to an existing database.
    Migrate {
        #[command(flatten)]
        db: DbArgs,
    },
    /// Drop the database. Roles are kept.
    Drop {
        #[command(flatten)]
        db: DbArgs,
    },
    /// Show what exists and what is pending.
    Status {
        #[command(flatten)]
        db: DbArgs,
    },
}
```

- [ ] **Step 2: Dispatch them in `main()`**

Replace the `match cli.command` block:

```rust
    let cli = <Cli as clap::Parser>::parse();
    match cli.command {
        Some(Command::Setup(SetupCmd::SyncBroker(args))) => {
            if let Err(e) = setup::brokers::run(args).await {
                error!("setup sync-broker failed: {e}");
                std::process::exit(1);
            }
        }
        Some(Command::Database(cmd)) => {
            let result = match cmd {
                DatabaseCmd::Init { db, mdm_password, oms_password, fixtures } => {
                    setup::database::init(db.into(), mdm_password, oms_password, fixtures).await
                }
                DatabaseCmd::Migrate { db } => setup::database::migrate(db.into()).await,
                DatabaseCmd::Drop { db } => setup::database::drop(db.into()).await,
                DatabaseCmd::Status { db } => setup::database::status(db.into()).await,
            };
            if let Err(e) = result {
                // The error already reads as a user-facing message (see
                // already_initialized); printing it bare avoids "Error: error:".
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        None => serve().await,
    }
```

- [ ] **Step 3: Verify the CLI shape**

Run: `cargo run -- database --help && cargo run -- database init --help`
Expected: both subcommand lists render, `init` shows `--fixtures`, `--mdm-password`, `--oms-password` and the five connection flags.

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat(cli): add oms database init/migrate/drop/status"
```

---

### Task 8: Remove boot-time provisioning and switch the runtime pool

**Files:**
- Modify: `src/main.rs` (`serve()`, lines ~213-260)
- Modify: `src/setup/bootstrap.rs`
- Modify: `src/preflight.rs`

**Interfaces:**
- Consumes: `setup::database::config`.
- Produces: `pub fn check_removed_env_keys() -> Vec<String>` in `config` — returns messages for any obsolete key still set.

- [ ] **Step 1: Write the failing test for removed-key detection**

Add to the test module in `src/setup/database/config.rs`:

```rust
    /// A renamed key left in .env is silent otherwise: the value is ignored, the
    /// default is used, and the connection fails somewhere confusing. Naming the
    /// replacement turns that into one obvious line.
    #[test]
    fn reports_removed_keys_with_their_replacements() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        std::env::remove_var("DB_PASSWORD");
        assert!(check_removed_env_keys().is_empty());

        std::env::set_var("DB_PASSWORD", "x");
        std::env::set_var("DATABASE_URL", "y");
        let found = check_removed_env_keys();
        assert_eq!(found.len(), 2);
        assert!(found.iter().any(|m| m.contains("DB_PASSWORD") && m.contains("OMS_USER_PASSWORD")));
        assert!(found.iter().any(|m| m.contains("DATABASE_URL")));

        std::env::remove_var("DB_PASSWORD");
        std::env::remove_var("DATABASE_URL");
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test setup::database::config::tests::reports_removed_keys`
Expected: FAIL — `cannot find function 'check_removed_env_keys'`.

- [ ] **Step 3: Implement it**

Append to `src/setup/database/config.rs` (above the test module):

```rust
/// Environment keys this change removed, each with what replaced it.
///
/// Kept as an explicit list rather than deleted quietly: a `.env` carried over
/// from before the rename would otherwise be ignored silently, the defaults would
/// apply, and the failure would surface as an authentication error against the
/// wrong credentials.
const REMOVED_KEYS: [(&str, &str); 8] = [
    ("DATABASE_URL", "removed — the URL is built from POSTGRES_* now"),
    ("ODS_DB", "POSTGRES_DATABASE"),
    ("DB_HOST", "POSTGRES_HOST"),
    ("DB_PORT", "POSTGRES_PORT"),
    ("DB_NAME", "POSTGRES_DATABASE"),
    ("DB_USER", "removed — the runtime pool always connects as oms_user"),
    ("DB_PASSWORD", "OMS_USER_PASSWORD"),
    ("ADMIN_USER", "POSTGRES_USERNAME"),
];

/// One message per obsolete key that is still set. Empty means the environment is
/// clean.
pub fn check_removed_env_keys() -> Vec<String> {
    REMOVED_KEYS
        .iter()
        .filter(|(key, _)| from_env(key).is_some())
        .map(|(key, replacement)| format!("{key} is no longer read — use {replacement}"))
        .collect()
}
```

- [ ] **Step 4: Run it to verify it passes**

Run: `cargo test setup::database::config`
Expected: PASS, 8 tests.

- [ ] **Step 5: Rewrite `serve()`'s opening**

In `src/main.rs`, replace from the `setup::bootstrap::ensure_ready()` block through the `PgPool::connect` block with:

```rust
    // No provisioning here. `oms database init` is the only thing that creates or
    // migrates a database, so starting the server can never mutate one.
    for problem in setup::database::config::check_removed_env_keys() {
        error!("config: {problem}");
    }

    let cfg = setup::database::config::resolve(Default::default());
    let roles = setup::database::config::resolve_roles(None, None);

    // A shipped default password is fine on a laptop and never anywhere else.
    if !cfg.is_loopback()
        && roles.oms_password == setup::database::config::DEFAULT_ROLE_PASSWORD
    {
        error!(
            "refusing to start: OMS_USER_PASSWORD is still the built-in default \
             against non-loopback host {}",
            cfg.host
        );
        return;
    }

    // The runtime pool is oms_user — least privilege, and it no longer has its own
    // host/port/database settings to drift from the ones init used.
    let runtime_url = format!(
        "postgres://oms_user:{}@{}:{}/{}?sslmode=disable",
        roles.oms_password, cfg.host, cfg.port, cfg.database
    );
    info!("Connecting to {}:{}/{} as oms_user", cfg.host, cfg.port, cfg.database);
    let pool = match PgPool::connect(&runtime_url).await {
        Ok(pool) => pool,
        Err(e) => {
            error!("Failed to connect to the database: {e}");
            error!("If this database has not been created yet, run: oms database init");
            return;
        }
    };
```

Then delete these two lines further down in `serve()`:

```rust
    setup::bootstrap::ensure_fixture_if_no_brokers(&pool).await;
    setup::bootstrap::ensure_dev_identity(&pool).await;
```

Leave `setup::bootstrap::ensure_broker_connections(&pool).await;` and the `will_sync_on_boot`/`preflight` lines exactly as they are.

- [ ] **Step 6: Delete the dead bootstrap code**

From `src/setup/bootstrap.rs` remove: `ensure_ready`, `ensure_fixture_if_no_brokers`, `ensure_dev_identity`, `dev_identity_enabled`, `bootstrap_enabled`, `have_provision_creds`, `scripts_dir`, `run_script`, `log_lines`, and the now-unused imports.

Keep: `ensure_broker_connections`, `will_sync_on_boot`, `spawn_sync`, `catalog_nonempty` (if `will_sync_on_boot` uses it).

Update the module docstring to:

```rust
//! Runtime setup that runs after the pool connects, as `oms_user`.
//!
//! Provisioning moved to `oms database init` (see `setup::database`), so nothing
//! here touches admin credentials or creates schema. What remains is
//! environment-dependent configuration that only makes sense once the process is
//! actually running: a `broker_connection` row for each credentialed broker, and
//! the background instrument sync.
```

- [ ] **Step 7: Make preflight name the fix**

In `src/preflight.rs`, in `check_catalog`, change the two `Fatal` messages that mention seeding so they name the new command. Replace the `venue`/`currency` hint strings:

```rust
    for (table, hint) in [
        ("venue", "run `oms database init` (or `oms database migrate` if it exists)"),
        ("currency", "run `oms database init` (or `oms database migrate` if it exists)"),
    ] {
```

- [ ] **Step 8: Verify**

Run: `cargo build && cargo test`
Expected: clean build with no warnings about unused functions in `bootstrap.rs`; all tests pass.

- [ ] **Step 9: Commit**

```bash
git add src/main.rs src/setup/bootstrap.rs src/preflight.rs src/setup/database/config.rs
git commit -m "refactor(setup): remove boot-time provisioning

Starting the server no longer creates or migrates anything — oms database
init is the only path, so provisioning logic cannot drift between two
implementations. The runtime pool derives its connection from the same
POSTGRES_* values init used and always connects as oms_user, which removes
the DB_PASSWORD/OMS_USER_PASSWORD pair that had to agree by hand.

Obsolete keys left in a .env are now reported by name with their
replacement, because the alternative is a silent fallback to defaults."
```

---

### Task 9: Delete make, the shell scripts and the Python seeder

**Files:**
- Delete: `Makefile`, `db/scripts/{provision,migrate,access,seed,fixtures,seed_live}.sh`, `db/scripts/seed_venues.py`, `scripts/requirements.txt`
- Modify: `readme.md`, `.env.example`

**Interfaces:** none — documentation and removal only.

- [ ] **Step 1: Confirm nothing still references them**

Run:

```bash
grep -rn "make db-\|db/scripts/\|seed_venues\|OMS_BOOTSTRAP\|OMS_DEV_IDENTITY\|OMS_DB_SCRIPTS_DIR" \
  --include='*.rs' --include='*.md' --include='*.yml' --include='*.toml' . \
  | grep -v target/ | grep -v docs/superpowers/
```

Expected: only hits in `readme.md`, which Step 3 rewrites. Any hit in `src/` means Task 8 missed something — fix it before continuing.

- [ ] **Step 2: Delete the files**

```bash
git rm Makefile
git rm db/scripts/provision.sh db/scripts/migrate.sh db/scripts/access.sh \
       db/scripts/seed.sh db/scripts/fixtures.sh db/scripts/seed_live.sh \
       db/scripts/seed_venues.py scripts/requirements.txt
```

Keep `db/scripts/*.sql` (they are embedded by `assets.rs`) and `db/scripts/cleanup_orphaned_options.sql`.

- [ ] **Step 3: Rewrite the README Install/Run sections**

Replace everything from `## Install` up to `## Auth` in `readme.md` with:

````markdown
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
````

- [ ] **Step 4: Rewrite `.env.example`**

Replace its entire contents with:

```sh
# openoms configuration — every value here is optional.
#
# Database settings default to a local Postgres (localhost:5432, superuser
# `postgres`, database `ods`) and can also be given as flags to
# `cargo run -- database ...`. Set them here only to point somewhere else.
#
# POSTGRES_HOST=localhost
# POSTGRES_PORT=5432
# POSTGRES_USERNAME=postgres
# POSTGRES_PASSWORD=postgres
# POSTGRES_DATABASE=ods

# Passwords for the two roles `database init` creates. Change these for anything
# that is not a local development machine — the server refuses to start with the
# built-in default against a non-loopback host.
# MDM_MASTER_PASSWORD=openoms-dev
# OMS_USER_PASSWORD=openoms-dev

# Server
OMS_BIND_ADDR=localhost:3001
OMS_ADMIN_AUTH_ENABLED=true
OMS_ADMIN_PASSWORD=change-me-to-a-secure-random-token

# Brokers — a broker with credentials gets a broker_connection row on boot.
ALPACA_ENV=PAPER
ALPACA_PAPER_API_KEY=
ALPACA_PAPER_API_SECRET=
BINANCE_PAPER_API_KEY=
BINANCE_PAPER_PRIVATE_KEY_PATH=

# Market data
DATABENTO_API_KEY=

# Kafka (optional)
KAFKA_BROKER=
KAFKA_TOPIC=
KAFKA_CLIENT_ID=
KAFKA_PROJECTOR_GROUP_ID=
```

- [ ] **Step 5: Verify**

Run: `cargo build && cargo test`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "chore(setup): delete make, the shell scripts and the python seeder

Everything they did now lives in \`oms database\`. Prerequisites drop from
Rust + Postgres + psql + python3 to Rust + Postgres, and there is one
documented way to set up a clone instead of two.

.env becomes an override file: every database value has a working localhost
default, so a fresh checkout needs no configuration at all."
```

---

### Task 10: CI — prove init and migrate against a real Postgres

**Files:**
- Modify: `.github/workflows/build.yml`

**Interfaces:** none.

- [ ] **Step 1: Add a Postgres service and the provisioning job**

Add this job to `.github/workflows/build.yml`:

```yaml
  database:
    name: database init/migrate
    runs-on: ubuntu-latest
    services:
      postgres:
        image: postgres:16
        env:
          POSTGRES_PASSWORD: postgres
        ports:
          - 5432:5432
        options: >-
          --health-cmd pg_isready
          --health-interval 10s
          --health-timeout 5s
          --health-retries 5
    env:
      POSTGRES_HOST: localhost
      POSTGRES_PORT: 5432
      POSTGRES_USERNAME: postgres
      POSTGRES_PASSWORD: postgres
      POSTGRES_DATABASE: ods
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo build --quiet

      - name: init on an empty server succeeds
        run: cargo run --quiet -- database init --fixtures

      - name: init a second time fails loudly
        run: |
          if cargo run --quiet -- database init; then
            echo "expected the second init to fail, but it succeeded"
            exit 1
          fi

      - name: migrate is idempotent
        run: |
          cargo run --quiet -- database migrate
          out=$(cargo run --quiet -- database migrate)
          echo "$out"
          echo "$out" | grep -q "applied 0 migration" \
            || { echo "second migrate was not a no-op"; exit 1; }

      - name: status reports no pending migrations
        run: |
          out=$(cargo run --quiet -- database status)
          echo "$out"
          echo "$out" | grep -q "0 pending" || { echo "unexpected pending"; exit 1; }

      - name: database-backed tests
        run: cargo test -- --ignored
```

- [ ] **Step 2: Verify locally first**

Against a scratch database, so the dev one is untouched:

```bash
POSTGRES_DATABASE=ods_ci cargo run -- database init --fixtures
POSTGRES_DATABASE=ods_ci cargo run -- database init          # must fail
POSTGRES_DATABASE=ods_ci cargo run -- database migrate       # applied 0
POSTGRES_DATABASE=ods_ci cargo run -- database status        # 43 applied, 0 pending
POSTGRES_DATABASE=ods_ci cargo test -- --ignored
POSTGRES_DATABASE=ods_ci cargo run -- database drop
```

Expected: init succeeds and prints the phase list; the second init exits non-zero with the "already initialized" message; migrate reports 0; status reports 43 applied, 0 pending.

- [ ] **Step 3: Verify the existing dev database is unaffected**

```bash
cargo run -- database status     # expect: 43 applied, 0 pending
cargo run -- database migrate    # expect: applied 0 migration(s)
cargo run                        # expect: preflight clean, server listens
```

This is the proof that the tracking table stayed compatible — the whole point of keeping `public._mdm_migrations` unchanged.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/build.yml
git commit -m "ci: prove database init is strict and migrate is idempotent

Runs init on an empty Postgres, asserts a second init fails, and asserts a
second migrate applies nothing — the two properties the CLI's contract rests
on, neither of which can be tested without a server."
```

---

## Self-Review

**Spec coverage** — every section maps to a task:

| Spec section | Task |
|---|---|
| CLI surface (`init/migrate/drop/status`, `--fixtures`) | 6, 7 |
| `init` strict, `migrate` idempotent, error text | 6, verified in 10 |
| Credential model, three-tier merge, `POSTGRES_*` | 1 |
| Runtime pool derives from same values | 8 |
| Default passwords localhost-only | 8 |
| Module layout `src/setup/database/` | 1–6 |
| Embedded assets | 2 |
| Migration runner semantics | 4 |
| Provisioning (`\gexec` replacement) | 3 |
| Venue seeding (CSV, no Python) | 5 |
| Removals (make, scripts, bootstrap admin phase) | 8, 9 |
| `ensure_broker_connections` / `spawn_sync` survive | 8 Step 6 |
| `OMS_DEV_IDENTITY` subsumed by `--fixtures` | 8 Step 5, 5 |
| Migration path for the existing dev DB | 10 Step 3 |
| Risk 1: silent auth failure on renamed keys | 8 |
| Risk 2: `cargo run` no longer self-provisions | 8 Step 5 (error names the fix) |
| Risk 3: default passwords off-localhost | 8 Step 5 |
| Verification items 1–7 | 10 |

**Type consistency:** `PostgresOverrides` is produced in Task 1 and consumed in 6 and 7 (via `From<DbArgs>`); `Existing` is produced in 3 and consumed in 6; `MigrationTarget`/`TARGETS` produced in 2, consumed in 4; `parse_mic_csv`/`VenueRow` produced in 5, used only within 5. `migrate::migrate` name collision avoided — the module function in `mod.rs` is `pub async fn migrate` and calls `migrate::apply_all`, which is unambiguous because the module is `super::migrate`.

**Known wrinkle for the implementer:** in `mod.rs`, `pub async fn migrate` shadows the `migrate` module inside that function's body. If the compiler objects, rename the module import to `use migrate as migrate_mod;` or call it as `crate::setup::database::migrate::apply_all`. Flagged here rather than left to be discovered.

**Placeholder scan:** no TBD/TODO, every code step has real code, no "similar to Task N".
