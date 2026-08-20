# Bootstrap Configuration File Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace hand-edited `.env` bootstrap with a generated `oms.toml`, written by a new interactive `oms init` that also provisions the database.

**Architecture:** A new `src/config.rs` owns the `oms.toml` file — its shape, loading, and atomic writing. `setup/database/config.rs` gains a file tier between env and default, so precedence becomes flag → env → file → default without changing its existing signatures. `oms init` is a thin command that prompts, generates secrets, writes the file, and delegates to the existing `setup::database::init`.

**Plan 1 of 4** for this spec; the others are listed under *Out of scope*.

**Tech Stack:** Rust 2021, clap 4 (derive), serde 1 (derive), `toml` 0.8 (new), `rpassword` 7 (new), `rand` 0.8 (new), `base64` 0.22 (present), tokio, sqlx 0.8.

**Spec:** `docs/superpowers/specs/2026-08-20-connection-config-gui-design.md`

## Global Constraints

- **Rust edition 2021.** Match surrounding style: doc comments explain *why*, not *what*.
- **`oms.toml` file mode is `0600`.** Unix only; the project targets macOS and Linux.
- **The master key is 32 bytes from the OS CSPRNG**, stored base64 with a `base64:` prefix.
- **Precedence is flag → env → `oms.toml` → default**, in that order, everywhere.
- **`oms database init|migrate|drop|status` keep their current behaviour and flags.** CI drives them directly (`.github/workflows/build.yml:50-73`). `oms init` wraps `database init`; it does not replace it.
- **No new system dependencies.** Pure-Rust crates only.
- **Secrets are never logged.** No `Debug` derive that prints a password or key; no `info!` containing one.
- **Tests that need Postgres are `#[ignore]`d**, matching `migrate.rs::applying_twice_is_a_no_op`.

---

## File Structure

**Create:**
- `src/config.rs` — the `oms.toml` document: `FileConfig`, `load()`, `write_new()`, `path()`. One responsibility: this file's shape and its bytes on disk.
- `src/setup/init.rs` — the `oms init` command: prompting, secret generation, orchestration.

**Modify:**
- `Cargo.toml` — add `toml`, `rpassword`, `rand`.
- `src/main.rs` — register `mod config`, add the `Init` subcommand, read `bind_addr` and the admin password through the file tier.
- `src/setup/database/config.rs` — insert the file tier into `resolve` and `resolve_role_password`.
- `src/setup/mod.rs` — `pub mod init;`.
- `.gitignore` — add `oms.toml`.
- `.env.example` — note that `oms.toml` is now the primary home.
- `README.md` — `oms init` becomes the documented first step.

**Why `config.rs` sits at the crate root** rather than under `setup/`: it is read by `serve()` on every boot, not only by setup commands, so filing it under `setup` would misdescribe it.

---

### Task 1: The `oms.toml` document

