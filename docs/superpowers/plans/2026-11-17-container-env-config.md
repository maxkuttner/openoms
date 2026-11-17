# Container Setup from Environment Variables Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The published image starts from a compose file with environment variables only — no `oms init`, no hand-edited `oms.toml` — while the `oms` server binary stays hands-off.

**Architecture:** A new `OMS_PUBLIC_BASE_URL` overrides `[auth].public_base_url`. A new CLI verb `oms database setup` picks `init` (fresh server) or `migrate` (existing database). The image's `ENTRYPOINT` becomes a POSIX `sh` script that, for a bare server start, checks for a master key, runs `oms database setup` when superuser credentials are given, then `exec oms`; any other arguments pass straight through.

**Tech Stack:** Rust (clap, sqlx), POSIX sh, Docker, GitHub Actions, Astro docs site.

**Spec:** `docs/superpowers/specs/2026-11-17-container-env-config-design.md`

## Global Constraints

- `oms` (the server, no subcommand) must not create or migrate a database — unchanged.
- `oms database init` and `oms database migrate` keep their behaviour and strictness; `setup` only chooses between them.
- Precedence stays flag > env > `oms.toml`.
- Existing compose installs with an `oms.toml` in `/config` keep working.
- Nothing in the repo refers to any specific server, host or deployment (public repo).
- DB tests run against throwaway Postgres only (CI services), never a shared database.

## Review Focus

1. **Roles exist, database absent** (shared Postgres cluster, or a half-finished earlier init): `setup` must fail with `init`'s existing refusal text, not migrate an empty database or create a second role. → test in Task 2 (`plan_setup` returns `Init` for roles-only, and the CI job checks `init`'s refusal still applies).
2. **`OMS_PUBLIC_BASE_URL` with a trailing slash or whitespace**: must behave like the file value does today (the origin check already trims `/`); empty/whitespace-only is ignored. → test in Task 1.
3. **Master key only in `oms.toml` (existing installs)**: the start script must not refuse to start. → test in Task 3 (`master_key` line in the config file satisfies the check).
4. **Image started with arguments** (`docker compose run --rm oms init`, `oms database status`): no setup run, no master-key check, exact passthrough. → test in Task 3.
5. **Restarting an already-set-up container**: second start migrates as a no-op and serves. → Task 4 smoke test restarts the container.

---

## File Structure

```
src/config.rs                       OMS_PUBLIC_BASE_URL override (pure helper + use in auth_settings)
src/setup/database/mod.rs           plan_setup() + setup()
src/main.rs                         DatabaseCmd::Setup wiring
docker/entrypoint.sh                the image's start script (new)
docker/entrypoint_test.sh           sh tests with a fake `oms` on PATH (new)
Dockerfile                          COPY + ENTRYPOINT the script
.github/workflows/build.yml         shellcheck + entrypoint test; `database setup` job
.github/workflows/release.yml       env-only image smoke test before push
docs-site/public/compose.yaml       env-only setup
docs-site/src/content/docs/getting-started/installation.mdx   Docker tab
docs-site/src/content/docs/configuration/reference.md         OMS_PUBLIC_BASE_URL row
```

---

### Task 1: `OMS_PUBLIC_BASE_URL`

**Files:**
- Modify: `src/config.rs` (`auth_settings`, around L529–547; tests module)

**Interfaces:**
- Produces: `fn public_base_url_from(env: Option<&str>, file: Option<String>) -> Option<String>` (private, pure); `FileConfig::auth_settings()` reads `std::env::var("OMS_PUBLIC_BASE_URL")` through it. Because `serve` falls back to `FileConfig::default().auth_settings()` when no file exists (main.rs ~L1460), env-only installs get the override too.

