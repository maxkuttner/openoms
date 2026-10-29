# Guided `oms init` and Getting Started Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `oms init` asks every first-run decision (database source with optional Docker Postgres, admin password, listen address) through dialoguer prompts; the installer offers to run it; the docs collapse Installation + First run into one OS-tabbed Getting started page.

**Architecture:** `src/setup/init.rs` becomes a module `src/setup/init/` split by responsibility: `rules.rs` (pure decisions), `docker.rs` (Docker Postgres behind a `DockerOps` trait), `prompts.rs` (a `Prompter` trait, its dialoguer impl, and the `collect` flow), `mod.rs` (`run`, config building, flag-driven answers). `main.rs` gains the new flags and an `oms config path` subcommand the installer uses to detect an existing install.

**Tech Stack:** Rust (clap 4 derive, sqlx, tokio), `dialoguer` 0.11 (replaces `rpassword`), POSIX sh installer, Astro Starlight docs.

**Spec:** `docs/superpowers/specs/2026-10-29-guided-init-and-getting-started-design.md`

## Global Constraints

- Docker Postgres: container `oms-postgres`, image `postgres:16`, volume `oms-pgdata`, `--restart unless-stopped`, published on `127.0.0.1` only.
- Docker port: 5432, else first free from 5433 up; say so.
- Readiness: retry connecting for up to 60 s, then fail naming `docker logs oms-postgres`.
- Existing `oms-postgres` container or `oms-pgdata` volume → refuse, naming both and the `docker rm` / `docker volume rm` to run. Never reuse or delete silently.
- Docker superuser password: generated, used for provisioning, never printed, never stored.
- Admin password: no echo, confirmed, minimum 12 characters; Enter generates; only a generated password is printed, and only in interactive mode.
- Listen default `localhost:3001`; non-loopback prints one warning line (cockpit and trading API reachable from the network; prefer an SSH tunnel, Tailscale, or a reverse proxy with TLS).
- Flags: `--postgres docker|existing` (non-interactive default `existing`), `--admin-password` / `OMS_ADMIN_PASSWORD`, `--bind` / `OMS_BIND_ADDR`.
- Interactive `oms init` with no terminal fails up front with the `--non-interactive` / `--password` hint.
- Installer stays prompt-free except `Run oms init now? [Y/n]` from `/dev/tty`; skipped when no usable tty, `OMS_NO_INIT=1`, or a config already exists.
- Superuser password never persisted; secrets never printed in non-interactive mode; every failure after `oms.toml` is written names `oms database init --resume`.
- Commit only what the task touches; never commit `.gitignore`, `.ignore`, `docs/superpowers/plans/2026-09-28-options-search-chain-strategy.md`.
- Worktree builds: `CARGO_TARGET_DIR=/Users/max/workarea/oms/target`.

## Review Focus

1. User picks Docker but the image isn't pulled yet: `docker run` pulls for minutes before readiness polling starts — the 60 s readiness clock must start after `docker run` returns, not before. (Task 2 test: `readiness_timeout_starts_after_run_returns`.)
2. Docker started, then the connection probe or provisioning fails: the container is left running holding a password nobody knows. Message must say so and name `docker rm -f oms-postgres && docker volume rm oms-pgdata` to retry clean. (Task 4 test: `docker_failure_after_start_names_cleanup`.)
3. `--admin-password` shorter than 12 in non-interactive mode must be refused, not written. (Task 4 test: `short_admin_password_flag_is_refused`.)
4. Installer under `curl | sh` in CI (no controlling tty) must not hang or error. (Task 5 verification step with `setsid`/`</dev/null`.)
5. Bind typed as `:3001` or `3001` must re-ask, not write a config the server can't bind. (Task 1 test: `bind_rejects_missing_host`.)

---

### Task 1: Pure decision rules + module split

