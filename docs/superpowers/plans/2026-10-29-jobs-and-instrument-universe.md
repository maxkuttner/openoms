# Background Jobs and Instrument Universe Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persisted background jobs visible in a cockpit Task Center, and a per-broker-connection instrument universe that the cockpit can load, reload and show the status of — then the cockpit-first "first paper trade" guide, shipped with the release that makes it true.

**Architecture:** A new `oms.job` / `oms.job_log` pair of tables and a `src/jobs.rs` module own job records. Universe loads reuse the existing sync code, refactored out of the `oms setup sync-broker` CLI into a callable function that reports progress, run on a dedicated thread like today's boot sync. New admin endpoints expose jobs and per-connection universe status; the cockpit polls them.

**Tech Stack:** Rust (axum 0.7, sqlx/Postgres, utoipa, tokio), React + Mantine cockpit (`cockpit/`), Starlight docs (`docs-site/`).

**Spec:** `docs/superpowers/specs/2026-10-29-cockpit-setup-jobs-universe-design.md` — parts **A**, **B** and **D**. Part **C** (on-demand derivatives) gets its own plan.

## Global Constraints

- Jobs persist in Postgres. At startup every job left `queued` or `running` becomes `interrupted`.
- Statuses: `queued` → `running` → `succeeded` | `failed` | `interrupted`. Nothing re-runs automatically.
- One universe load per connection at a time; a second request is refused, naming the running job.
- Universe states: `not_loaded`, `loading` (progress `done / total`), `loaded` (count, when), `partial` (interrupted, or N skipped with reason), `failed` (error). Actions: Load, Reload, "Load again" (partial), Retry (failed).
- A connection's universe = base instruments only (Alpaca US equities/ETFs; Binance spot). Cockpit and boot loads skip enrichment (`no_enrich: true`).
- Boot: one `system` universe-load job per connection with configured credentials and no loaded universe; `OMS_SYNC_ON_BOOT=never` opts out; `OMS_SYNC_UNDERLYINGS` is removed.
- `POST /admin/symbology/backfill` and `POST /admin/instruments/expiry-sweep` become jobs answering `202` with the job id. Scheduled expiry sweeps write a job only when they expire ≥ 1 instrument.
- Saving credentials for a connection whose universe is `not_loaded` prompts: **"Load Alpaca's instrument universe now? About 14,400 stocks and ETFs — runs in the background."** with **Load now** / **Later** (broker name and count from the connection).
- Credential save → adapter reload stays synchronous; not a job. No raw server-log pane. No admin CLI.
- Every new endpoint: utoipa annotation with a `///` summary, unique `operation_id`, registered in `ApiDoc`, `docs/openapi.json` regenerated (`cargo run -- openapi > docs/openapi.json`). The existing tests `every_schema_ref_in_the_openapi_spec_resolves`, `every_operation_id_is_unique`, `the_committed_openapi_spec_is_current` must stay green.
- Migrations go in `db/migrations/ods/oms/`, next number `0027_…`; the server never migrates on its own.
- DB tests follow the repo pattern: `#[ignore = "needs a live Postgres; run with --ignored"]`, using `test_pool()` (see `src/admin.rs:3373`), and delete every row they create.
- User-facing docs (`docs-site/`) publish with the release that makes them true — the docs changes and the release land in the same push (Task 8).
- Build in the worktree with `CARGO_TARGET_DIR=/Users/max/workarea/oms/target` (a fresh target dir fails to link quickfix).

## Review Focus