- [ ] **Step 1: Write the failing tests** in `config.rs`'s tests module:
```rust
#[test]
fn public_base_url_env_overrides_the_file() {
    assert_eq!(
        public_base_url_from(Some("https://oms.example.com"), Some("https://old.example".into())).as_deref(),
        Some("https://oms.example.com")
    );
}
#[test]
fn public_base_url_env_empty_or_blank_is_ignored() {
    assert_eq!(public_base_url_from(Some(""), Some("https://f".into())).as_deref(), Some("https://f"));
    assert_eq!(public_base_url_from(Some("   "), None), None);
}
#[test]
fn public_base_url_env_is_trimmed() {
    assert_eq!(public_base_url_from(Some(" https://x/ "), None).as_deref(), Some("https://x/"));
}
#[test]
fn no_env_keeps_the_file_value() {
    assert_eq!(public_base_url_from(None, Some("https://f".into())).as_deref(), Some("https://f"));
}
```
- [ ] **Step 2:** `cargo test --bin oms config::tests::public_base_url` → FAIL (function missing).
- [ ] **Step 3:** Implement `public_base_url_from` (trim; empty → fall back to `file`) and use it in `auth_settings()`: `public_base_url: public_base_url_from(std::env::var("OMS_PUBLIC_BASE_URL").ok().as_deref(), a.public_base_url.clone().or_else(|| a.oidc.public_base_url.clone()))`. Add `OMS_PUBLIC_BASE_URL` to the module doc comment's example next to `public_base_url`.
- [ ] **Step 4:** Same command → PASS; `cargo test --bin oms config::` → PASS.
- [ ] **Step 5:** Commit `feat(config): OMS_PUBLIC_BASE_URL overrides [auth].public_base_url`.

---

### Task 2: `oms database setup`

**Files:**
- Modify: `src/setup/database/mod.rs` (new `SetupAction`, `plan_setup`, `setup`; tests)
- Modify: `src/main.rs` (`DatabaseCmd::Setup`, dispatch at ~L737)
- Modify: `.github/workflows/build.yml` (new job `database setup`)

**Interfaces:**
- Produces:
  - `enum SetupAction { Init, Migrate }`
  - `fn plan_setup(existing: &provision::Existing) -> SetupAction` — `Migrate` iff `existing.database`, else `Init`.
  - `pub async fn setup(o: PostgresOverrides) -> Fallible` — `provision::inspect(&config::resolve(o.clone()))`, then `init(o, None, false)` or `migrate(o)`; prints which one it chose.
  - CLI: `oms database setup [DbArgs]`, doc comment "Initialize the database if it does not exist, otherwise apply pending migrations. For scripts and containers."
- Consumes: existing `init`, `migrate`, `provision::inspect`, `provision::Existing { roles, database }`. Check `PostgresOverrides: Clone`; derive it if not.