**Files:**
- Move: `src/setup/init.rs` → `src/setup/init/mod.rs` (`git mv`, content unchanged in this task except `mod rules;`)
- Create: `src/setup/init/rules.rs`
- Modify: `Cargo.toml` (add `dialoguer = "0.11"`, keep `rpassword` until Task 3)

**Interfaces:**
- Produces (in `rules.rs`, all `pub`):
  - `pub enum DockerStatus { Usable, NotFound, NotRunning }` (derive `Debug, Clone, Copy, PartialEq, Eq`)
  - `#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)] pub enum DbSource { Docker, Existing }`
  - `pub fn default_db_source(s: DockerStatus) -> DbSource`
  - `pub const ADMIN_PASSWORD_MIN: usize = 12;`
  - `pub fn check_admin_password(pw: &str) -> Result<(), String>`
  - `pub fn parse_bind(s: &str) -> Result<String, String>` — trims; accepts `host:port` where port parses as u16 and host is non-empty (`localhost`, a hostname, an IPv4, or `[ipv6]`); returns the trimmed value.
  - `pub fn exposure_warning(bind: &str) -> Option<String>` — `None` for `localhost`, `127.x.x.x`, `[::1]`; `Some(line)` otherwise.
  - `pub fn docker_status_note(s: DockerStatus) -> &'static str` — `"Docker found"`, `"Docker not found"`, `"Docker not running"`.
  - `pub fn docker_fix_hint(s: DockerStatus) -> Option<&'static str>` — `None` for `Usable`; install Docker / start the daemon (or add the user to the `docker` group) otherwise.

- [ ] **Step 1: Write the failing tests** in `rules.rs` `mod tests`:

```rust
#[test] fn docker_is_the_default_only_when_usable() {
    assert_eq!(default_db_source(DockerStatus::Usable), DbSource::Docker);
    assert_eq!(default_db_source(DockerStatus::NotFound), DbSource::Existing);
    assert_eq!(default_db_source(DockerStatus::NotRunning), DbSource::Existing);
}
#[test] fn admin_password_needs_twelve_chars() {
    assert!(check_admin_password("12345678901").is_err());
    assert!(check_admin_password("123456789012").is_ok());
}
#[test] fn bind_accepts_host_port_forms() {
    for ok in ["localhost:3001", "0.0.0.0:3001", "127.0.0.1:8080", "[::1]:3001", "oms.internal:3001", " localhost:3001 "] {
        assert!(parse_bind(ok).is_ok(), "{ok}");
    }
    assert_eq!(parse_bind(" localhost:3001 ").unwrap(), "localhost:3001");
}
#[test] fn bind_rejects_missing_host() {
    for bad in [":3001", "3001", "localhost", "localhost:", "localhost:99999", ""] {
        assert!(parse_bind(bad).is_err(), "{bad}");
    }
}
#[test] fn only_non_loopback_binds_warn() {
    for quiet in ["localhost:3001", "127.0.0.1:3001", "[::1]:3001"] { assert!(exposure_warning(quiet).is_none(), "{quiet}"); }
    let w = exposure_warning("0.0.0.0:3001").unwrap();
    assert!(w.contains("network") && w.contains("Tailscale"), "{w}");
}
#[test] fn every_unusable_docker_state_has_a_fix_hint() {
    assert!(docker_fix_hint(DockerStatus::Usable).is_none());
    assert!(docker_fix_hint(DockerStatus::NotFound).is_some());
    assert!(docker_fix_hint(DockerStatus::NotRunning).unwrap().contains("docker"));
}
```

- [ ] **Step 2: Run** `cargo test --lib setup::init::rules` — Expected: FAIL to compile (items not defined).
- [ ] **Step 3: Implement** the items above in `rules.rs`.
- [ ] **Step 4: Run** `cargo test --lib setup::init` — Expected: rules tests PASS, and the existing `setup::init::tests` still PASS (module move changed nothing).
- [ ] **Step 5: Commit** `feat(init): first-run decision rules; init becomes a module`.

---

### Task 2: Docker Postgres

**Files:**
- Create: `src/setup/init/docker.rs`