**Files:**
- Create: `src/config.rs`
- Modify: `Cargo.toml`, `src/main.rs` (add `mod config;`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub struct FileConfig { pub database: DatabaseSection, pub oms: OmsSection, pub server: ServerSection }`
  - `pub struct DatabaseSection { pub host: Option<String>, pub port: Option<u16>, pub username: Option<String>, pub database: Option<String> }`
  - `pub struct OmsSection { pub password: Option<String>, pub master_key: Option<String> }`
  - `pub struct ServerSection { pub bind_addr: Option<String>, pub admin_password: Option<String> }`
  - `pub fn path() -> PathBuf`
  - `pub fn load() -> Option<&'static FileConfig>`
  - `pub fn parse(toml_str: &str) -> Result<FileConfig, ConfigError>`
  - `pub enum ConfigError { Parse(String), Io(String) }`

- [ ] **Step 1: Add the dependencies**

In `Cargo.toml`, alongside the existing `serde`/`base64` lines:

```toml
toml = "0.8"
rpassword = "7"
rand = "0.8"
```

- [ ] **Step 2: Write the failing tests**

Create `src/config.rs` containing only the tests for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Every field is optional: a partial file must parse, because the missing
    /// pieces fall through to env or the built-in defaults.
    #[test]
    fn parses_a_partial_file() {
        let cfg = parse("[database]\nhost = \"db.internal\"\n").expect("parse");
        assert_eq!(cfg.database.host.as_deref(), Some("db.internal"));
        assert_eq!(cfg.database.port, None);
        assert_eq!(cfg.oms.password, None);
    }

    /// An entirely empty file is legal and yields all-None.
    #[test]
    fn parses_an_empty_file() {
        let cfg = parse("").expect("parse");
        assert_eq!(cfg.database.host, None);
        assert_eq!(cfg.server.bind_addr, None);
    }

    #[test]
    fn parses_a_full_file() {
        let cfg = parse(
            r#"
[database]
host = "localhost"
port = 5432
username = "postgres"
database = "ods"

[oms]
password = "role-pw"
master_key = "base64:AAAA"

[server]
bind_addr = "0.0.0.0:3001"
admin_password = "admin-pw"
"#,
        )
        .expect("parse");
        assert_eq!(cfg.database.port, Some(5432));
        assert_eq!(cfg.oms.master_key.as_deref(), Some("base64:AAAA"));
        assert_eq!(cfg.server.bind_addr.as_deref(), Some("0.0.0.0:3001"));
        assert_eq!(cfg.server.admin_password.as_deref(), Some("admin-pw"));
    }

    /// A malformed file must be a clear error, never a silent empty config —
    /// silently ignoring a typo would connect to the wrong server.
    #[test]
    fn rejects_malformed_toml() {
        let err = parse("[database\nhost = ").expect_err("should not parse");
        assert!(matches!(err, ConfigError::Parse(_)));
    }

    /// An unknown key is a typo, and typos in a config file that selects a
    /// database must not pass silently.
    #[test]
    fn rejects_unknown_keys() {
        let err = parse("[database]\nhsot = \"typo\"\n").expect_err("should not parse");
        assert!(matches!(err, ConfigError::Parse(_)));
    }

    /// `{:?}` on the config must never print the master key or a password. A
    /// panic message or a stray log line is the classic way secrets escape.
    #[test]
    fn debug_output_redacts_every_secret() {
        let cfg = parse(
            "[oms]\npassword = \"role-pw\"\nmaster_key = \"base64:KEYKEYKEY\"\n\
             [server]\nadmin_password = \"admin-pw\"\n",
        )
        .expect("parse");
        let rendered = format!("{cfg:?}");
        for secret in ["role-pw", "base64:KEYKEYKEY", "admin-pw"] {
            assert!(!rendered.contains(secret), "{secret} leaked into {rendered}");
        }
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test config::tests -- --nocapture`
Expected: FAIL to compile — `parse`, `ConfigError`, `FileConfig` do not exist.

- [ ] **Step 4: Write the implementation**

Put this above the `#[cfg(test)] mod tests` block in `src/config.rs`:

```rust
//! The `oms.toml` bootstrap file.
//!
//! Holds the settings that cannot live in the database, because they are what
//! tells the process how to reach it — plus the master key, which must not live
//! in the thing it encrypts. Everything else belongs in Postgres and is managed
//! from the cockpit.
//!
//! Every field is optional. The file is one tier in the flag → env → file →
//! default chain, so a half-filled file is normal rather than an error.

use std::path::PathBuf;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// Where the file lives when `--config`/`OMS_CONFIG` say nothing.
pub const DEFAULT_FILENAME: &str = "oms.toml";

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    #[serde(default)]
    pub database: DatabaseSection,
    #[serde(default)]
    pub oms: OmsSection,
    #[serde(default)]
    pub server: ServerSection,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseSection {
    pub host: Option<String>,
    pub port: Option<u16>,
    /// Superuser *name* only. Its password is prompted, never stored — that
    /// credential can drop the database.
    pub username: Option<String>,
    pub database: Option<String>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OmsSection {
    /// Password for the `oms` role the server connects as. Generated by `oms init`.
    pub password: Option<String>,
    /// `base64:`-prefixed 32 bytes. Losing this loses every stored credential.
    pub master_key: Option<String>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSection {
    pub bind_addr: Option<String>,
    pub admin_password: Option<String>,
}

// Hand-written so a stray `{:?}` — in a log line, a panic message, an
// `expect_err` — cannot print the master key or either password. `FileConfig`
// derives Debug and contains these, so deriving here would leak through it.
impl std::fmt::Debug for OmsSection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OmsSection")
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("master_key", &self.master_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl std::fmt::Debug for ServerSection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerSection")
            .field("bind_addr", &self.bind_addr)
            .field("admin_password", &self.admin_password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Parse(String),
    Io(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Parse(m) => write!(f, "{m}"),
            ConfigError::Io(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for ConfigError {}

pub fn parse(toml_str: &str) -> Result<FileConfig, ConfigError> {
    toml::from_str(toml_str).map_err(|e| ConfigError::Parse(e.to_string()))
}

/// The configured path: `--config` is threaded in as `OMS_CONFIG` by `main`, so
/// this single function serves both.
pub fn path() -> PathBuf {
    std::env::var("OMS_CONFIG")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_FILENAME))
}

static LOADED: OnceLock<Option<FileConfig>> = OnceLock::new();

/// Read and cache the file. Absent is `None` — running without one is valid.
///
/// A file that exists but does not parse is fatal: it means the operator wrote
/// something they believe is in effect. Silently ignoring it could point the
/// server at a different database than they intended.
pub fn load() -> Option<&'static FileConfig> {
    LOADED
        .get_or_init(|| {
            let p = path();
            match std::fs::read_to_string(&p) {
                Err(_) => None,
                Ok(text) => match parse(&text) {
                    Ok(cfg) => Some(cfg),
                    Err(e) => {
                        eprintln!("error: {} is not valid: {e}", p.display());
                        std::process::exit(1);
                    }
                },
            }
        })
        .as_ref()
}
```

Add `mod config;` to `src/main.rs` beside the other `mod` declarations (near `mod preflight;`, line 62).

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test config::tests`
Expected: 6 passed.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/config.rs src/main.rs
git commit -m "feat(config): the oms.toml bootstrap document

Every field optional — the file is one tier in flag → env → file →
default, so a partial file is normal. Unknown keys are rejected, because
a typo in the file that selects your database must not pass silently."
```

---

### Task 2: Atomic write with 0600

**Files:**
- Modify: `src/config.rs`

**Interfaces:**
- Consumes: `FileConfig` from Task 1.
- Produces: `pub fn write_new(path: &Path, cfg: &FileConfig) -> Result<(), ConfigError>`

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` in `src/config.rs`:

```rust
    use std::os::unix::fs::PermissionsExt;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("oms-cfg-test-{}-{}", std::process::id(), name));
        p
    }

    /// The file holds a master key and a role password: group- and world-readable
    /// would leak both to every account on the machine.
    #[test]
    fn writes_with_owner_only_permissions() {
        let p = temp_path("perms.toml");
        let _ = std::fs::remove_file(&p);
        write_new(&p, &FileConfig::default()).expect("write");

        let mode = std::fs::metadata(&p).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
        std::fs::remove_file(&p).ok();
    }

    /// Round-trip: what we write must be what we read back.
    #[test]
    fn written_file_parses_back_identically() {
        let p = temp_path("roundtrip.toml");
        let _ = std::fs::remove_file(&p);

        let mut cfg = FileConfig::default();
        cfg.database.host = Some("db.internal".into());
        cfg.database.port = Some(6543);
        cfg.oms.password = Some("role-pw".into());
        cfg.oms.master_key = Some("base64:AAAA".into());
        write_new(&p, &cfg).expect("write");

        let back = parse(&std::fs::read_to_string(&p).expect("read")).expect("parse");
        assert_eq!(back.database.host.as_deref(), Some("db.internal"));
        assert_eq!(back.database.port, Some(6543));
        assert_eq!(back.oms.password.as_deref(), Some("role-pw"));
        assert_eq!(back.oms.master_key.as_deref(), Some("base64:AAAA"));
        std::fs::remove_file(&p).ok();
    }

    /// Refuse to clobber. The existing file may hold the only copy of a master
    /// key, and overwriting it destroys every stored credential.
    #[test]
    fn refuses_to_overwrite_an_existing_file() {
        let p = temp_path("exists.toml");
        std::fs::write(&p, "# already here\n").expect("seed");

        let err = write_new(&p, &FileConfig::default()).expect_err("must refuse");
        assert!(matches!(err, ConfigError::Io(_)));
        assert_eq!(std::fs::read_to_string(&p).expect("read"), "# already here\n");
        std::fs::remove_file(&p).ok();
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test config::tests -- --nocapture`
Expected: FAIL to compile — `write_new` not found.

- [ ] **Step 3: Write the implementation**

Add to `src/config.rs`, and extend the `use` lines at the top to
`use std::path::{Path, PathBuf};`:

```rust
/// Write a new config file, refusing to overwrite one that exists.
///
/// Created with mode 0600 *at open time* rather than chmod-ed afterwards, so
/// there is no window in which the master key sits in a world-readable file.
pub fn write_new(path: &Path, cfg: &FileConfig) -> Result<(), ConfigError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let body = toml::to_string_pretty(cfg).map_err(|e| ConfigError::Parse(e.to_string()))?;
    let header = "# openoms bootstrap configuration.\n\
                  #\n\
                  # Generated by `oms init`. Back this file up: the master key below is the\n\
                  # only thing that can decrypt stored broker credentials.\n\n";

    // create_new fails if the path exists — the check and the create are one
    // atomic operation, so a concurrent `oms init` cannot slip between them.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| ConfigError::Io(format!("could not create {}: {e}", path.display())))?;

    f.write_all(header.as_bytes())
        .and_then(|_| f.write_all(body.as_bytes()))
        .map_err(|e| ConfigError::Io(format!("could not write {}: {e}", path.display())))
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test config::tests`
Expected: 9 passed.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat(config): write oms.toml atomically at mode 0600

create_new makes the exists-check and the create one operation, and the
mode is set at open time rather than chmod-ed after — no window where the
master key sits in a world-readable file. Refuses to overwrite: the
existing file may hold the only copy of a key."
```

---

### Task 3: The file tier in database config resolution

**Files:**
- Modify: `src/setup/database/config.rs`

**Interfaces:**
- Consumes: `crate::config::{load, FileConfig}` from Task 1.
- Produces: no signature changes. `resolve(PostgresOverrides) -> PostgresConfig` and `resolve_role_password(Option<String>) -> String` keep their shapes; only their middle tier changes.

- [ ] **Step 1: Write the failing tests**

Add inside the existing `mod tests` in `src/setup/database/config.rs`:

```rust
    /// The file sits *below* env: a container injecting POSTGRES_HOST must win
    /// over a file baked into the image.
    #[test]
    fn env_beats_the_file() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        let file = crate::config::parse("[database]\nhost = \"from-file\"\n").expect("parse");
        std::env::set_var("POSTGRES_HOST", "from-env");
        assert_eq!(resolve_with(PostgresOverrides::default(), Some(&file)).host, "from-env");
        clear_env();
    }

    /// The file sits *above* the built-in default.
    #[test]
    fn file_beats_the_default() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        let file = crate::config::parse("[database]\nhost = \"from-file\"\nport = 6543\n")
            .expect("parse");
        let c = resolve_with(PostgresOverrides::default(), Some(&file));
        assert_eq!(c.host, "from-file");
        assert_eq!(c.port, 6543);
        // Unset in the file, so still the default.
        assert_eq!(c.database, "ods");
        clear_env();
    }

    /// A flag still beats everything.
    #[test]
    fn flag_beats_the_file() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        let file = crate::config::parse("[database]\nhost = \"from-file\"\n").expect("parse");
        let o = PostgresOverrides { host: Some("from-flag".into()), ..Default::default() };
        assert_eq!(resolve_with(o, Some(&file)).host, "from-flag");
        clear_env();
    }

    /// The role password follows the identical chain.
    #[test]
    fn role_password_reads_the_file_tier() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        let file = crate::config::parse("[oms]\npassword = \"from-file\"\n").expect("parse");
        assert_eq!(resolve_role_password_with(None, Some(&file)), "from-file");

        std::env::set_var("OMS_PASSWORD", "from-env");
        assert_eq!(resolve_role_password_with(None, Some(&file)), "from-env");
        clear_env();
    }

    /// No file at all must behave exactly as before this task.
    #[test]
    fn no_file_falls_back_to_env_and_default() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        let c = resolve_with(PostgresOverrides::default(), None);
        assert_eq!(c.host, "localhost");
        assert_eq!(c.database, "ods");
        assert_eq!(resolve_role_password_with(None, None), DEFAULT_ROLE_PASSWORD);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test setup::database::config -- --nocapture`
Expected: FAIL to compile — `resolve_with` and `resolve_role_password_with` not found.

- [ ] **Step 3: Write the implementation**

Replace the existing `resolve` and `resolve_role_password` in
`src/setup/database/config.rs` with these. The public signatures are unchanged;
the `_with` variants exist so tests can inject a file without touching global
state — `load()` caches in a `OnceLock` and cannot be reset between tests.

```rust
/// Flag → env → `oms.toml` → default.
pub fn resolve(o: PostgresOverrides) -> PostgresConfig {
    resolve_with(o, crate::config::load())
}

/// The tier chain with the file supplied explicitly. `resolve` passes the real
/// one; tests pass their own, because `config::load` caches process-globally.
pub fn resolve_with(o: PostgresOverrides, file: Option<&crate::config::FileConfig>) -> PostgresConfig {
    let db = file.map(|f| &f.database);
    PostgresConfig {
        host: o
            .host
            .or_else(|| from_env("POSTGRES_HOST"))
            .or_else(|| db.and_then(|d| d.host.clone()))
            .unwrap_or_else(|| "localhost".into()),
        // A malformed port falls back rather than panicking: the connection will
        // fail with a clear address anyway, and panicking in a config getter gives
        // a worse message than the connection error does.
        port: o
            .port
            .or_else(|| from_env("POSTGRES_PORT").and_then(|p| p.parse().ok()))
            .or_else(|| db.and_then(|d| d.port))
            .unwrap_or(5432),
        username: o
            .username
            .or_else(|| from_env("POSTGRES_USERNAME"))
            .or_else(|| db.and_then(|d| d.username.clone()))
            .unwrap_or_else(|| "postgres".into()),
        // Deliberately no file tier: the superuser password is prompted, never
        // stored. See the spec — that credential can drop the database.
        password: o
            .password
            .or_else(|| from_env("POSTGRES_PASSWORD"))
            .unwrap_or_else(|| "postgres".into()),
        database: o
            .database
            .or_else(|| from_env("POSTGRES_DATABASE"))
            .or_else(|| db.and_then(|d| d.database.clone()))
            .unwrap_or_else(|| "ods".into()),
    }
}

/// The password for the `oms` role, on the same tiers as everything else.
pub fn resolve_role_password(flag: Option<String>) -> String {
    resolve_role_password_with(flag, crate::config::load())
}

pub fn resolve_role_password_with(
    flag: Option<String>,
    file: Option<&crate::config::FileConfig>,
) -> String {
    flag.or_else(|| from_env("OMS_PASSWORD"))
        .or_else(|| file.and_then(|f| f.oms.password.clone()))
        .unwrap_or_else(|| DEFAULT_ROLE_PASSWORD.into())
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test setup::database::config`
Expected: all pass, including the five pre-existing tier tests.

- [ ] **Step 5: Commit**

```bash
git add src/setup/database/config.rs
git commit -m "feat(config): read oms.toml as the tier below env

Precedence becomes flag → env → file → default. Env stays above the file
so a container can override an image-baked config.

The superuser password deliberately has no file tier — it is prompted."
```

---

### Task 4: Master key and role password generation

**Files:**
- Create: `src/setup/init.rs`
- Modify: `src/setup/mod.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub fn generate_master_key() -> String` — `base64:`-prefixed 32 random bytes
  - `pub fn generate_password() -> String` — 24 URL-safe random characters

- [ ] **Step 1: Write the failing tests**

Create `src/setup/init.rs` with only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    /// 32 bytes is the AES-256 key size the credential store will expect. Getting
    /// this wrong would surface much later, as a decryption failure.
    #[test]
    fn master_key_is_32_bytes_base64_prefixed() {
        let k = generate_master_key();
        let body = k.strip_prefix("base64:").expect("base64: prefix");
        let raw = base64::engine::general_purpose::STANDARD
            .decode(body)
            .expect("valid base64");
        assert_eq!(raw.len(), 32, "master key must be 32 bytes, got {}", raw.len());
    }

    /// Two calls must not collide — the whole point is that these are not chosen
    /// by a human and not reused across installs.
    #[test]
    fn generated_values_are_unique() {
        assert_ne!(generate_master_key(), generate_master_key());
        assert_ne!(generate_password(), generate_password());
    }

    /// The password lands in a URL (`postgres://oms:<pw>@…`) and in a TOML string.
    /// Restricting the alphabet keeps both cases boring.
    #[test]
    fn password_is_url_safe_and_long_enough() {
        let p = generate_password();
        assert_eq!(p.len(), 24);
        assert!(
            p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "unexpected character in {p}"
        );
    }
}
```

Add `pub mod init;` to `src/setup/mod.rs`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test setup::init -- --nocapture`
Expected: FAIL to compile — `generate_master_key` not found.

