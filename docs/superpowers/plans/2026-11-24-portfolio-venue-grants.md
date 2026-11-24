# Trading Grants Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A generic, extendable grant system decides what a portfolio may trade (venues first), routing needs no user-visible accounts, and a new portfolio comes with its own view group.

**Architecture:** `trading_grant` rows (subject, kind, jsonb params) map to a Rust `enum Grant`; `grants::check` runs in order entry before the risk engine. A venue grant also maintains the hidden `account` row orders route through. Portfolio creation adds group `portfolio <code>`.

**Tech Stack:** Rust (axum, sqlx, serde, Postgres), React + shadcn/ui cockpit (vitest), Astro docs.

**Spec:** `docs/superpowers/specs/2026-11-24-portfolio-venue-grants-design.md`

## Global Constraints

- Worktree `/Users/max/workarea/oms-cockpit-redesign`, branch `feat/cockpit-redesign`.
- Rust env: `OPENSSL_ROOT_DIR=/opt/homebrew/opt/openssl@3 OPENSSL_DIR=/opt/homebrew/opt/openssl@3`.
- DB tests only with `OMS_RELOAD_TEST_DATABASE_URL=postgres://postgres:pw@127.0.0.1:55432/ods`; always `--skip real_docker`; never `.env`/`dev_pool()`.
- Commit in a separate Bash call, subject-only message.
- Cockpit: shadcn/ui only.
- Table `trading_grant`; `subject_type` only `'portfolio'`; status `'ACTIVE'|'REVOKED'`.
- Serde shape: `{"kind":"venue","params":{"connection":"alpaca-paper","account_ref":null}}`.
- Absent-kind default: `venue` deny, all others allow.
- Group `portfolio <code>`, access `view`. Hidden account `code` `<portfolio code>-<connection>`; ref `""` unless broker `IBKR`/`IBKR_TWS` (required, 422 `IBKR needs its account`).
- Error copy: `venue not granted to this portfolio`, `choose a venue`, `venue is used by strategy <code>`.
- Next migration number: `0073`; `embeds_every_migration` count 73 → 74.

## Review Focus

1. Two portfolios granted one Alpaca connection: an external fill resolves to one deterministic account row (Task 5 `resolve_account_is_deterministic_with_shared_ref`).
2. Revoking a venue with working orders: old orders still load and book fills; new submits refused (Task 4 `revoked_venue_refuses_submit_but_keeps_history`).
3. Re-granting a revoked venue reuses grant and account rows, no unique violation (Task 3 `regrant_reactivates_rows`).
4. Unknown `kind` or malformed `params` in a PUT → 422, nothing written (Task 3 `bad_grant_is_rejected_atomically`).
5. Portfolio created via host token (no human) still gets its group, with no member (Task 1 `host_created_portfolio_gets_empty_owner_group`).

---

### Task 1: Owner group on portfolio creation

**Files:** Modify `src/admin.rs` (`create_portfolio`, ~L113); tests in its `#[cfg(test)]` module.

**Interfaces:**
- Produces: `create_portfolio(State, Extension<AuthContext>, Json<CreatePortfolio>)`; in one transaction inserts the portfolio, `user_group(name 'portfolio <code>')`, `user_group_portfolio(access 'view')`, and the member when `ctx.principal_id` is a `HUMAN` principal; audit `group.created`.

- [ ] **Step 1:** Failing DB tests `admin_created_portfolio_gets_owner_group` (creator sole member; `portfolio_access` can_view true, can_trade false) and `host_created_portfolio_gets_empty_owner_group`.
- [ ] **Step 2:** `cargo test --bin oms admin::tests -- --ignored --skip real_docker` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Same command → PASS.
- [ ] **Step 5:** Commit `feat(admin): new portfolio gets its own view group`.

### Task 2: Grant model and check (pure)

**Files:** Create `src/grants/mod.rs` (types + check, unit tests); add `mod grants;` in `src/main.rs`.