**Interfaces:**
- Consumes: `DockerStatus` (Task 1); `Prompts` and `generate_password()` (existing in `init/mod.rs`).
- Produces:
  - `pub const CONTAINER: &str = "oms-postgres"; pub const VOLUME: &str = "oms-pgdata"; pub const IMAGE: &str = "postgres:16";`
  - `pub const READY_TIMEOUT: Duration = Duration::from_secs(60);`
  - `pub trait DockerOps { fn status(&self) -> DockerStatus; fn container_exists(&self, name: &str) -> bool; fn volume_exists(&self, name: &str) -> bool; fn port_free(&self, port: u16) -> bool; fn run_postgres(&self, port: u16, superuser_password: &str) -> Result<(), String>; }`
  - `pub struct Cli;` — `impl DockerOps for Cli` shells out to `docker` (`docker info` with a 5 s cap for `status`; `docker container inspect` / `docker volume inspect` exit codes; `port_free` = `std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()`; `run_postgres` = `docker run -d --name oms-postgres --restart unless-stopped -e POSTGRES_PASSWORD=<pw> -p 127.0.0.1:<port>:5432 -v oms-pgdata:/var/lib/postgresql/data postgres:16`, stderr returned on failure).
  - `pub fn preflight(ops: &dyn DockerOps) -> Result<(), String>`
  - `pub fn choose_port(ops: &dyn DockerOps) -> Result<u16, String>` — 5432, else 5433..=5532, else error.
  - `pub fn start(ops: &dyn DockerOps) -> Result<(Prompts, Option<String>), String>` — preflight, choose_port, generate password, `run_postgres`; returns `Prompts { host: "127.0.0.1", port, database: "ods", username: "postgres", password }` and `Some(note)` when the port isn't 5432 (`"port 5432 is taken — using <p>"`).
  - `pub async fn wait_ready(p: &Prompts, timeout: Duration) -> Result<(), String>` — retries `PgConnection::connect` against the `postgres` db every 500 ms until success or `timeout`; error text contains `docker logs oms-postgres`.

- [ ] **Step 1: Write the failing tests** with a `Fake` implementing `DockerOps` (fields: `container: bool, volume: bool, taken: Vec<u16>, ran: RefCell<Option<u16>>`):

```rust
#[test] fn an_existing_container_or_volume_is_refused_by_name() {
    let e = preflight(&Fake { container: true, ..Fake::default() }).unwrap_err();
    assert!(e.contains("docker rm oms-postgres") && e.contains("docker volume rm oms-pgdata"), "{e}");
    assert!(preflight(&Fake { volume: true, ..Fake::default() }).is_err());
    assert!(preflight(&Fake::default()).is_ok());
}
#[test] fn port_falls_back_from_5432() {
    assert_eq!(choose_port(&Fake::default()).unwrap(), 5432);
    assert_eq!(choose_port(&Fake { taken: vec![5432, 5433], ..Fake::default() }).unwrap(), 5434);
}
#[test] fn start_runs_on_loopback_defaults_and_notes_a_moved_port() {
    let f = Fake { taken: vec![5432], ..Fake::default() };
    let (p, note) = start(&f).unwrap();
    assert_eq!((p.host.as_str(), p.port, p.database.as_str(), p.username.as_str()), ("127.0.0.1", 5433, "ods", "postgres"));
    assert!(p.password.len() >= 24);
    assert_eq!(*f.ran.borrow(), Some(5433));
    assert!(note.unwrap().contains("5433"));
}
#[test] fn start_never_runs_after_a_refused_preflight() {
    let f = Fake { container: true, ..Fake::default() };
    assert!(start(&f).is_err());
    assert!(f.ran.borrow().is_none());
}
#[tokio::test] async fn wait_ready_gives_up_naming_docker_logs() {
    let p = Prompts { host: "127.0.0.1".into(), port: 1, database: "ods".into(), username: "postgres".into(), password: "x".into() };
    let e = wait_ready(&p, Duration::from_millis(800)).await.unwrap_err();
    assert!(e.contains("docker logs oms-postgres"), "{e}");
}
#[test] fn readiness_timeout_starts_after_run_returns() {
    // `start` is synchronous and returns only after `run_postgres` (the pull) finished;
    // `wait_ready` takes its own timeout. Pin that `start` does not poll.
    let f = Fake::default();
    let t = std::time::Instant::now();
    start(&f).unwrap();
    assert!(t.elapsed() < Duration::from_secs(1));
}
#[tokio::test] #[ignore] async fn real_docker_postgres_comes_up() { /* needs Docker: start(&Cli), wait_ready(READY_TIMEOUT), then `docker rm -f oms-postgres; docker volume rm oms-pgdata` */ }
```