1. **Two loads for one connection at once** (double-click, or a click while the boot load runs) — the second is refused with `409` naming the running job, never two writers. → Task 1 `a_second_active_universe_load_for_the_same_connection_is_refused`; Task 4 `load_endpoint_refuses_while_one_is_running`.
2. **The OMS restarts mid-load** — the job shows `interrupted`, the connection shows `partial` with "interrupted", and "Load again" works. → Task 1 `jobs_left_running_are_marked_interrupted`; Task 4 `an_interrupted_load_reads_as_partial`.
3. **A load for a connection without usable credentials** — refused with `422` before any job row is written. → Task 4 `load_endpoint_refuses_unconfigured_credentials`.
4. **The broker call fails mid-load** (bad key, network) — the job ends `failed` with the broker's error text and the connection shows `failed` with Retry. → Task 3 `a_failing_provider_fails_the_job_with_its_error`; Task 4 `a_failed_load_reads_as_failed`.
5. **A job that logs a lot** (enrichment over 14,000 symbols) — its log is capped at the newest 500 lines rather than growing without bound. → Task 1 `a_job_log_keeps_only_the_newest_500_lines`.

---

### Task 1: Job records — table, module, lifecycle

**Files:**
- Create: `db/migrations/ods/oms/0027_CREATE_JOB_TABLES.sql`
- Create: `src/jobs.rs` (+ `mod jobs;` in `src/main.rs`)
- Test: `src/jobs.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Produces:
  - `pub enum JobKind { UniverseLoad, Enrichment, SymbologyBackfill, ExpirySweep }` with `as_str()` → `"universe_load" | "enrichment" | "symbology_backfill" | "expiry_sweep"`.
  - `pub enum JobStatus { Queued, Running, Succeeded, Failed, Interrupted }` (serde `snake_case`, `utoipa::ToSchema`).
  - `#[derive(Serialize, ToSchema)] pub struct JobRecord { id: Uuid, kind: String, subject: Option<String>, started_by: String, params: serde_json::Value, status: JobStatus, progress_done: Option<i64>, progress_total: Option<i64>, result: Option<serde_json::Value>, error: Option<String>, created_at, started_at: Option<_>, finished_at: Option<_> }` (timestamps `chrono::DateTime<Utc>`).
  - `#[derive(Serialize, ToSchema)] pub struct JobLogLine { at: DateTime<Utc>, level: String, message: String }`.
  - `pub enum CreateError { AlreadyRunning(Uuid), Db(sqlx::Error) }`.
  - `pub async fn create(pool: &PgPool, kind: JobKind, subject: Option<&str>, started_by: &str, params: Value) -> Result<Uuid, CreateError>`
  - `pub struct JobHandle { pool: PgPool, pub id: Uuid }` with `async fn start(&self)`, `async fn progress(&self, done: i64, total: Option<i64>)`, `async fn log(&self, level: &str, message: &str)`, `async fn succeed(&self, result: Value)`, `async fn fail(&self, error: &str)` — each `Result<(), sqlx::Error>`; `JobHandle::new(pool, id)`.
  - `pub async fn mark_interrupted(pool: &PgPool) -> Result<u64, sqlx::Error>`
  - `pub async fn list_recent(pool: &PgPool, limit: i64) -> Result<Vec<JobRecord>, sqlx::Error>` (newest first)
  - `pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<(JobRecord, Vec<JobLogLine>)>, sqlx::Error>` (log oldest first)
  - `pub async fn latest(pool: &PgPool, kind: JobKind, subject: &str) -> Result<Option<JobRecord>, sqlx::Error>`
  - `pub const LOG_CAP: i64 = 500;`