- [ ] **Step 3: Write the implementation**

Above the test module in `src/setup/init.rs`:

```rust
//! `oms init` — the first-run command.
//!
//! Prompts for the connection to the operator's Postgres, generates everything
//! else, writes `oms.toml`, then delegates to `database init`. The generated
//! values are never chosen by a human: a password someone invents on the spot is
//! the weakest part of an otherwise encrypted store.

use base64::Engine;
use rand::RngCore;

/// 32 bytes — the AES-256 key size the credential store expects.
pub fn generate_master_key() -> String {
    let mut raw = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw);
    format!("base64:{}", base64::engine::general_purpose::STANDARD.encode(raw))
}

/// A 24-character password from a URL-safe alphabet. It ends up inside a
/// `postgres://` URL and inside a TOML string, so restricting the alphabet keeps
/// both boring — no escaping, no percent-encoding surprises.
pub fn generate_password() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut raw = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut raw);
    raw.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect()
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test setup::init`
Expected: 3 passed.

- [ ] **Step 5: Commit**

```bash
git add src/setup/init.rs src/setup/mod.rs
git commit -m "feat(init): generate the master key and role password

Both from the OS CSPRNG, never chosen by a human. The key is 32 bytes to
match AES-256; the password uses a URL-safe alphabet because it lands in
a postgres:// URL and a TOML string."
```

---

### Task 5: Prompting

**Files:**
- Modify: `src/setup/init.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub struct Prompts { pub host: String, pub port: u16, pub database: String, pub username: String, pub password: String }`
  - `pub fn prompt_with<R: BufRead>(input: &mut R, read_password: impl FnOnce() -> std::io::Result<String>) -> std::io::Result<Prompts>`
  - `pub fn prompt() -> std::io::Result<Prompts>` — wraps stdin and `rpassword`

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` in `src/setup/init.rs`:

```rust
    use std::io::BufReader;

    /// Pressing Enter through every prompt must yield a working localhost setup —
    /// that is what makes the happy path zero-decision.
    #[test]
    fn empty_input_takes_every_default() {
        let mut input = BufReader::new(&b"\n\n\n\n"[..]);
        let p = prompt_with(&mut input, || Ok("secret".into())).expect("prompt");
        assert_eq!(p.host, "localhost");
        assert_eq!(p.port, 5432);
        assert_eq!(p.database, "ods");
        assert_eq!(p.username, "postgres");
        assert_eq!(p.password, "secret");
    }

    #[test]
    fn typed_values_override_the_defaults() {
        let mut input = BufReader::new(&b"db.internal\n6543\ntrading\nadmin\n"[..]);
        let p = prompt_with(&mut input, || Ok("pw".into())).expect("prompt");
        assert_eq!(p.host, "db.internal");
        assert_eq!(p.port, 6543);
        assert_eq!(p.database, "trading");
        assert_eq!(p.username, "admin");
    }

    /// Surrounding whitespace is a paste artefact, not part of a hostname.
    #[test]
    fn trims_surrounding_whitespace() {
        let mut input = BufReader::new(&b"  db.internal  \n\n\n\n"[..]);
        let p = prompt_with(&mut input, || Ok("pw".into())).expect("prompt");
        assert_eq!(p.host, "db.internal");
    }

    /// A non-numeric port falls back to the default rather than aborting the whole
    /// interactive session over one typo.
    #[test]
    fn unparseable_port_falls_back_to_the_default() {
        let mut input = BufReader::new(&b"\nnot-a-number\n\n\n"[..]);
        let p = prompt_with(&mut input, || Ok("pw".into())).expect("prompt");
        assert_eq!(p.port, 5432);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test setup::init -- --nocapture`