**Interfaces:**
- Produces:
  - `#[serde(tag = "kind", content = "params", rename_all = "snake_case")] pub enum Grant { Venue { connection: String, account_ref: Option<String> } }`
  - `pub enum Absent { Deny, Allow }`; `impl Grant { pub fn kind(&self) -> &'static str; pub fn when_absent(kind: &str) -> Absent; fn admits(&self, f: &OrderFacts) -> bool }`
  - `pub struct OrderFacts { pub connection: String, pub broker_code: String, pub instrument_class: Option<String>, pub at: DateTime<Utc> }`
  - `pub struct Denied(pub String)`; `pub fn check(f: &OrderFacts, grants: &[Grant]) -> Result<(), Denied>`: for every kind in the registry `KINDS: &[&str] = &["venue"]`, grants of that kind present → at least one admits; absent → apply `when_absent`. Venue denial message `venue not granted to this portfolio`.

- [ ] **Step 1:** Failing unit tests: `no_venue_grant_denies`, `matching_venue_admits`, `other_venue_denies`, `serde_round_trip_venue` (exact JSON from Global Constraints), `unknown_kind_fails_to_parse`.
- [ ] **Step 2:** `cargo test --bin oms grants::` → FAIL.
- [ ] **Step 3:** Implement. Doc comment on `Grant` lists the future shapes `InstrumentClass { connection, class }` and `Hours { tz, days, from, to }` and the four places a new kind touches (variant, `admits`, `KINDS`/`when_absent`, UI section).
- [ ] **Step 4:** → PASS.
- [ ] **Step 5:** Commit `feat(grants): generic grant model and check`.

### Task 3: Grants table and admin API

**Files:** Create `db/migrations/ods/oms/0073_TRADING_GRANTS.sql`, `src/grants/store.rs`; modify `src/main.rs` (route), `src/setup/database/assets.rs` (count 74).

**Interfaces:**
- Migration: `CREATE TABLE trading_grant (...)` per spec, unique index on `(subject_type, subject_id, kind, params)`; `ALTER TABLE account DROP CONSTRAINT <unique (broker_connection_code, external_account_ref)>` (look up the name with `\d account`); `CREATE UNIQUE INDEX account_portfolio_connection ON account (portfolio_id, broker_connection_code) WHERE portfolio_id IS NOT NULL`.
- Produces: `pub async fn active_for_portfolio(conn, portfolio: Uuid) -> sqlx::Result<Vec<Grant>>`; `pub async fn set_for_portfolio(tx, ctx, portfolio, grants: Vec<Grant>) -> Result<(), ApiError>` (diff against active set; insert/reactivate/revoke; for `Venue` upsert/close the hidden `account` row; refuse revoking a venue whose account row is in `strategy_connection`; audit `grants.set`).
- Routes `GET/PUT /admin/portfolios/:id/grants` (body/response `Vec<Grant>`); GET also returns, for the UI, `connections: [{code, broker_code, environment}]` → response `{ grants, connections }`.

- [ ] **Step 1:** Failing DB tests: `venue_grant_creates_account_row`, `two_portfolios_share_a_connection`, `revoke_closes_account_row`, `regrant_reactivates_rows`, `ibkr_needs_account_ref`, `cannot_revoke_venue_used_by_strategy`, `bad_grant_is_rejected_atomically` (PUT `[{"kind":"teleport"}]` → 422, prior grants unchanged).
- [ ] **Step 2:** `cargo test --bin oms grants:: -- --ignored --skip real_docker` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Those tests and `cargo test --bin oms embeds_every_migration` → PASS.
- [ ] **Step 5:** Commit `feat(grants): store and admin API, venue grants route orders`.

### Task 4: Order entry enforces grants

**Files:** Modify `src/order_entry.rs` (~L490-570 account resolution, before `check_submit` ~L756), `src/handlers.rs` (`get_portfolio_venues`, ~L677-740).