- [ ] **Step 1: Write the migration.** `oms.job` with the `JobRecord` columns (`status` text with a CHECK over the five values, `params jsonb NOT NULL DEFAULT '{}'`); a **partial unique index** on `(kind, subject) WHERE status IN ('queued','running')` — this is what makes "one active load per connection" atomic. `oms.job_log (id bigserial PK, job_id uuid REFERENCES oms.job(id) ON DELETE CASCADE, at timestamptz NOT NULL DEFAULT now(), level text NOT NULL, message text NOT NULL)` with an index on `(job_id, id)`. Comment the file in the style of `0026_ALTER_ACCOUNT_ADD_PORTFOLIO_ID.sql`. Apply to the dev DB with `cargo run -- database migrate`.
- [ ] **Step 2: Write the failing DB tests** (all `#[ignore]`, `test_pool()`, unique `subject` per test, cleanup at the end):
  - `a_job_runs_through_its_lifecycle` — `create` → status `queued`; `start` → `running` and `started_at` set; `progress(3, Some(10))`; `succeed(json!({"n":3}))` → `succeeded`, `finished_at` set, `result == {"n":3}`.
  - `a_second_active_universe_load_for_the_same_connection_is_refused` — two `create(UniverseLoad, Some(s))` → second is `Err(CreateError::AlreadyRunning(first_id))`; after the first `succeed`s, a third `create` succeeds.
  - `jobs_left_running_are_marked_interrupted` — one `running`, one `queued`, one `succeeded`; `mark_interrupted` returns ≥ 2 and only those two are `interrupted`, with `finished_at` set.
  - `a_job_log_keeps_only_the_newest_500_lines` — 510 `log` calls → `get` returns exactly 500 lines, the first being message `"10"`.
  - `failing_a_job_records_the_error` — `fail("bad key")` → `failed`, `error == Some("bad key")`.
- [ ] **Step 3: Run to verify failure** — `CARGO_TARGET_DIR=/Users/max/workarea/oms/target cargo test --bin oms jobs:: -- --ignored`. Expected: compile errors (module missing).
- [ ] **Step 4: Implement `src/jobs.rs`.** `create` maps a unique-violation (`23505`) on the partial index to `AlreadyRunning` by selecting the active job's id. `log` inserts then deletes rows of that job older than the newest `LOG_CAP`.
- [ ] **Step 5: Run to verify pass**, then the whole suite `cargo test --bin oms`. Expected: the 5 ignored tests pass with `--ignored`; the full suite stays green.
- [ ] **Step 6: Commit** — `git commit -m "feat(jobs): persisted background job records"`

### Task 2: Universe load as a callable function

**Files:**
- Modify: `src/setup/brokers.rs:131-260` (`run`)
- Test: `src/setup/brokers.rs` tests

**Interfaces:**
- Produces:
  - `pub struct LoadOptions { pub no_enrich: bool, pub allow_skips: bool, pub underlyings: Vec<String> }`
  - `#[derive(Serialize)] pub struct UniverseLoadReport { pub fetched: usize, pub upserted: usize, pub mapped: usize, pub skipped: usize, pub skipped_reasons: Vec<String> }`
  - `pub trait LoadProgress { fn progress(&self, done: i64, total: i64); fn log(&self, level: &str, message: &str); }` (sync; the job runner adapts it)
  - `pub async fn load_universe(pool: &PgPool, broker: Broker, provider: &dyn InstrumentProvider, opts: &LoadOptions, p: &dyn LoadProgress) -> Result<UniverseLoadReport, Box<dyn std::error::Error>>`
  - `pub async fn provider_for(pool: &PgPool, broker: Broker) -> Result<Box<dyn InstrumentProvider>, Box<dyn std::error::Error>>` — the credential lookup now inline in `run` (master key, `load_brokers`, Configured/Unconfigured/Error/None messages unchanged).
- Consumes: `catalog::upsert_catalog`, `bulk_upsert_broker_instrument` (unchanged).

- [ ] **Step 1: Write the failing tests** (pure, with a fake `InstrumentProvider` returning fixed `BrokerInstrument`s and a recording `LoadProgress`; DB-backed ones `#[ignore]`):
  - `progress_counts_up_to_the_fetched_total` (`#[ignore]`) — 3 fake instruments → last `progress` call is `(3, 3)`; report `fetched == 3, mapped == 3`.
  - `skipped_instruments_are_reported_with_their_reason` (`#[ignore]`) — one fake instrument on venue `"ZZZZ"` (no MIC) → `skipped == 1`, `skipped_reasons` contains `"ZZZZ"`.