Expected: FAIL to compile — `prompt_with` not found.

- [ ] **Step 3: Write the implementation**

Add to `src/setup/init.rs`, extending the `use` lines with
`use std::io::{BufRead, Write};`:

```rust
/// What `oms init` asks for. Everything here describes the operator's existing
/// Postgres; nothing generated appears in this struct.
#[derive(Debug)]
pub struct Prompts {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    pub password: String,
}

/// Prompt against arbitrary input, with the password read through a supplied
/// closure. Split this way so the whole flow is testable — `rpassword` reads the
/// tty directly and cannot be driven from a test.
pub fn prompt_with<R: BufRead>(
    input: &mut R,
    read_password: impl FnOnce() -> std::io::Result<String>,
) -> std::io::Result<Prompts> {
    fn ask<R: BufRead>(input: &mut R, label: &str, default: &str) -> std::io::Result<String> {
        print!("{label} [{default}]: ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        input.read_line(&mut line)?;
        let trimmed = line.trim();
        Ok(if trimmed.is_empty() { default.to_string() } else { trimmed.to_string() })
    }

    let host = ask(input, "Postgres host     ", "localhost")?;
    // A typo here should cost one field, not the whole session.
    let port = ask(input, "Postgres port     ", "5432")?.parse().unwrap_or(5432);
    let database = ask(input, "Database name     ", "ods")?;
    let username = ask(input, "Superuser name    ", "postgres")?;
    let password = read_password()?;

    Ok(Prompts { host, port, database, username, password })
}

/// The real thing: stdin plus a no-echo password read.
pub fn prompt() -> std::io::Result<Prompts> {
    let stdin = std::io::stdin();
    let mut locked = stdin.lock();
    prompt_with(&mut locked, || rpassword::prompt_password("Superuser password: "))
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test setup::init`
Expected: 7 passed.

- [ ] **Step 5: Commit**

```bash
git add src/setup/init.rs
git commit -m "feat(init): interactive prompts with defaults

Enter through every prompt yields a working localhost setup. The password
read is injected as a closure so the flow is testable — rpassword talks to
the tty and cannot be driven from a test."
```

---

### Task 6: Wiring `oms init` together

**Files:**
- Modify: `src/setup/init.rs`, `src/main.rs`, `.gitignore`

**Interfaces:**
- Consumes: `Prompts`, `generate_master_key`, `generate_password` (Tasks 4-5); `crate::config::{FileConfig, write_new, path}` (Tasks 1-2); `setup::database::init` (existing, `src/setup/database/mod.rs:26`).
- Produces:
  - `pub fn build_file_config(p: &Prompts) -> (FileConfig, String)` — the config and the generated role password
  - `pub async fn run(prompts: Prompts) -> Result<(), Box<dyn std::error::Error>>`

- [ ] **Step 1: Write the failing test**

Append inside `mod tests` in `src/setup/init.rs`:

```rust
    /// The file we hand to `database init` must contain the generated secrets and
    /// the prompted connection — and must NOT contain the superuser password,
    /// which is the credential that can drop the database.
    #[test]
    fn builds_a_file_config_without_the_superuser_password() {
        let p = Prompts {
            host: "db.internal".into(),
            port: 6543,
            database: "trading".into(),
            username: "admin".into(),
            password: "super-secret".into(),
        };
        let (cfg, role_pw) = build_file_config(&p);

        assert_eq!(cfg.database.host.as_deref(), Some("db.internal"));
        assert_eq!(cfg.database.port, Some(6543));
        assert_eq!(cfg.database.username.as_deref(), Some("admin"));
        assert_eq!(cfg.database.database.as_deref(), Some("trading"));

        assert_eq!(cfg.oms.password.as_deref(), Some(role_pw.as_str()));
        assert!(cfg.oms.master_key.as_deref().unwrap().starts_with("base64:"));
        assert_eq!(cfg.server.bind_addr.as_deref(), Some("localhost:3001"));

        let rendered = toml::to_string_pretty(&cfg).expect("render");
        assert!(
            !rendered.contains("super-secret"),
            "superuser password must never reach the file:\n{rendered}"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test setup::init::tests::builds_a_file_config -- --nocapture`
Expected: FAIL to compile — `build_file_config` not found.

- [ ] **Step 3: Write the implementation**

Add to `src/setup/init.rs`:

