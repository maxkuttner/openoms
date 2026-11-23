# Reported Strategies Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Traders create self-managed (`REPORTED`) strategies, get a reporting token, and POST fill batches that the OMS dedupes and books into the strategy's auto-created portfolio, with unbookable fills becoming breaks.

**Architecture:** New `src/reported/` module owns the strategy creation, token, ingest and break-adopt paths. A dedicated `reported_fill` ledger feeds `positions::persist_fill` directly and is added as a branch of `positions::replay_sql` so rebuilds include it. `recon_break` gains `source`/`strategy_code` so the existing Breaks UI shows reported breaks.

**Tech Stack:** Rust (axum, sqlx, Postgres), React/TS cockpit (vitest).

**Spec:** `docs/superpowers/specs/2026-11-23-reported-strategies-design.md` (source: `STRATEGY2.md`)

## Global Constraints

- Server side only; no Python SDK work.
- No behaviour change for `VIA_OMS` / `COLOCATED` strategies; no colo code removed.
- Endpoint: `POST /api/v1/strategies/{code}/fills`, `GET /api/v1/strategies/{code}/config`, bearer reporting token; payload `schema_version: 1`.
- Dedupe key per fill: `(strategy_code, venue, account_ref, exec_id)`; batch idempotency: `(strategy_code, batch_id)`.
- Batches accepted per fill: outcome per fill is `booked | duplicate | break`; a bad fill never rejects the batch.
- Sequence gap: flag + alarm only. Unknown fee asset: book, `FeeEffect::Other`, alarm.
- Token: hash stored only, shown once; rotation leaves the previous token valid 24h.
- Portfolio access for people only through groups (migration 0065); never direct grants.
- UI labels path 2 "self-managed / unverified"; reported breaks say "reported, not verified".
- DB tests: run with `--ignored --skip real_docker` (never without the skip).
- Single user prerelease: no compat shims.

## Review Focus

1. Same `exec_id` resent with a different qty/price → must not double-book or overwrite; becomes a `CONFLICTING_DUPLICATE` break. (Task 4 test `conflicting_duplicate_becomes_break`.)
2. Late fill after a full `rebuild_positions` → reported fills survive rebuilds. (Task 3 test `rebuild_keeps_reported_fills`.)
3. Token for strategy A used on `/strategies/B/fills` → 403, nothing booked. (Task 4 test `token_of_other_strategy_is_forbidden`.)
4. Same batch_id resent concurrently → books once. (Task 4 test `concurrent_same_batch_books_once`.)
5. Admin removes the owner from the strategy's group → refused. (Task 6 test `owner_cannot_be_removed_from_strategy_group`.)

---

### Task 1: Schema

**Files:**
- Create: `db/migrations/ods/oms/0068_REPORTED_STRATEGIES.sql`
- Test: `src/reported/schema_tests.rs`

**Interfaces:**
- Produces tables: `strategy_reporting_token(id uuid pk, strategy_code text fk, token_hash text unique, created_at, revoked_at null, expires_at null)`; `reported_batch(strategy_code, batch_id, sequence bigint null, received_at, booked int, duplicate int, broken int, sequence_gap bool, response_json jsonb, payload_json jsonb, pk(strategy_code,batch_id))`; `reported_fill(id uuid pk, strategy_code, batch_id, venue, account_ref, exec_id, client_order_id null, symbol, instrument_id fk, side, qty numeric, price numeric, fee numeric null, fee_asset null, fee_effect text null, trade_time timestamptz, booked_at, raw_json jsonb, unique(strategy_code,venue,account_ref,exec_id))`.
- `strategy.mode` allows `'REPORTED'`; `principal_id`, `sender_comp_id`, `target_comp_id` nullable with `CHECK (mode = 'REPORTED' OR (principal_id IS NOT NULL AND sender_comp_id IS NOT NULL AND target_comp_id IS NOT NULL))`; new `strategy.stale_after_secs INT NOT NULL DEFAULT 300`.
- `user_group.strategy_code TEXT NULL UNIQUE REFERENCES strategy(code)`.
- `recon_break`: `source TEXT NOT NULL DEFAULT 'RECONCILED' CHECK (source IN ('RECONCILED','REPORTED'))`, `strategy_code` null fk, `reported_fill_json jsonb null`; `broker_connection_code`, `account_id`, `venue_order_id` nullable with CHECK they're set when `source='RECONCILED'` and `strategy_code`+`reported_fill_json` set when `REPORTED`. Unique open index `recon_break_reported_one_open ON recon_break (strategy_code, (reported_fill_json->>'venue'), (reported_fill_json->>'account_ref'), (reported_fill_json->>'exec_id')) WHERE status='open' AND source='REPORTED'`.