- [ ] **Step 2: Run to verify failure.** Expected: `load_universe` not found.
- [ ] **Step 3: Implement.** Move the body of `run` after credential lookup into `load_universe`: fetch → `progress(0, fetched)` → upsert catalog → map broker instruments in `BATCH` chunks, calling `progress(done, fetched)` after each chunk; `skipped_reasons` from the venues/currencies `upsert_catalog` already names. `run` becomes: `provider_for` → `load_universe` with a `LoadProgress` that logs via `tracing` → existing dry-run/allow-skips behaviour. CLI output unchanged.
- [ ] **Step 4: Run to verify pass**, plus `cargo run -- setup sync-broker --broker alpaca --no-enrich --dry-run` against the dev DB. Expected: tests pass; CLI prints the same "fetched … / dry run complete" lines as before.
- [ ] **Step 5: Commit** — `git commit -m "refactor(setup): universe load callable with progress, CLI wraps it"`

### Task 3: Universe-load jobs and boot

**Files:**
- Create: `src/jobs/universe.rs` (convert `src/jobs.rs` to `src/jobs/mod.rs`)
- Modify: `src/setup/bootstrap.rs:96-150` (replace `will_sync_on_boot`/`spawn_sync`/`sync_all_brokers`), `src/main.rs` (boot sequence near `bootstrap::spawn_sync`, `:852`, `:1227`)
- Test: `src/jobs/universe.rs` tests