```rust
use crate::config::{self, FileConfig};

/// Assemble the file from the prompts plus freshly generated secrets. Returns the
/// config and the generated role password, which the caller also needs to pass to
/// `database init`.
///
/// The superuser password is deliberately absent: it was used to connect, and
/// that is all. Persisting a credential that can `DROP DATABASE` is not worth
/// saving one prompt on the rare `migrate`.
pub fn build_file_config(p: &Prompts) -> (FileConfig, String) {
    let role_password = generate_password();
    let mut cfg = FileConfig::default();
    cfg.database.host = Some(p.host.clone());
    cfg.database.port = Some(p.port);
    cfg.database.username = Some(p.username.clone());
    cfg.database.database = Some(p.database.clone());
    cfg.oms.password = Some(role_password.clone());
    cfg.oms.master_key = Some(generate_master_key());
    cfg.server.bind_addr = Some("localhost:3001".to_string());
    cfg.server.admin_password = Some(generate_password());
    (cfg, role_password)
}

/// The whole `oms init` flow.
pub async fn run(prompts: Prompts) -> Result<(), Box<dyn std::error::Error>> {
    let cfg_path = config::path();
    if cfg_path.exists() {
        return Err(format!(
            "error: {} already exists\n\
             \x20        This machine is already initialized. Use `oms database migrate`\n\
             \x20        to upgrade it, or delete the file to start over — but note the\n\
             \x20        master key in it decrypts your stored credentials.",
            cfg_path.display()
        )
        .into());
    }

    // Test connectivity before writing anything: a wrong password should cost
    // nothing, not leave a half-made install behind.
    let probe = crate::setup::database::config::PostgresConfig {
        host: prompts.host.clone(),
        port: prompts.port,
        username: prompts.username.clone(),
        password: prompts.password.clone(),
        database: "postgres".to_string(),
    };
    sqlx::PgPool::connect(&probe.url_for("postgres"))
        .await
        .map_err(|e| format!("error: could not connect to {}:{} — {e}", prompts.host, prompts.port))?;
    println!("  connected to {}:{}", prompts.host, prompts.port);

    let (file_cfg, role_password) = build_file_config(&prompts);
    let admin_password = file_cfg.server.admin_password.clone().unwrap_or_default();

    // Written before provisioning: if provisioning fails halfway, the generated
    // secrets survive and `oms database init` can finish the job.
    config::write_new(&cfg_path, &file_cfg)?;
    println!("  wrote {} (mode 0600)", cfg_path.display());
    ensure_gitignored(&cfg_path);

    let overrides = crate::setup::database::config::PostgresOverrides {
        host: Some(prompts.host.clone()),
        port: Some(prompts.port),
        username: Some(prompts.username.clone()),
        password: Some(prompts.password.clone()),
        database: Some(prompts.database.clone()),
    };
    crate::setup::database::init(overrides, Some(role_password)).await?;

    println!(
        "\nBack up {}. The master key in it is the only thing that can\n\
         decrypt stored broker credentials — lose it and they are gone.\n\n\
         Cockpit login password: {admin_password}\n\n\
         Start the server:  oms",
        cfg_path.display()
    );
    Ok(())
}

/// Append the config filename to `.gitignore` if it is not already covered.
/// Best-effort: not being in a git repository is normal and not an error.
fn ensure_gitignored(cfg_path: &std::path::Path) {
    let Some(name) = cfg_path.file_name().and_then(|n| n.to_str()) else { return };
    let gitignore = std::path::Path::new(".gitignore");
    if !gitignore.exists() {
        return;
    }
    let current = std::fs::read_to_string(gitignore).unwrap_or_default();
    if current.lines().any(|l| l.trim() == name) {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(gitignore) {
        use std::io::Write;
        let _ = writeln!(f, "\n# openoms bootstrap config — holds the master key\n{name}");
        println!("  added {name} to .gitignore");
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test setup::init`
Expected: 8 passed.

- [ ] **Step 5: Register the subcommand**

In `src/main.rs`, add to `enum Command` (beside `Setup` and `Database`, line 186):

```rust
    /// First-run setup: generate oms.toml and create the database.
    Init {
        /// Take values from flags and the environment instead of prompting.
        #[arg(long)]
        non_interactive: bool,
        #[command(flatten)]
        db: DbArgs,
    },
```

And in the `match` that dispatches subcommands (beside the `DatabaseCmd` arms, around line 312):

```rust
        Some(Command::Init { non_interactive, db }) => {
            let prompts = if non_interactive {
                let cfg = setup::database::config::resolve(db.into());
                setup::init::Prompts {
                    host: cfg.host,
                    port: cfg.port,
                    database: cfg.database,
                    username: cfg.username,
                    password: cfg.password,
                }
            } else {
                match setup::init::prompt() {
                    Ok(p) => p,
                    Err(e) => { eprintln!("error: {e}"); std::process::exit(1); }
                }
            };
            if let Err(e) = setup::init::run(prompts).await {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
```

- [ ] **Step 6: Add the file to `.gitignore`**

Append to `.gitignore`:

```
# openoms bootstrap config — holds the master key
oms.toml
```

- [ ] **Step 7: Verify the command exists and builds**

Run: `cargo build && cargo run -q -- init --help`
Expected: help text listing `--non-interactive` and the `DbArgs` flags.

- [ ] **Step 8: Commit**

```bash
git add src/setup/init.rs src/main.rs .gitignore
git commit -m "feat(init): oms init wires prompts, secrets and database init

Connectivity is tested before anything is written, so a wrong password
costs nothing. oms.toml is written before provisioning, so a half-failed
provision leaves the generated secrets recoverable.

The superuser password never reaches the file — it is used to connect and
discarded. oms init wraps database init rather than replacing it; CI
drives the database verbs directly and they are unchanged."
```

---

### Task 7: Server reads bind address and admin password from the file

**Files:**
- Modify: `src/main.rs:391-437`