- [ ] **Step 2: Run** `cargo test --lib setup::init::docker` — Expected: FAIL to compile.
- [ ] **Step 3: Implement** `docker.rs` per Interfaces. `Cli::status`: `NotFound` when spawning `docker` fails with `NotFound`, `NotRunning` when `docker info` exits non-zero or exceeds 5 s.
- [ ] **Step 4: Run** `cargo test --lib setup::init::docker` — Expected: PASS (6 passed, 1 ignored). If Docker is available locally, also run `cargo test --lib real_docker_postgres_comes_up -- --ignored` — Expected: PASS and the container/volume removed afterwards.
- [ ] **Step 5: Commit** `feat(init): start PostgreSQL 16 in Docker`.

---

### Task 3: Prompter trait, dialoguer impl, `collect` flow

**Files:**
- Create: `src/setup/init/prompts.rs`
- Modify: `src/setup/init/mod.rs` — delete `prompt_with`, `prompt`, `PromptDefaults` and their tests (replaced; prerelease, no shims), keep `Prompts`.
- Modify: `Cargo.toml` — remove `rpassword`.

**Interfaces:**
- Consumes: Task 1 rules; `Prompts`; `generate_password()`.
- Produces:
  - `pub trait Prompter { fn select(&mut self, label: &str, items: &[String], default: usize) -> io::Result<usize>; fn input(&mut self, label: &str, default: &str, validate: &dyn Fn(&str) -> Result<(), String>) -> io::Result<String>; fn password(&mut self, label: &str, confirm: bool) -> io::Result<String>; fn note(&mut self, line: &str); }` — `password` returns `""` when the user pressed Enter.
  - `pub struct Terminal;` — `impl Prompter` with dialoguer `Select`, `Input::with_initial_text`-free `default`, `.validate_with`, `Password` (`with_confirmation("Confirm", "Passwords don't match")` when `confirm`, `allow_empty_password(true)`), one `ColorfulTheme`; `note` prints to stdout.
  - `pub enum DbChoice { Docker, Existing(Prompts) }`
  - `pub enum AdminPassword { Typed(String), Generated(String) }` with `pub fn value(&self) -> &str`.
  - `pub struct Answers { pub db: DbChoice, pub admin: AdminPassword, pub bind: String }`
  - `pub struct Defaults { pub host: String, pub port: u16, pub database: String, pub username: String, pub superuser_password: Option<String>, pub admin_password: Option<String>, pub bind: String, pub source: Option<DbSource> }`
  - `pub fn collect(p: &mut dyn Prompter, d: &Defaults, docker: DockerStatus) -> io::Result<Answers>`
- Behaviour of `collect`:
  1. Database: if `d.source` is `Some`, no menu. Else `select("Database", ["Start PostgreSQL 16 in Docker for me   (<docker_status_note>)", "Use an existing PostgreSQL 16"], default from `default_db_source`)`. Docker picked while not `Usable` → `note(docker_fix_hint)` and ask again.
  2. Existing → `input` host / port (validated u16) / database / superuser name, then `password("Superuser password", false)` unless `d.superuser_password` is set. Empty superuser password re-asks.
  3. Admin: if `d.admin_password` set, `Typed` (already validated by caller). Else `password("Cockpit admin password (Enter = generate)", true)`; `""` → `Generated(generate_password())`; else `check_admin_password` failure → `note(err)` and re-ask.
  4. Bind: `input("Listen on", &d.bind, parse_bind-as-validator)`; `exposure_warning` → `note`.