**Interfaces:**
- Consumes: Task 1 (`create`, `JobHandle`, `mark_interrupted`), Task 2 (`provider_for`, `load_universe`, `LoadOptions`, `LoadProgress`).
- Produces:
  - `pub async fn spawn_universe_load(pool: PgPool, broker: Broker, started_by: &str) -> Result<Uuid, CreateError>` — creates the job (subject = `broker.connection_code()`, params `{"broker": code, "no_enrich": true}`), then runs it on a dedicated thread with its own current-thread runtime (same reason as today's `spawn_sync`: non-`Send` errors), returning the id immediately.
  - `pub async fn start_boot_loads(pool: &PgPool, credentialed: &[Broker]) -> Vec<Uuid>` — honours `OMS_SYNC_ON_BOOT=never`; for each broker whose `broker_instrument` count for its broker code is 0, `spawn_universe_load(…, "system")`.

- [ ] **Step 1: Write the failing tests** (`#[ignore]`):
  - `a_universe_load_job_succeeds_with_its_report` — a job run through the runner's inner function `run_job(handle, provider, opts)` with a 2-instrument fake provider → job `succeeded`, `result.fetched == 2`, `progress_done == progress_total == 2`.
  - `a_failing_provider_fails_the_job_with_its_error` — a fake provider erroring `"401 unauthorized"` → job `failed`, `error` contains `"401 unauthorized"`.
- [ ] **Step 2: Run to verify failure.**
- [ ] **Step 3: Implement** `run_job(handle: &JobHandle, provider: &dyn InstrumentProvider, broker: Broker, opts: &LoadOptions)` (the testable core) and `spawn_universe_load` around it. Boot: call `jobs::mark_interrupted` right after the pool connects and before anything spawns work; replace the `will_sync_on_boot`/`spawn_sync` call sites with `start_boot_loads`; delete `OMS_SYNC_UNDERLYINGS` handling. Preflight's "catalog empty" warning keeps its `auto_sync_pending` meaning: true when `start_boot_loads` returned any id.
- [ ] **Step 4: Run to verify pass** + full suite. Then start the dev OMS against a database whose Alpaca universe is empty (the throwaway Postgres pattern from the docs walkthrough) and confirm a `system` job appears and succeeds. Expected: tests pass; job row `succeeded`.
- [ ] **Step 5: Commit** — `git commit -m "feat(jobs): universe loads run as jobs; boot loads per connection"`

### Task 4: Universe status and load endpoints

**Files:**
- Create: `src/jobs/api.rs` (handlers + `UniverseStatus`)
- Modify: `src/main.rs` (routes next to `/admin/broker-connections…` ~`:1355-1400`; `ApiDoc` paths/schemas), `docs/openapi.json`
- Test: `src/jobs/api.rs` tests

**Interfaces:**
- Consumes: Tasks 1, 3.
- Produces:
  - `#[derive(Serialize, ToSchema)] #[serde(rename_all="snake_case")] pub enum UniverseState { NotLoaded, Loading, Loaded, Partial, Failed }`
  - `#[derive(Serialize, ToSchema)] pub struct UniverseStatus { state: UniverseState, count: i64, reason: Option<String>, last_job: Option<JobRecord> }`
  - `pub fn derive_universe_state(last_job: Option<&JobRecord>, count: i64) -> (UniverseState, Option<String>)` — pure.
  - `GET /admin/broker-connections/{code}/universe` → `UniverseStatus` (operation_id `get_connection_universe`, summary "Get a broker connection's instrument universe status").
  - `POST /admin/broker-connections/{code}/universe/load` → `202 {"job_id": …}`; `404` unknown code; `422` credentials not Configured; `409 {"error": "a universe load is already running", "job_id": …}` (operation_id `load_connection_universe`, summary "Load or reload a broker connection's instrument universe").
- Rules for `derive_universe_state`:

| last job | count | state | reason |
|---|---|---|---|
| queued / running | any | `loading` | — |
| failed | any | `failed` | the job's `error` |
| interrupted | any | `partial` | `"interrupted"` |
| succeeded, `result.skipped > 0` | any | `partial` | `"{skipped} skipped: {skipped_reasons joined}"` |
| succeeded | > 0 | `loaded` | — |
| none | > 0 | `loaded` | — (loaded by the CLI before jobs existed) |
| none / succeeded | 0 | `not_loaded` | — |

  `count` = rows in `broker_instrument` for the connection's `broker_code`.

- [ ] **Step 1: Write the failing tests** — pure: one test per table row, named after it (`a_running_load_reads_as_loading`, `a_failed_load_reads_as_failed`, `an_interrupted_load_reads_as_partial`, `skipped_instruments_read_as_partial_with_the_reason`, `a_succeeded_load_reads_as_loaded`, `a_cli_loaded_universe_reads_as_loaded`, `no_job_and_no_instruments_reads_as_not_loaded`). Router-level (`#[ignore]`, `tower::ServiceExt::oneshot` against the admin router as existing admin tests do): `load_endpoint_refuses_unconfigured_credentials` (422, no job row written), `load_endpoint_refuses_while_one_is_running` (pre-create an active job → 409 with its id), `load_endpoint_404s_an_unknown_connection`.
- [ ] **Step 2: Run to verify failure.**
- [ ] **Step 3: Implement** handlers, routes, `ApiDoc` registration; regenerate `docs/openapi.json`.
- [ ] **Step 4: Run to verify pass** + full suite (incl. the three OpenAPI tests).
- [ ] **Step 5: Commit** — `git commit -m "feat(api): broker connection universe status and load endpoints"`

### Task 5: Job endpoints; backfill and expiry sweep as jobs

**Files:**
- Modify: `src/jobs/api.rs`, `src/admin.rs:2638-2700` (`backfill_symbology`, `expiry_sweep`), `src/expiry.rs:114-132` (`run_once` / scheduled loop), `src/main.rs`, `docs/openapi.json`
- Test: `src/jobs/api.rs`, `src/expiry.rs` tests

**Interfaces:**
- Consumes: Task 1.
- Produces:
  - `GET /admin/jobs?limit=` → `Vec<JobRecord>`, default 50, max 200 (operation_id `list_jobs`, summary "List recent background jobs").
  - `GET /admin/jobs/{id}` → `JobDetail { job: JobRecord, log: Vec<JobLogLine> }` (operation_id `get_job`, summary "Get a background job and its log").
  - `backfill_symbology` and `expiry_sweep` → `202 {"job_id"}`; their former response bodies become the job's `result`.
  - Scheduled sweep: `expiry::run_once` callers in the hourly loop write a `system` `expiry_sweep` job, already `succeeded`, only when expired count ≥ 1.
  - `POST /admin/broker-connections/{code}/universe/enrich` → `202 {"job_id"}`: an `enrichment` job running the OpenFIGI enricher over that broker's instruments (`OpenFigiEnricher`, as in `src/setup/brokers.rs`), logging per batch (operation_id `enrich_connection_universe`, summary "Enrich a broker connection's instruments with FIGIs"). API-only — no cockpit button.

- [ ] **Step 1: Write the failing tests** (`#[ignore]` where DB): `list_jobs_returns_newest_first_and_honours_limit`; `get_job_returns_its_log_oldest_first`; `get_job_404s_an_unknown_id`; `expiry_sweep_endpoint_answers_202_with_a_job` (job ends `succeeded`, `result` has the old fields); `a_scheduled_sweep_that_expires_nothing_writes_no_job`; `enrich_endpoint_answers_202_with_an_enrichment_job`.
- [ ] **Step 2: Run to verify failure.**
- [ ] **Step 3: Implement**; regenerate `docs/openapi.json`. Update any cockpit caller of the two converted endpoints (`grep -rn "symbology/backfill\|expiry-sweep" cockpit/src`) to read the job id.
- [ ] **Step 4: Run to verify pass** + full suite; `cd cockpit && npx tsc --noEmit && npm run build`.
- [ ] **Step 5: Commit** — `git commit -m "feat(api): job list/detail; backfill and expiry sweep run as jobs"`

### Task 6: Cockpit — jobs indicator and Task Center

**Files:**
- Create: `cockpit/src/components/JobsIndicator.tsx`, `cockpit/src/components/TaskCenter.tsx`, `cockpit/src/api/jobs.ts`
- Modify: `cockpit/src/App.tsx:49-59` (`AppShell.Header`), `cockpit/src/api/types.ts`

**Interfaces:**
- Consumes: Task 5 endpoints.
- Produces: `useJobs(): { jobs: JobRecord[]; running: number; failedSinceSeen: boolean; markSeen(): void }` (react-query, `refetchInterval` 2000 ms while any job is `queued`/`running`, else 15000 ms; "seen" = newest `finished_at` the user has opened the panel past, kept in `localStorage` with try/catch); `<TaskCenter opened onClose />`.

- [ ] **Step 1: Implement.** Indicator: hidden when `running == 0` and not `failedSinceSeen`; otherwise "Jobs · N running" (plus a red dot when a job failed since last opened); click opens a Mantine `Drawer` (right). Task Center rows, newest first: kind label (`universe_load` → "Instrument universe"), subject, status badge (colours from the blotter's palette), `Progress` bar when `progress_total` is set, duration, result summary or error; expanding a row fetches `GET /admin/jobs/{id}` and shows the log lines in a monospace block. A `universe_load` row that is `failed` or `interrupted` has a **Run again** button (POST the load endpoint for its subject).
- [ ] **Step 2: Verify.** `npx tsc --noEmit && npm run build`; then in a browser against the dev OMS: trigger `POST …/alpaca-paper/universe/load`, watch the indicator show "Jobs · 1 running" with progress, finish, and the row expand to its log. Screenshot both states.
- [ ] **Step 3: Commit** — `git commit -m "feat(cockpit): jobs indicator and Task Center"`

### Task 7: Cockpit — universe column and the post-save prompt

**Files:**
- Create: `cockpit/src/components/UniverseStatus.tsx`
- Modify: `cockpit/src/pages/BrokerConnections.tsx`, `cockpit/src/components/CredentialsPanel.tsx` (after a successful save, ~`:207`), `cockpit/src/pages/Instruments.tsx:30,69` (empty state text)

**Interfaces:**
- Consumes: Task 4 endpoints; Task 6's `useJobs` (to refresh status when a job finishes).

- [ ] **Step 1: Implement.**
  - Broker connections: an "Instrument universe" column rendering `UniverseStatus` per row — state with the spec's symbols (○ not loaded · ◐ loading with `done / total` and a progress bar · ● loaded with count and finished time · ◑ partial with reason · ✕ failed with error), and the action button: `not_loaded` → **Load** (disabled with tooltip "Add credentials first" unless credentials are configured), `loaded` → **Reload**, `partial` → **Load again**, `failed` → **Retry**, `loading` → none. A `409` shows "Already loading" and opens the Task Center.
  - Credentials panel: after a successful save, `GET …/universe`; if `not_loaded`, open a modal with the exact copy from Global Constraints (broker display name from the connection's `broker_code`; count text "About 14,400 stocks and ETFs" for `ALPACA`, "About 2,000 spot pairs" for `BINANCE`) and buttons **Later** / **Load now**. Load now → POST load, close, show a notification linking to the Task Center.
  - Instruments page empty state: "No instruments yet — load a broker's universe from Broker connections." (replacing the `make sync-broker` text).
- [ ] **Step 2: Verify.** `npx tsc --noEmit && npm run build`; browser walk-through on a throwaway install (fresh Postgres, as in the docs walkthrough): row shows ○ Not loaded with Load disabled → owner enters Alpaca paper credentials → prompt appears with the exact copy → Load now → row goes ◐ with progress → ● Loaded 14,4xx. Then stop the OMS mid-load once and restart: row shows ◑ Partial "interrupted" → Load again works. Screenshots of each state.
- [ ] **Step 3: Commit** — `git commit -m "feat(cockpit): instrument universe status, load actions, post-save prompt"`

### Task 8: Docs, cockpit-first guide, release v0.4.0

**Files:**
- Modify: `docs-site/src/content/docs/getting-started/first-paper-trade.md` (rewrite), `docs-site/src/content/docs/integrations/alpaca.md` (loading instruments), `docs-site/src/content/docs/configuration/reference.md` (drop `OMS_SYNC_UNDERLYINGS`; `OMS_SYNC_ON_BOOT` = per-connection boot load), `docs-site/src/content/docs/operations/troubleshooting.md` (job failures), `Cargo.toml` (`version = "0.4.0"`)

- [ ] **Step 1: Rewrite the guide cockpit-first** per spec D: Alpaca keys → Broker connections → alpaca-paper → Credentials (Test, Save) → the prompt → Load now → watch it in Jobs → Principals → Portfolios → Accounts (portfolio's default account) → Api page (generate token for alice/main) → one `curl` submit and the Python snippet. Remote line: `ssh -L 3001:localhost:3001 yourserver`. The current curl setup moves to a final "Prefer the API?" section, updated for the new load endpoint (`POST /admin/broker-connections/alpaca-paper/universe/load`).
- [ ] **Step 2: Verify by running it** on a throwaway install with the owner entering Alpaca paper keys (same procedure as the original walkthrough); every screen and output in the guide matches what happened; cancel any order left working. `cd docs-site && npm run check` — all tests pass (the CLI-reference test still passes after `oms setup sync-broker` usages are reduced).
- [ ] **Step 3: Commit** — `git commit -m "docs: cockpit-first first paper trade; universe and jobs"`
- [ ] **Step 4: Release.** Bump to `0.4.0`, `cargo build`, `cargo test --bin oms`, commit `chore(release): v0.4.0`, tag `v0.4.0`. Push `main` and the tag **in the same push** as the docs changes. Watch Pages, Build and Test, Release, Install smoke test to green; `oms update --check` on a v0.3.0 install reports 0.4.0.