- [ ] **Step 1: Write failing tests** `existing_strategies_still_valid_after_migration` (seed a VIA_OMS strategy with `apply_matching(< "0068_")`, apply all, row unchanged); `reported_strategy_needs_no_principal` (insert REPORTED row with NULL principal/comp ids succeeds; VIA_OMS with NULL principal fails); `reported_fill_dedupe_key_unique`; `reported_break_needs_strategy`.
- [ ] **Step 2: Run** `cargo test -p oms reported::schema_tests -- --ignored --skip real_docker` → FAIL (table missing).
- [ ] **Step 3: Write migration** as in Interfaces. Register `mod reported;` in `src/main.rs` with `src/reported/mod.rs` declaring `#[cfg(test)] mod schema_tests;`.
- [ ] **Step 4: Run** same command → PASS; also `cargo test -- --ignored --skip real_docker ingest::schema_tests` → PASS.
- [ ] **Step 5: Commit** `feat(db): reported strategy schema`

### Task 2: Trader creates a reported strategy (portfolio + private group + token)

**Files:**
- Create: `src/reported/strategy.rs`, `src/reported/token.rs`
- Modify: `src/main.rs` (routes next to `/strategies` at ~L940)
- Test: in-file `mod tests`

**Interfaces:**
- Consumes: Task 1 tables.
- Produces:
  - `pub async fn create(State<AppState>, Extension<AuthContext>, Json<CreateReported>) -> Result<Json<ReportedCreated>, ApiError>` at `POST /strategies/reported`. `CreateReported { code: String, name: String, base_currency: Option<String> }`. `ReportedCreated { code, portfolio_id: Uuid, group_id: Uuid, token: String }`.
  - `pub async fn rotate_token(..., Path(code)) -> Json<TokenCreated { token: String }>` at `POST /strategies/reported/:code/token`; `pub async fn revoke_token(...)` at `DELETE /strategies/reported/:code/token`.
  - `pub async fn list_mine(...) -> Json<Vec<ReportedStrategy>>` at `GET /strategies/reported` (owner's own; admin sees all).
  - `pub async fn authenticate(pool: &PgPool, bearer: &str) -> Result<String /*strategy_code*/, AuthFail>` in `token.rs`: valid iff not revoked and (`expires_at` null or > now) and strategy `state='ACTIVE'`.
- One transaction in `create`: portfolio `code = "RPT-" + code` status ACTIVE; `user_group { name = "strategy " + code, strategy_code = code }`; member = caller; `user_group_portfolio access='manage'`; strategy `mode='REPORTED', owner_principal_id = caller, portfolio_id`; first token. Any HUMAN principal may call it (no `StrategiesManage` check). Token format: reuse `generate_key_material` from `src/admin.rs`; store hash the same way trading tokens do.
- Rotate: old active token gets `expires_at = now() + 24h`; at most one token in grace (rotating again revokes the graced one). Owner or admin only, else 403.

- [ ] **Step 1: Write failing tests** `trader_creates_reported_strategy_with_private_portfolio` (portfolio_access has caller=manage; another HUMAN has no row); `token_is_shown_once_and_authenticates` (`authenticate` returns code; list_mine response has no token field); `rotate_keeps_old_token_24h` (old token works; with `expires_at` set to past it fails); `revoke_stops_auth`; `non_owner_cannot_rotate` (403); `duplicate_code_is_conflict` (409, nothing created).
- [ ] **Step 2: Run** `cargo test reported::strategy -- --ignored --skip real_docker` → FAIL.
- [ ] **Step 3: Implement** functions above; wire routes.
- [ ] **Step 4: Run** → PASS; plus `cargo test strategies::trader_api -- --ignored --skip real_docker` → PASS (existing `trader_cannot_create_strategy` unchanged).
- [ ] **Step 5: Commit** `feat(reported): trader-created reported strategies with tokens`

### Task 3: Positions read reported fills

**Files:**
- Modify: `src/positions.rs` (`replay_sql` L221-247, `replay` L295-346, `persist_fee` L480)
- Test: `src/positions.rs` `mod tests`

**Interfaces:**
- Produces: `replay_sql` gains a `UNION ALL` branch `SELECT s.portfolio_id, rf.instrument_id, rf.side, 'reported_fill', NULL, rf.qty::float8, rf.price::float8, rf.trade_time, rf.exec_id, false, NULL FROM reported_fill rf JOIN strategy s ON s.code = rf.strategy_code`, and a fee branch `'reported_fee'` (is_fee true, qty = fee) when `fee_effect IN ('Quote','Base')`. `replay` handles both kinds.
- `pub async fn persist_reported_fee(conn, portfolio_id: Uuid, instrument_id: &str, exec_id: &str, trade_time: DateTime<Utc>, amount: f64, effect: FeeEffect) -> Result<bool, sqlx::Error>`: same late/incremental logic as `persist_fee` without an order id.

- [ ] **Step 1: Write failing tests** `rebuild_keeps_reported_fills` (insert reported_fill BUY 1@100 + SELL 1@110, `rebuild_positions_scoped`, realized_pnl = 10); `late_reported_fill_rebuilds_in_trade_time_order` (persist fill t2 then t1 via `persist_fill`; result equals rebuild); `reported_quote_fee_reduces_realized`.
- [ ] **Step 2: Run** `cargo test positions:: -- --ignored --skip real_docker` → new tests FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** → all positions tests PASS.
- [ ] **Step 5: Commit** `feat(positions): include reported fills in replay`

### Task 4: Fill ingest endpoint

**Files:**
- Create: `src/reported/ingest.rs`, `src/reported/api.rs`
- Modify: `src/main.rs` (mount `/api/v1/strategies/:code/{fills,config}` outside the session-auth layer)
- Test: `src/reported/ingest_tests.rs`

**Interfaces:**
- Consumes: `token::authenticate`, `positions::persist_fill`, `positions::persist_reported_fee`.
- Produces:
  - `FillBatch { schema_version: u32, batch_id: String, sequence: Option<i64>, fills: Vec<serde_json::Value> }` (fills kept raw so a malformed one becomes a break, not a 400).
  - `ReportedFill { venue, account_ref, exec_id, client_order_id: Option, symbol, side: OrderSide, qty: f64, price: f64, fee: Option<f64>, fee_asset: Option<String>, trade_time: DateTime<Utc> }`; unknown fields ignored (incl. any portfolio/book).
  - `pub async fn ingest(pool: &PgPool, strategy_code: &str, batch: FillBatch) -> Result<BatchResult, IngestError>`; `BatchResult { batch_id, results: Vec<FillOutcome { exec_id: Option<String>, outcome: "booked"|"duplicate"|"break", break_id: Option<Uuid> }>, sequence_gap: bool }`.
  - `pub(crate) async fn book_fill(tx, strategy_code, portfolio_id, instrument_id, fill: &ReportedFill, batch_id) -> Result<Outcome, sqlx::Error>` (reused by Task 5 adopt).
  - `pub(crate) async fn open_reported_break(tx, strategy_code, kind: &str, raw: &Value) -> Result<Uuid, sqlx::Error>` (`ON CONFLICT` on the open index → return existing id). Kinds: `UNKNOWN_INSTRUMENT`, `INVALID_FILL`, `CONFLICTING_DUPLICATE`.
- HTTP: 401 bad/revoked token; 403 path code ≠ token strategy; 400 `schema_version != 1`; 413 `fills.len() > 500`; 429 over per-strategy bucket (default 20 batches/s, burst 40, in-memory `DashMap<String, Bucket>` in `AppState` or module static). `GET config` returns `{ strategy, portfolio_code, schema_version: 1, max_batch_fills: 500, max_batches_per_sec: 20 }`.
- Flow: lock `reported_batch` by inserting `ON CONFLICT DO NOTHING`; conflict → return stored `response_json`. Each fill in a savepoint. Instrument via `broker_instrument (broker_code = venue, broker_symbol = symbol)`. Fee effect: fee_asset = instrument currency → Quote, = base (`native_id`/symbol base) → Base, else Other + `tracing::warn!` alarm. Sequence gap = `sequence` present and ≠ previous max + 1 → `warn!` + flag.
- After commit, update `strategy` last-report (via `MAX(received_at)` of `reported_batch`; no extra column).

- [ ] **Step 1: Write failing tests**
  - `fill_books_into_strategy_portfolio` (BUY 0.01@61000.5 → position net 0.01 avg 61000.5)
  - `same_batch_twice_books_once`, `same_fill_in_new_batch_is_duplicate`, `concurrent_same_batch_books_once` (two `tokio::join!` ingests)
  - `late_fill_placed_at_trade_time`
  - `cross_venue_aggregates_and_keeps_venue` (BINANCE + BYBIT same instrument → one position; two reported_fill rows with their venues)
  - `unknown_instrument_becomes_break_rest_book` (3 fills, middle unknown → outcomes booked/break/booked)
  - `malformed_fill_becomes_invalid_break`, `conflicting_duplicate_becomes_break`
  - `retry_does_not_duplicate_break`
  - `bnb_fee_books_with_other_effect` (position unaffected by fee; `fee_effect='Other'`)
  - `sequence_gap_flagged_not_rejected`
  - `payload_book_field_ignored`
  - HTTP-level (axum `oneshot`): `revoked_token_is_401`, `token_of_other_strategy_is_forbidden`, `wrong_schema_version_is_400`, `oversized_batch_is_413`.
- [ ] **Step 2: Run** `cargo test reported:: -- --ignored --skip real_docker` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** → PASS; `cargo clippy --all-targets -- -D warnings` clean.
- [ ] **Step 5: Commit** `feat(reported): fill ingest endpoint`

### Task 5: Reported breaks in admin recon API

**Files:**
- Modify: `src/recon_admin_api.rs` (`list_breaks` L321, `adopt_break` L388, `dismiss_break` L450)
- Test: in-file tests

**Interfaces:**
- Consumes: `ingest::book_fill`.
- Produces: `ReconBreak` gains `source: String`, `strategy_code: Option<String>`, `reported_fill: Option<Value>`; connection/account fields become `Option`. `BreakFilter` gains `source: Option<String>`. Adopt body for REPORTED: `{ instrument_id: String, portfolio_id: Option<Uuid> }` (default = strategy's portfolio; a different one = explicit rebook, recorded in audit). Adopt books exactly the stored fill once (break → `resolved`; second adopt → 409). Optional `map_symbol: bool` (default true) inserts `broker_instrument(venue, symbol → instrument_id)` so later fills book.

- [ ] **Step 1: Write failing tests** `adopt_reported_break_books_once`, `adopted_mapping_books_later_fills`, `dismiss_reported_break_books_nothing`, `list_breaks_filters_by_source`, existing recon break tests still pass.
- [ ] **Step 2: Run** `cargo test recon_admin_api -- --ignored --skip real_docker` → new FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(recon): adopt and dismiss reported breaks`

### Task 6: Strategy groups in admin groups API

**Files:**
- Modify: `src/groups_api.rs` (`list_groups` L121, `update_group` L190, `delete_group` L212, `set_members` L228, `set_portfolios` L264, `set_user_groups` L299)
- Test: in-file tests

**Interfaces:**
- Produces: `Group` gains `strategy_code: Option<String>`. Rules for a group with `strategy_code`: admin may add/remove members with any access level path already used; owner (`strategy.owner_principal_id`) must stay a member (`set_members`/`set_user_groups` → 422 "owner must stay in strategy group"); `delete_group` → 422; `set_portfolios` must keep the strategy portfolio → 422.

- [ ] **Step 1: Write failing tests** `admin_adds_person_to_strategy_group_grants_access`, `owner_cannot_be_removed_from_strategy_group`, `strategy_group_cannot_be_deleted`, `set_user_groups_keeps_owner_in_strategy_group`.
- [ ] **Step 2: Run** `cargo test groups_api -- --ignored --skip real_docker` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(groups): protect strategy-owned groups`

### Task 7: Reported strategy status API

**Files:**
- Modify: `src/reported/api.rs`, `src/main.rs`
- Test: in-file tests

**Interfaces:**
- Produces (session auth; owner, group members with view, or admin):
  - `GET /strategies/reported/:code/status` → `{ last_report_at: Option<DateTime>, stale: bool /* now - last > stale_after_secs */, last_sequence: Option<i64>, gaps: Vec<{ batch_id, sequence, expected }>, open_breaks: i64 }`
  - `GET /strategies/reported/:code/fills?limit&before` → reported fills, newest trade_time first.
  - `GET /strategies/reported/:code/batches?limit` → batches with counts.
  - `PATCH /strategies/reported/:code { stale_after_secs?: i32 }` (owner/admin).

- [ ] **Step 1: Write failing tests** `status_reports_stale_after_threshold`, `status_lists_sequence_gaps`, `fills_visible_to_group_member_not_stranger` (403).
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(reported): status, fills and batches views`

### Task 8: Cockpit

**Files:**
- Create: `cockpit/src/pages/trading/ReportedStrategies.tsx`, `cockpit/src/reported.ts`, `cockpit/src/reported.test.ts`
- Modify: `cockpit/src/nav.ts` (add trading nav entry), `cockpit/src/pages/admin/Breaks.tsx` + `cockpit/src/recon.ts` (source column/filter, reported adopt form), `cockpit/src/pages/admin/Groups.tsx` (badge "strategy <code>", disable delete)

**Interfaces:**
- Consumes: Task 2/5/6/7 endpoints.
- `reported.ts`: `staleLabel(status): string`, `gapSummary(gaps): string`, typed fetchers.
- Page: create form (code, name) → modal shows token once with copy button + "This token is shown once"; list of own strategies with "Self-managed / unverified" badge, last report, stale warning, gaps, open breaks, fills table (venue, account_ref, exec_id, symbol, side, qty, price, fee, trade_time); rotate/revoke buttons. Breaks: REPORTED rows show "Reported, not verified".

- [ ] **Step 1: Write failing vitest** `staleLabel` ("never reported", "stale 12m", "ok"), `gapSummary`, nav entry present.
- [ ] **Step 2: Run** `cd cockpit && npm test -- reported nav` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `npm test && npm run build` → PASS.
- [ ] **Step 5: Commit** `feat(cockpit): reported strategies page and reported breaks`

### Task 9: Docs and end-to-end check

**Files:**
- Create: `docs-site/.../strategies/reported.md` (beside the co-located page; follow existing path), raw HTTP example with `curl`
- Modify: co-located docs page header → "legacy" note; `STRATEGY2.md` status line → "implemented server-side, SDK pending"

- [ ] **Step 1:** Run full suite `cargo test -- --ignored --skip real_docker` → PASS.
- [ ] **Step 2:** Manual: start OMS locally, create strategy in cockpit, `curl` a batch with the token, see fill + position; resend → duplicate; unknown symbol → break, adopt → booked; revoke → 401.
- [ ] **Step 3:** `graft build`.
- [ ] **Step 4: Commit** `docs: reported strategies`