- [ ] **Step 1: Write the failing tests** with a `Script` prompter (a `VecDeque` of canned answers per call kind, recording labels and notes):

```rust
#[test] fn enter_everywhere_with_docker_takes_docker_a_generated_password_and_localhost() {
    let mut s = Script::new().select(0).password("").input("localhost:3001");
    let a = collect(&mut s, &defaults(), DockerStatus::Usable).unwrap();
    assert!(matches!(a.db, DbChoice::Docker));
    assert!(matches!(a.admin, AdminPassword::Generated(ref p) if p.len() >= 24));
    assert_eq!(a.bind, "localhost:3001");
}
#[test] fn without_docker_the_menu_defaults_to_existing() {
    let mut s = Script::new().select_default().inputs(["db.internal", "5433", "ods", "postgres"]).password("su-pw").password("").input("localhost:3001");
    let a = collect(&mut s, &defaults(), DockerStatus::NotFound).unwrap();
    assert_eq!(s.select_defaults_seen(), vec![1]);
    assert!(matches!(a.db, DbChoice::Existing(ref p) if p.host == "db.internal" && p.port == 5433 && p.password == "su-pw"));
}
#[test] fn choosing_unusable_docker_explains_and_asks_again() {
    let mut s = Script::new().select(0).select(1).inputs(["localhost", "5432", "ods", "postgres"]).password("su").password("").input("localhost:3001");
    collect(&mut s, &defaults(), DockerStatus::NotRunning).unwrap();
    assert!(s.notes().iter().any(|n| n.contains("docker")));
}
#[test] fn a_short_admin_password_is_refused_and_re_asked() {
    let mut s = Script::new().select(0).password("short").password("long-enough-pw").input("localhost:3001");
    let a = collect(&mut s, &defaults(), DockerStatus::Usable).unwrap();
    assert!(matches!(a.admin, AdminPassword::Typed(ref p) if p == "long-enough-pw"));
    assert!(s.notes().iter().any(|n| n.contains("12")));
}
#[test] fn flag_values_skip_their_prompts() {
    let d = Defaults { source: Some(DbSource::Docker), admin_password: Some("from-the-flag-1".into()), ..defaults() };
    let mut s = Script::new().input("localhost:3001");
    let a = collect(&mut s, &d, DockerStatus::Usable).unwrap();
    assert!(matches!(a.admin, AdminPassword::Typed(_)));
    assert_eq!(s.selects_asked() + s.passwords_asked(), 0);
}
#[test] fn a_network_bind_warns_once() {
    let mut s = Script::new().select(0).password("").input("0.0.0.0:3001");
    collect(&mut s, &defaults(), DockerStatus::Usable).unwrap();
    assert_eq!(s.notes().iter().filter(|n| n.contains("network")).count(), 1);
}
```

`Script::input` must run the supplied validator and panic on a value it rejects, so tests can't feed an answer dialoguer would never return.

- [ ] **Step 2: Run** `cargo test --lib setup::init::prompts` — Expected: FAIL to compile.
- [ ] **Step 3: Implement** `prompts.rs`; delete the old prompt code/tests in `mod.rs`; remove `rpassword` from `Cargo.toml`.
- [ ] **Step 4: Run** `cargo test --lib setup::init` — Expected: PASS. `cargo build` — Expected: no `rpassword` references remain (`grep -rn rpassword src Cargo.toml` empty) and `main.rs` fails to compile only where it calls the removed `prompt`/`PromptDefaults` (fixed in Task 4; if the crate must compile at this commit, stub the `main.rs` call site to `collect(&mut Terminal, …)` here and finish wiring in Task 4).
- [ ] **Step 5: Commit** `feat(init): dialoguer prompts for database, admin password and listen address`.