- [ ] **Step 1: Write the failing tests:**
```rust
#[test]
fn setup_initialises_a_fresh_server() {
    let e = provision::Existing { roles: vec![], database: false };
    assert!(matches!(plan_setup(&e), SetupAction::Init));
}
#[test]
fn setup_migrates_an_existing_database() {
    let e = provision::Existing { roles: vec![provision::ROLE.into()], database: true };
    assert!(matches!(plan_setup(&e), SetupAction::Migrate));
}
#[test]
fn setup_hands_a_half_made_install_to_init_which_refuses() {
    // roles but no database: init (without --resume) refuses with its own message
    let e = provision::Existing { roles: vec![provision::ROLE.into()], database: false };
    assert!(matches!(plan_setup(&e), SetupAction::Init));
    assert!(plan_provisioning(false, &e).is_err());
}
```
- [ ] **Step 2:** `cargo test --bin oms setup::database::tests::setup_` → FAIL.
- [ ] **Step 3:** Implement `SetupAction`, `plan_setup`, `setup`; add `Setup { #[command(flatten)] db: DbArgs }` to `DatabaseCmd` and `DatabaseCmd::Setup { db } => setup::database::setup(db.into()).await` to the dispatch.
- [ ] **Step 4:** Same command → PASS; `cargo test --bin oms setup::database::` → PASS.
- [ ] **Step 5:** CI job in `build.yml`, `database setup`, own `postgres:16` service (user `postgres`, password `postgres`, port 5432) and env `OMS_PASSWORD: setup-ci-role-password`:
  1. `cargo run --quiet -- database setup` → output names init; exit 0.
  2. `out=$(cargo run --quiet -- database setup)`; `echo "$out" | grep -q "applied 0 migration"` (second run is a migrate no-op).
  3. `cargo run --quiet -- database status | grep -q "0 pending"`.
  4. `PGPASSWORD=postgres psql -h localhost -U postgres -c 'DROP DATABASE ods WITH (FORCE)'`, then `cargo run --quiet -- database setup` must exit non-zero (roles left, database gone → init's refusal).
- [ ] **Step 6:** Commit `feat(database): oms database setup — init if absent, else migrate`.

---

### Task 3: The image's start script

**Files:**
- Create: `docker/entrypoint.sh`, `docker/entrypoint_test.sh`
- Modify: `Dockerfile` (COPY the script, `ENTRYPOINT ["/usr/local/bin/oms-entrypoint"]`), `.github/workflows/build.yml` (shellcheck list + run the test)

**Interfaces:**
- Produces: `/usr/local/bin/oms-entrypoint` in the image. Contract:
  - `$# -gt 0` → `exec oms "$@"` (no checks).
  - `$# -eq 0`:
    1. master key present ⇔ `OMS_MASTER_KEY` non-empty, or the file at `${OMS_CONFIG:-/config/oms.toml}` has a line matching `^[[:space:]]*master_key[[:space:]]*=`. Otherwise print to stderr: `oms: no master key. Set OMS_MASTER_KEY (generate one with: echo "base64:$(openssl rand -base64 32)") or provide [oms].master_key in $OMS_CONFIG.` and `exit 1`.
    2. `POSTGRES_PASSWORD` non-empty → `oms database setup || exit $?`; else print `oms: POSTGRES_PASSWORD not set; skipping database setup` to stderr.
    3. `exec oms`.

- [ ] **Step 1: Write the failing test** `docker/entrypoint_test.sh` (POSIX sh, `set -eu`): creates a temp dir with a fake `oms` that appends `"$*"` to `$CALLS` and exits `${FAKE_SETUP_EXIT:-0}` when called with `database setup`; prepends it to `PATH`; runs `sh docker/entrypoint.sh` under `env -i` with chosen variables and asserts on exit status and `$CALLS`:
  - no args, no key, no config → exit 1, stderr mentions `OMS_MASTER_KEY`, no calls;
  - no args, `OMS_MASTER_KEY=x`, `POSTGRES_PASSWORD=p` → calls `database setup` then `` (bare server start), in that order;
  - no args, key only in `OMS_CONFIG` file (`master_key = "base64:..."`), no `POSTGRES_PASSWORD` → calls only the bare start, stderr says skipping;
  - no args, key set, `POSTGRES_PASSWORD=p`, `FAKE_SETUP_EXIT=3` → exit 3, server never started;
  - args `init` → calls exactly `init`, with no key and no password set.
- [ ] **Step 2:** `sh docker/entrypoint_test.sh` → FAIL (script missing).
- [ ] **Step 3:** Write `docker/entrypoint.sh` per the contract (`#!/bin/sh`, `set -eu`).
- [ ] **Step 4:** `sh docker/entrypoint_test.sh` → all pass; `shellcheck docker/entrypoint.sh docker/entrypoint_test.sh` → clean.
- [ ] **Step 5:** Dockerfile: `COPY docker/entrypoint.sh /usr/local/bin/oms-entrypoint` (+ `chmod 0755` in the same layer or `COPY --chmod=0755`), `ENTRYPOINT ["/usr/local/bin/oms-entrypoint"]`. `--version` passthrough: `docker run --rm img --version` still prints the version (release.yml relies on it).
- [ ] **Step 6:** build.yml shellcheck job: add both files to the `shellcheck` line and a step `sh docker/entrypoint_test.sh`.
- [ ] **Step 7:** Commit `feat(docker): start script sets the database up before the server`.

---

### Task 4: Image smoke test (release)

**Files:**
- Modify: `.github/workflows/release.yml` (`docker` job, after "the image reports the tag's version", before push)

- [ ] **Step 1:** Add step "the image starts from env vars alone":
  ```sh
  docker network create smoke
  docker run -d --name pg --network smoke -e POSTGRES_PASSWORD=pw postgres:16
  until docker exec pg pg_isready -U postgres; do sleep 1; done
  env="-e POSTGRES_HOST=pg -e POSTGRES_PASSWORD=pw -e OMS_PASSWORD=smoke-role-pw -e OMS_MASTER_KEY=base64:$(openssl rand -base64 32) -e OMS_PUBLIC_BASE_URL=http://localhost:3001"
  docker run -d --name oms --network smoke -p 127.0.0.1:3001:3001 $env openoms:ci
  for _ in $(seq 60); do curl -fsS http://127.0.0.1:3001/health && break; sleep 2; done
  curl -fsS http://127.0.0.1:3001/health
  docker restart oms
  for _ in $(seq 60); do curl -fsS http://127.0.0.1:3001/health && break; sleep 2; done
  curl -fsS http://127.0.0.1:3001/health
  docker logs oms 2>&1 | grep -q "applied 0 migration" || { docker logs oms; exit 1; }
  if docker run --rm -e POSTGRES_PASSWORD=pw openoms:ci; then echo "started without a master key"; exit 1; fi
  ```
  Print `docker logs oms` on any failure.
- [ ] **Step 2:** Verify locally before committing: `docker build -t openoms:ci .` then run the step's commands by hand → `/health` 200 twice, the no-key run exits 1.
- [ ] **Step 3:** Commit `ci(release): the image starts from env vars alone`.

---

### Task 5: Compose file and docs

**Files:**
- Modify: `docs-site/public/compose.yaml`, `docs-site/src/content/docs/getting-started/installation.mdx` (Docker tab), `docs-site/src/content/docs/configuration/reference.md`

- [ ] **Step 1:** `compose.yaml`:
  - header steps become:
    ```
    #   [ -f .env ] || {
    #     echo "POSTGRES_PASSWORD=$(openssl rand -hex 24)"
    #     echo "OMS_PASSWORD=$(openssl rand -hex 24)"
    #     echo "OMS_MASTER_KEY=base64:$(openssl rand -base64 32)"
    #   } > .env
    #   docker compose up -d
    ```
    plus one line: set `OMS_PUBLIC_BASE_URL` in `.env` when the app is reached at any URL other than `http://localhost:3001`.
  - `oms.environment` adds `OMS_PASSWORD`, `OMS_MASTER_KEY` (both `${...:?...}` like `POSTGRES_PASSWORD`) and `OMS_PUBLIC_BASE_URL: ${OMS_PUBLIC_BASE_URL:-http://localhost:3001}`.
  - `oms-config` volume stays.
- [ ] **Step 2:** Docker tab of `installation.mdx`: same commands (no `oms init`); bullets: keep `.env` safe — `OMS_MASTER_KEY` is the only key to stored broker credentials, back it up; behind a domain or reverse proxy set `OMS_PUBLIC_BASE_URL`; existing installs with `oms.toml` in the volume keep working and now migrate on start. Remove the `docker compose cp ... oms.toml.backup` bullet (no file on new installs) and replace with the `.env` backup note.
- [ ] **Step 3:** `reference.md`: add `OMS_PUBLIC_BASE_URL` wherever environment variables are listed (next to `OMS_BIND_ADDR`), and mention `oms database setup` where `init`/`migrate` are listed.
- [ ] **Step 4:** `docker compose -f docs-site/public/compose.yaml config -q` with a stub `.env` (the three variables) → valid. `cd docs-site && npm run build && npm test` → pass.
- [ ] **Step 5:** Commit `docs: compose install from env vars, no oms init`.

---

### Task 6: Whole-branch verification

- [ ] `cargo test --workspace` → pass; `cargo test -- --ignored --skip real_docker` against a throwaway Postgres → pass.
- [ ] `sh docker/entrypoint_test.sh`, `shellcheck` list → pass.
- [ ] Local image check: build, run Task 4's commands → pass.
- [ ] Open the PR; CI green (the release smoke test runs on the next tag).