**Interfaces:**
- Consumes: `crate::config::load()` (Task 1).
- Produces: no new symbols. `serve()` resolves two values through the new tier.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` at the bottom of `src/main.rs` (it already holds the
`bind_is_loopback` tests):

```rust
    use crate::config::FileConfig;

    #[test]
    fn bind_addr_prefers_env_then_file_then_default() {
        let file = crate::config::parse("[server]\nbind_addr = \"1.2.3.4:9999\"\n").expect("parse");

        assert_eq!(resolve_bind_addr(Some("0.0.0.0:1".into()), Some(&file)), "0.0.0.0:1");
        assert_eq!(resolve_bind_addr(None, Some(&file)), "1.2.3.4:9999");
        assert_eq!(resolve_bind_addr(None, None), DEFAULT_BIND_ADDR);
    }

    #[test]
    fn admin_password_prefers_env_then_file() {
        let file = crate::config::parse("[server]\nadmin_password = \"from-file\"\n").expect("parse");

        assert_eq!(resolve_admin_password(Some("from-env".into()), Some(&file)).as_deref(), Some("from-env"));
        assert_eq!(resolve_admin_password(None, Some(&file)).as_deref(), Some("from-file"));
        assert_eq!(resolve_admin_password(None, Some(&FileConfig::default())), None);
        assert_eq!(resolve_admin_password(None, None), None);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --bin oms tests:: -- --nocapture`
Expected: FAIL to compile — `resolve_bind_addr` not found.

- [ ] **Step 3: Write the implementation**

Add these two helpers next to `bind_is_loopback` in `src/main.rs`:

```rust
/// Bind address on the usual tiers. No CLI flag exists for this today, so the
/// chain is env → file → default.
fn resolve_bind_addr(from_env: Option<String>, file: Option<&config::FileConfig>) -> String {
    from_env
        .filter(|v| !v.is_empty())
        .or_else(|| file.and_then(|f| f.server.bind_addr.clone()))
        .unwrap_or_else(|| DEFAULT_BIND_ADDR.to_string())
}

/// Cockpit login password: env → file. `None` means unset, which the caller
/// turns into the loopback-only default or a refusal.
fn resolve_admin_password(
    from_env: Option<String>,
    file: Option<&config::FileConfig>,
) -> Option<String> {
    from_env
        .filter(|v| !v.is_empty())
        .or_else(|| file.and_then(|f| f.server.admin_password.clone()))
        .filter(|v| !v.is_empty())
}
```

Then in `serve()`, replace the existing bind-address resolution (currently
`env::var("OMS_BIND_ADDR").ok().filter(...).unwrap_or_else(...)`) with:

```rust
    let file_cfg = config::load();
    let bind_addr = resolve_bind_addr(env::var("OMS_BIND_ADDR").ok(), file_cfg);
```

And replace the body of the `admin_token` match arm — the part currently reading
`env::var("OMS_ADMIN_PASSWORD").ok().filter(...).or_else(...)` — with:

```rust
        let configured = resolve_admin_password(
            env::var("OMS_ADMIN_PASSWORD")
                .ok()
                .or_else(|| env::var("OMS_ADMIN_TOKEN").ok()),
            file_cfg,
        );
        match configured {
```

The three arms that follow (`Some(token)`, `None if bind_is_loopback(...)`,
`None`) are unchanged.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --bin oms`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat(config): serve reads bind addr and admin password from oms.toml

Same env → file → default chain the database settings use. The loopback
default-password guard is untouched: a generated admin password in the
file counts as configured, so a fresh oms init never trips it."
```

---

### Task 8: `database status` without the superuser

The spec requires that the common "what state is my database in" check stop
demanding the credential that can drop it. `status` reads `pg_roles` and
`pg_database` — public catalogs any role can read — plus `_mdm_migrations`, which
is owned by the superuser. One grant closes the gap.

**Files:**
- Create: `db/migrations/ods/public/0024_GRANT_MIGRATIONS_READ_TO_OMS.sql`
- Modify: `src/setup/database/mod.rs:99-127`

**Interfaces:**
- Consumes: `config::resolve_role_password` (Task 3), `PostgresConfig::runtime_url` (existing, `config.rs:89`).
- Produces: no new symbols. `status` connects as the `oms` role instead of the superuser.

- [ ] **Step 1: Write the migration**

Create `db/migrations/ods/public/0024_GRANT_MIGRATIONS_READ_TO_OMS.sql`:

```sql
-- `oms database status` reports which migrations are applied. That is a read-only
-- inspection and must not require the superuser credential — the one that can drop
-- the database. pg_roles and pg_database are public catalogs; only the tracking
-- table needed a grant.
--
-- The table is owned by the connecting admin (see migrate.rs: the tracking row is
-- written after RESET ROLE), so this grant is what lets the ordinary role read it.

GRANT SELECT ON public._mdm_migrations TO oms;
```

- [ ] **Step 2: Verify the migration is picked up**

The runner embeds every `.sql` under `db/migrations/ods/public` via `include_dir!`
(`assets.rs:9`), and `assets.rs` has a test pinning the count.

Run: `cargo test setup::database::assets`
Expected: FAIL — `embeds_every_migration` asserts 23 public migrations, now 24.

- [ ] **Step 3: Update the pinned count**

In `src/setup/database/assets.rs`, find the assertion for the `public` target's
migration count and change `23` to `24`.

Run: `cargo test setup::database::assets`
Expected: PASS.

- [ ] **Step 4: Point `status` at the runtime role**

In `src/setup/database/mod.rs`, inside `status`, replace the pool connection

```rust
    let pool = PgPool::connect(&cfg.url()).await?;
```

with

```rust
    // Connect as the ordinary role, not the superuser: `status` is a read-only
    // inspection, and requiring a database-dropping credential to ask "is this
    // migrated?" would mean prompting for it constantly. Migration 0024 grants
    // this role SELECT on the tracking table.
    let pool = PgPool::connect(&cfg.runtime_url(&config::resolve_role_password(None))).await?;
```

`provision::inspect` above it already connects to the `postgres` maintenance
database and reads only public catalogs, so it is unaffected.

- [ ] **Step 5: Verify against a real database**

```bash
docker run -d --name oms-t8 -e POSTGRES_PASSWORD=secret -p 55433:5432 postgres:16
sleep 3
export POSTGRES_HOST=localhost POSTGRES_PORT=55433 \
       POSTGRES_USERNAME=postgres POSTGRES_PASSWORD=secret POSTGRES_DATABASE=ods
cargo run -q -- database init
# now prove status needs no superuser password at all:
env -u POSTGRES_PASSWORD POSTGRES_HOST=localhost POSTGRES_PORT=55433 \
    POSTGRES_DATABASE=ods cargo run -q -- database status
docker rm -f oms-t8
```

Expected: the second command prints `44 applied, 0 pending` without a superuser
password present in the environment.

- [ ] **Step 6: Commit**

```bash
git add db/migrations/ods/public/0024_GRANT_MIGRATIONS_READ_TO_OMS.sql \
        src/setup/database/assets.rs src/setup/database/mod.rs
git commit -m "feat(database): status runs as the oms role

Asking whether a database is migrated should not require the credential
that can drop it. pg_roles and pg_database are public catalogs; one grant
on the tracking table was all that stood in the way.

Leaves migrate and drop as the only verbs that ever need the superuser."
```

---

### Task 9: Documentation

**Files:**
- Modify: `README.md`, `.env.example`

**Interfaces:** none.

- [ ] **Step 1: Rewrite the README setup section**

Replace the `## Setup` code block with:

````markdown
```sh
git clone git@github.com:maxkuttner/openoms.git && cd openoms
cargo run -- init     # prompts for your Postgres, generates oms.toml, creates the database
cargo run             # start the OMS on localhost:3001
```
````

And replace the "What each step does" table rows with:

````markdown
| Step | What happens |
|---|---|
| `init` | Asks where your Postgres is, generates the `oms` role password, the master key and a cockpit login password, writes `oms.toml` (mode 0600), then creates the role, database, schemas, migrations, grants and reference data. |
| `cargo run` | Starts the server. It never creates or migrates anything — if the database is missing or stale, it says so and names the command to run. |
````

- [ ] **Step 2: Replace the configuration table**

Replace the settings table with one that names the file first:

````markdown
Everything lives in `oms.toml`, generated by `oms init`. Flags and environment
variables override it, in that order — that is how Docker and CI inject settings.

| Setting | `oms.toml` | Flag | Environment |
|---|---|---|---|
| Host | `database.host` | `--host` | `POSTGRES_HOST` |
| Port | `database.port` | `--port` | `POSTGRES_PORT` |
| Superuser | `database.username` | `--username` | `POSTGRES_USERNAME` |
| Superuser password | *(prompted)* | `--password` | `POSTGRES_PASSWORD` |
| Database | `database.database` | `--database` | `POSTGRES_DATABASE` |
| `oms` role password | `oms.password` | `--oms-password` | `OMS_PASSWORD` |
| Master key | `oms.master_key` | — | — |
| Bind address | `server.bind_addr` | — | `OMS_BIND_ADDR` |
| Cockpit password | `server.admin_password` | — | `OMS_ADMIN_PASSWORD` |

**Back up `oms.toml`.** The master key in it is the only thing that can decrypt
stored credentials. The superuser password is deliberately not in the file — it is
prompted by the two commands that need it, `database migrate` and `database drop`.
````

- [ ] **Step 3: Point `.env.example` at the file**

Add at the top of `.env.example`:

```
# openoms reads oms.toml first — run `oms init` to generate it. This file is only
# needed to override those settings, which is mainly how containers inject them.
```

- [ ] **Step 4: Verify the documented flow**

Run a real end-to-end check against a throwaway Postgres:

```bash
docker run -d --name oms-plan1-test -e POSTGRES_PASSWORD=secret -p 55432:5432 postgres:16
mkdir -p /tmp/oms-plan1 && cd /tmp/oms-plan1
printf 'localhost\n55432\nods\npostgres\n' | OMS_CONFIG=/tmp/oms-plan1/oms.toml \
  <path-to-repo>/target/debug/oms init     # password prompt reads the tty; use --non-interactive in CI
cat /tmp/oms-plan1/oms.toml
stat -f '%Sp' /tmp/oms-plan1/oms.toml      # expect -rw-------
docker rm -f oms-plan1-test
```

Expected: `oms.toml` exists at mode `0600` containing a `base64:` master key and a
generated `oms.password`; the database reports 43 migrations applied.

- [ ] **Step 5: Commit**

```bash
git add README.md .env.example
git commit -m "docs: oms init is the documented first step

The settings table now leads with oms.toml and shows flag and env as
overrides. Notes that the superuser password is prompted, not stored, and
that losing the master key loses stored credentials."
```

---

## Verification

After every task, the whole suite must be green:

```bash
cargo build          # no new warnings
cargo test           # 149 existing + 26 new
```

The end-to-end check in Task 8 Step 4 is the acceptance test: a fresh directory
with no `oms.toml` and no `.env`, pointed at an empty Postgres, produces a working
install from `oms init` alone.

**CI is unaffected.** `.github/workflows/build.yml` drives `oms database init`
directly with `POSTGRES_*` environment variables. Those keep working — env sits
above the file tier, and the database verbs are unchanged.

## Out of scope

These belong to later plans in the same spec:

- **Plan 2** — the encrypted credential store, `config import-env`,
  `config rotate-key`, and loading adapters from the database.

  **Deviation from the spec, recorded deliberately:** the spec places
  `oms config rotate-key` in the bootstrap section, implying it ships here.
  It cannot — rotation re-encrypts credential rows, and no credential rows exist
  until Plan 2 creates the table. Moved there; nothing is lost, because the master
  key this plan generates is forward-compatible with it. The `master_key`
  written here is unused until then, which is deliberate: generating it now means
  existing installs do not need a second migration later.
- **Plan 3** — `ArcSwap` registry and live feed restart.
- **Plan 4** — credential endpoints, `setup-status`, cockpit screens.