---

### Task 4: Wire `run`, flags, `oms config path`

**Files:**
- Modify: `src/setup/init/mod.rs` (`run`, `build_file_config`, new `answers_from_flags`)
- Modify: `src/main.rs:381-387` (`Command::Init`), `:411-421` (`ConfigCmd`), `:642-698` (dispatch)

**Interfaces:**
- Consumes: Tasks 1–3.
- Produces:
  - `pub fn build_file_config(p: &Prompts, bind: &str) -> (FileConfig, String)` — `server.bind_addr = Some(bind)`.
  - `pub struct InitFlags { pub source: Option<DbSource>, pub admin_password: Option<String>, pub bind: Option<String> }` — `admin_password` falls back to env `OMS_ADMIN_PASSWORD`, `bind` to `OMS_BIND_ADDR` (empty = unset), resolved in `main.rs`.
  - `pub fn answers_from_flags(f: &InitFlags, db: Prompts) -> Result<Answers, String>` — non-interactive: `source` default `Existing`; admin validated by `check_admin_password` else `Generated`; bind via `parse_bind`, default `localhost:3001`.
  - `pub async fn run(a: Answers, docker: &dyn DockerOps, interactive: bool) -> Result<(), Box<dyn std::error::Error>>` — order: existing-config refusal → if `DbChoice::Docker`: `docker::start` (print note) then `wait_ready(READY_TIMEOUT)` → connectivity probe → write config (admin password + bind) → `database::init` → summary. Summary prints `Cockpit login password: <pw>` only for `Generated` + `interactive`; `Typed` prints `Cockpit login password: the one you chose`; non-interactive prints `written to oms.toml` when generated. Last line: `Start the server:  oms` then `Then open http://<bind>/cockpit/` (`localhost` substituted for `0.0.0.0`).
  - Test seam: `async fn run_with_timeout(a: Answers, docker: &dyn DockerOps, interactive: bool, ready: Duration, existing_cfg: PathBuf, target_cfg: PathBuf)`; `run` passes `config::path()`, `config::init_target()`, `READY_TIMEOUT`. Tests pass temp paths — never mutate `OMS_CONFIG` (the repo's `./oms.toml` would otherwise trip the already-initialized refusal, and env mutation races other tests).
  - `pub fn docker_cleanup_hint() -> String` — `"The oms-postgres container is still running. To start over: docker rm -f oms-postgres && docker volume rm oms-pgdata"`; appended to every error after `docker::start` succeeded.
  - CLI: `Init { non_interactive, #[arg(long, value_enum)] postgres: Option<DbSource>, #[arg(long)] admin_password: Option<String>, #[arg(long)] bind: Option<String>, db: DbArgs }`; `ConfigCmd::Path` — prints `config::absolute(config::path())` and exits 0 if it exists, else prints nothing and exits 1.
  - Interactive dispatch: before prompting, `std::io::stdin().is_terminal()` false → print the existing no-terminal hint (`oms init --password '<superuser password>'` / `oms init --non-interactive`) and exit 1. Then `collect(&mut Terminal, &defaults, docker::Cli.status())`. A `--admin-password` that fails `check_admin_password` exits 1 before any prompt.

- [ ] **Step 1: Write the failing tests** in `init/mod.rs` tests:

```rust
#[test] fn build_file_config_writes_the_chosen_bind() {
    let (cfg, _) = build_file_config(&sample_prompts(), "0.0.0.0:4000");
    assert_eq!(cfg.server.bind_addr.as_deref(), Some("0.0.0.0:4000"));
}
#[test] fn non_interactive_defaults_to_existing_localhost_and_a_generated_password() {
    let a = answers_from_flags(&InitFlags { source: None, admin_password: None, bind: None }, sample_prompts()).unwrap();
    assert!(matches!(a.db, DbChoice::Existing(_)));
    assert!(matches!(a.admin, AdminPassword::Generated(_)));
    assert_eq!(a.bind, "localhost:3001");
}
#[test] fn short_admin_password_flag_is_refused() {
    let e = answers_from_flags(&InitFlags { source: None, admin_password: Some("short".into()), bind: None }, sample_prompts()).unwrap_err();
    assert!(e.contains("12"), "{e}");
}
#[test] fn a_bad_bind_flag_is_refused() {
    assert!(answers_from_flags(&InitFlags { source: None, admin_password: None, bind: Some(":3001".into()) }, sample_prompts()).is_err());
}
#[tokio::test] async fn docker_failure_after_start_names_cleanup() {
    // Fake DockerOps whose run_postgres succeeds; nothing listens on the chosen port,
    // so wait_ready fails. run() must append docker_cleanup_hint().
    // Fake::taken = every port except 1 is irrelevant here: make choose_port return a
    // port nothing listens on, and pass a 500 ms timeout through the test seam.
    let a = Answers { db: DbChoice::Docker, admin: AdminPassword::Generated("x".repeat(24)), bind: "localhost:3001".into() };
    let e = run_with_timeout(a, &fake_docker_that_starts(), false, Duration::from_millis(500), tmp.join("none.toml"), tmp.join("oms.toml")).await.unwrap_err().to_string();
    assert!(e.contains("docker rm -f oms-postgres && docker volume rm oms-pgdata"), "{e}");
}
```

and in `main.rs` tests: `oms init --postgres docker --admin-password x --bind localhost:1` parses (clap `try_parse_from`), and `oms config path` parses.

- [ ] **Step 2: Run** `cargo test --lib setup::init` and `cargo test --bin oms init` — Expected: FAIL to compile.
- [ ] **Step 3: Implement** per Interfaces (`run_with_timeout` per the seam above).
- [ ] **Step 4: Run** `cargo test` (full suite, non-ignored) — Expected: all PASS. Then a real end-to-end against the shared dev Postgres is NOT allowed (it would create a database); instead, if Docker is available: `OMS_CONFIG=$SCRATCH/oms.toml ./target/debug/oms init --non-interactive --postgres docker --bind localhost:3999` — Expected: `wrote …/oms.toml`, `applied … migrations`, `Cockpit login password: written to oms.toml`; then `OMS_CONFIG=$SCRATCH/oms.toml ./target/debug/oms config path` prints that path; then clean up `docker rm -f oms-postgres && docker volume rm oms-pgdata`. If Docker isn't available, record it in the ledger and rely on the ignored test.
- [ ] **Step 5: Commit** `feat(init): guided setup — Docker or existing Postgres, admin password, listen address`.

---

### Task 5: Installer

**Files:**
- Modify: `install.sh` (closing section, ~lines 150–175); `docs-site/public/install.sh` is produced by `scripts/copy-install.mjs` at build — do not edit it by hand.

**Interfaces:**
- Consumes: `oms config path` (exit 0 = configured), `oms init`.

- [ ] **Step 1:** Replace the closing `next:` heredoc: `oms init` (not `oms database init`), `oms`, open `http://localhost:3001/cockpit/`, `oms update` later; drop the `docker compose` hint.
- [ ] **Step 2:** Add `offer_init()` called last in `main`: return early when `OMS_NO_INIT=1`; when `"$INSTALL_DIR/oms" config path >/dev/null 2>&1` succeeds (print `existing config found — skipping oms init`); or when `/dev/tty` can't be opened (`(: </dev/tty) 2>/dev/null || return 0`). Otherwise `printf 'Run oms init now? [Y/n] ' >/dev/tty; read -r ans </dev/tty`; empty/`y`/`Y` → `"$INSTALL_DIR/oms" init </dev/tty`; anything else → nothing (next steps were already printed).
- [ ] **Step 3: Run** `shellcheck install.sh` — Expected: no findings.
- [ ] **Step 4: Run** the no-tty path: `OMS_INSTALL_DIR=$SCRATCH/bin setsid sh install.sh </dev/null` (macOS: `script`-free — use `sh install.sh </dev/null 2>&1 | cat` from the Bash tool, which has no controlling tty) — Expected: exits 0, prints the `next:` block, never prints `Run oms init now?`. And `OMS_NO_INIT=1` likewise.
- [ ] **Step 5: Run** `cd docs-site && npm test` — Expected: PASS (the copied installer still builds).
- [ ] **Step 6: Commit** `feat(install): offer to run oms init; fix the stale next steps`.

---

### Task 6: Getting started page

**Files:**
- Create: `docs-site/src/content/docs/getting-started/index.mdx`
- Delete: `docs-site/src/content/docs/getting-started/installation.md`, `first-run.md`
- Create: `docs-site/public/getting-started/installation/index.html`, `docs-site/public/getting-started/first-run/index.html` (meta-refresh stubs to `/openoms/getting-started/`, same shape as `public/architecture.html`)
- Modify: `docs-site/astro.config.mjs:33-39` (sidebar: `getting-started` → first-paper-trade → first-api-call)
- Modify: links in `index.mdx` (landing), `operations/troubleshooting.md`, `getting-started/first-paper-trade.md`, `readme.md`
- Modify: `configuration/reference.md` (`OMS_ADMIN_PASSWORD`, `OMS_BIND_ADDR`, `OMS_NO_INIT`), CLI flags wherever `oms init` flags are documented
- Modify: `docs-site/test/dist.test.mjs` (redirect cases; grain-leak test reads `getting-started/index.html`)

**Interfaces:**
- Consumes: the prompt wording from Task 3 and summary lines from Task 4 (copy them from the code, not the spec).

- [ ] **Step 1: Write the failing tests** in `dist.test.mjs`: add `["getting-started/installation/index.html", "/openoms/getting-started/"]` and `["getting-started/first-run/index.html", "/openoms/getting-started/"]` to the redirect table, and:

```js
test("getting started has OS tabs and links the installer source, not the raw script", () => {
  const html = readFileSync(join(DIST, "getting-started/index.html"), "utf8");
  for (const tab of ["macOS", "Linux", "From source"]) assert.ok(html.includes(`>${tab}<`), `${tab} tab missing`);
  assert.ok(html.includes('data-sync-key="os"'), "tabs not synced across pages");
  assert.ok(html.includes("https://github.com/maxkuttner/openoms/blob/main/install.sh"));
  assert.ok(!html.includes('href="/openoms/install.sh"'), "still links the raw script");
});
```

- [ ] **Step 2: Run** `cd docs-site && npm run build && npm test` — Expected: the new tests FAIL.
- [ ] **Step 3: Write the page.** Title **Getting started**. Sections: Requirements (PostgreSQL 16 — or Docker and let `oms init` start one) → Install (`<Tabs syncKey="os">`: macOS / Linux / From source, each with install command, PATH line for `~/.zshrc` vs `~/.bashrc`, Linux OpenSSL 3 / glibc table, Docker Desktop vs Docker Engine + `docker` group note; From source = toolchain + build commands from the old page) → Set up (`oms init`, the prompt transcript as a `text` block, a table of what each answer does, the master-key backup caution) → Start (`oms`) → Open the cockpit → Next steps. A collapsed `<details><summary>Scripted installs</summary>` holds installer variables (`OMS_VERSION`, `OMS_INSTALL_DIR`, `OMS_NO_INIT`) and `oms init --non-interactive` flags. Updating section links to Operations → Updating.
- [ ] **Step 4:** Delete the two old pages, add redirect stubs, update sidebar and every link found by `grep -rn "first-run\|getting-started/installation" docs-site/src readme.md`.
- [ ] **Step 5: Run** `cd docs-site && npm run build && npm test` — Expected: PASS, link validator clean.
- [ ] **Step 6: Commit** `docs: one Getting started page with OS tabs and guided oms init`.