**Interfaces:**
- Consumes: `grants::{check, OrderFacts}`, `store::active_for_portfolio`.
- Produces: submit without `account_id` → the portfolio's single ACTIVE venue account listing the instrument; none → 422 `venue not granted to this portfolio`; several → 422 `choose a venue`. With `account_id`, it must be one of the portfolio's ACTIVE venue accounts. Then `grants::check` (422 with `Denied` text). `default_account_id` no longer read. Strategy-origin orders (`strategy_route`) keep their route and skip `grants::check`. `/portfolios/:id/venues` returns only granted connections.

- [ ] **Step 1:** Failing DB tests: `submit_routes_to_single_venue`, `submit_with_two_venues_needs_choice`, `revoked_venue_refuses_submit_but_keeps_history`, `venues_lists_only_granted`.
- [ ] **Step 2:** `cargo test --bin oms order_entry:: -- --ignored --skip real_docker` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** → PASS; `cargo test --bin oms` (unit) green.
- [ ] **Step 5:** Commit `feat(orders): enforce trading grants`.

### Task 5: Shared venue accounts in ingest and recon

**Files:** Modify `src/ingest/apply.rs` (`resolve_account`, ~L197), `src/reconciler.rs` (`accounts`, ~L312).

**Interfaces:** exact match orders `(status = 'ACTIVE') DESC, created_at, id` and takes the first; single-account fallback counts distinct `external_account_ref`; `Reconciler::accounts` (Unfollowed) returns one row per distinct ref, same ordering.

- [ ] **Step 1:** Failing DB tests `resolve_account_is_deterministic_with_shared_ref`, `reconciler_pulls_shared_ref_once`.
- [ ] **Step 2:** → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test --bin oms ingest:: reconciler:: -- --ignored --skip real_docker` → PASS.
- [ ] **Step 5:** Commit `fix(recon): shared venue account resolves to one row`.

### Task 6: Cockpit grants dialog, accounts gone

**Files:** Create `cockpit/src/grants.ts` + `grants.test.ts`, `cockpit/src/components/admin/PortfolioGrants.tsx`; modify `cockpit/src/pages/admin/Portfolios.tsx` (drop `default_account_id`; row action **Grants**), `cockpit/src/App.tsx` + admin nav (remove Accounts); delete `cockpit/src/pages/admin/Accounts.tsx`.

**Interfaces:**
- Produces: `type Grant = { kind: "venue"; params: { connection: string; account_ref: string | null } }`; `venueGrants(picked: Set<string>, refs: Record<string,string>, brokers: Record<string,string>): Grant[]`; `needsRef(brokerCode: string): boolean` (true for `IBKR`, `IBKR_TWS`). Dialog: section **Venues** (Checkbox per connection `code · environment`, Input for IBKR account), Save → PUT, toast "Grants saved"; sections are an array so future kinds append one.

- [ ] **Step 1:** Failing vitest for `venueGrants` and `needsRef`.
- [ ] **Step 2:** `npm test` in `cockpit` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `npm test`, `npx tsc -b`, `npm run build` pass; `grep -rn "admin/accounts\|default_account" src` empty.
- [ ] **Step 5:** Commit `feat(cockpit): portfolio grants replace accounts`.

### Task 7: Docs

**Files:** `docs-site/src/content/docs/configuration/login.md` (portfolio group; grants), `docs-site/src/content/docs/trade/order-entry.md` (venues come from grants), any page describing OMS account setup (`grep -rni "account" docs-site/src/content/docs`; keep broker-side wording).

- [ ] **Step 1:** Edit; short "Grants" section: admin opens **Portfolios → Grants**, ticks venues; no venue = no trading.
- [ ] **Step 2:** `npm run build` and `npm test` in `docs-site` → complete, 0 fail.
- [ ] **Step 3:** Commit `docs: trading grants and portfolio groups`.
