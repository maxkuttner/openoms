# External Orders & Execution Reconciliation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Book every order and fill that happens on a connected venue account exactly once — OMS-placed, manual, or from other systems — through one shared ingest path fed by streams, intraday pulls and an end-of-day sweep.

**Architecture:** Connectors become venue I/O only: they emit normalized `VenueEvent`s (push) and implement `ExecutionSource` (pull). A per-connection `Reconciler` task in the OMS schedules pulls and owns cursors; everything lands in one idempotent ingest path inside the engine actor (`GatewayMsg::Venue`) that matches, adopts into a book by attribution rules, or records a break. Outbound stays as today (EventBus + Kafka outbox).

**Tech Stack:** Rust (tokio, sqlx/Postgres, quickfix), React cockpit (`cockpit/`), Astro docs (`docs-site/`).

**Spec:** `docs/superpowers/specs/2026-11-18-external-orders-reconciliation-design.md`

## Global Constraints

- Single-user prerelease: no compat shims. Old paths (`ExecEvent`/`VenueReport`, `src/recon_orders.rs`, colo follow booking) are deleted at the end, not wrapped.
- Engine actor stays the single writer of order state.
- Migrations: next file is `db/migrations/ods/oms/0066_*.sql`; new tables unprefixed; natural `code` PK for config tables, UUID for order/event-domain rows.
- Fill dedup key: `(broker_connection_code, account_id, exec_id)`. Fee dedup key unchanged (existing fee path).
- EOD lookback values, exactly: `1d`, `1w`, `1m`, `1y`, `5y`, `10y`.
- New account's intraday cursor starts at "now"; history only via EOD sweep.
- External policy values: `adopt` (default) | `break`.
- `multi_account = true` only for IBKR (TWS and FIX).
- DB tests run `--ignored`; always add `--skip real_docker`.
- Known limit, not in scope: `broker_connection` is `UNIQUE (broker_code, environment)`, so one Binance sub-account per environment until a follow-up lifts it.

## Review Focus

1. Same fill arriving by stream and by pull at the same moment → booked once (Task 4 test `concurrent_stream_and_pull_book_once`).
2. A pulled fill without client order id for an OMS-placed order (Binance `myTrades`, IBKR `reqExecutions` after restart) → matched by venue order id, not adopted as external (Task 3 test `fill_without_client_id_matches_by_venue_order_id`).
3. Fill seen before any order update on an external order → adopted at filled qty, later `OrderUpdate` grows qty without re-booking (Task 4 test `fill_first_then_update_grows_qty`).
4. Ingest fails mid-page (DB down) → cursor not advanced, next pull re-reads the page (Task 5 test `cursor_not_advanced_when_ingest_fails`).
5. Fills on an OMS-placed order summing past its quantity (venue overfill) → break `overfill`, not silently booked (Task 3 test `overfill_on_oms_order_is_a_break`).

---

## File Structure

| File | Responsibility |
|---|---|
| `db/migrations/ods/oms/0066_EXTERNAL_ORDERS_RECON.sql` | all new columns/tables + venue_fill seed |
| `crates/gateway/src/venue_event.rs` | `VenueEvent`, `VenueFill`, `VenueOrderUpdate`, `ExecutionSource` trait, `PullHints` |
| `crates/gateway/src/connectors/mod.rs` | `Manifest.multi_account`; `ExecStream` emits `VenueEvent`s |
| `src/ingest/decide.rs` | pure matching/attribution/dedup decision (functional core) |
| `src/ingest/apply.rs` | DB shell: adopt, book, record break, publish |
| `src/ingest/mod.rs` | `GatewayMsg::Venue` handling entry |
| `src/reconciler.rs` | per-connection task: stream forward, intraday pulls, cursors |
| `src/jobs/eod_sweep.rs` | end-of-day sweep job |
| `src/recon_admin_api.rs` | connection recon settings, attribution rules, breaks API |
| `cockpit/src/pages/admin/Breaks.tsx` | breaks list + adopt/dismiss |
| connectors: `binance/`, `alpaca/`, `bybit/`, `okx/`, `coinbase/`, `ibkr_tws/`, `ibkr/fix.rs`, `fix/dialect.rs` | emit `VenueEvent`, implement `ExecutionSource` |

---

### Task 1: Schema

**Files:**
- Create: `db/migrations/ods/oms/0066_EXTERNAL_ORDERS_RECON.sql`
- Test: `src/ingest/schema_tests.rs` (DB, `#[ignore]`)

**Interfaces:**
- Produces tables/columns:
  - `broker_connection.default_portfolio_id UUID NULL REFERENCES portfolio(id)`, `external_policy TEXT NOT NULL DEFAULT 'adopt' CHECK (IN ('adopt','break'))`, `eod_lookback TEXT NOT NULL DEFAULT '1w' CHECK (IN ('1d','1w','1m','1y','5y','10y'))`, `intraday_poll_secs INT NULL` (NULL = adapter hint)
  - `attribution_rule (id UUID PK, broker_connection_code TEXT FK, account_id UUID NULL FK, client_id_prefix TEXT NULL, portfolio_id UUID FK, priority INT NOT NULL, CHECK (account_id IS NOT NULL OR client_id_prefix IS NOT NULL))`
  - `venue_fill (broker_connection_code, account_id, exec_id, order_id UUID, booked_at, PRIMARY KEY (broker_connection_code, account_id, exec_id))`
  - `recon_cursor (broker_connection_code, account_id, scope TEXT, cursor TEXT, updated_at, PRIMARY KEY (broker_connection_code, account_id, scope))`
  - `recon_break (id UUID PK, broker_connection_code, account_id, venue_order_id TEXT, kind TEXT, events_json JSONB, status TEXT CHECK (IN ('open','resolved','dismissed')), created_at, resolved_at, resolved_by, UNIQUE (broker_connection_code, account_id, venue_order_id) WHERE status='open')`
  - unique index `order_state (broker_connection_code, account_id, external_order_id) WHERE external_order_id IS NOT NULL`
  - seed: `INSERT INTO venue_fill` from existing fill events in `order_event` (payload `execution_id`) joined to `order_state` — `ON CONFLICT DO NOTHING`.

- [ ] **Step 1: Write failing DB tests** — `migration_seeds_venue_fill_from_existing_fills` (seed an order + fill event pre-migration fixture, assert one `venue_fill` row with that exec_id); `duplicate_venue_fill_is_rejected` (second insert same key → unique violation); `one_open_break_per_venue_order` (second open break same key → violation; resolved + new open OK).
- [ ] **Step 2: Run** `cargo test -p oms schema_tests -- --ignored --skip real_docker` → FAIL (tables missing).
- [ ] **Step 3: Write the migration.** If existing data violates the `order_state` unique index (colo one-order-per-trade rows share a venue order id), delete those colo `EXTERNAL` per-trade rows and their events in the same migration — prerelease, they are replaced by re-adoption via the EOD sweep. Comment this in the SQL.
- [ ] **Step 4: Run** same command → PASS.
- [ ] **Step 5: Commit** `feat(db): schema for external orders and execution recon`.

---

### Task 2: Gateway contract

**Files:**
- Create: `crates/gateway/src/venue_event.rs`
- Modify: `crates/gateway/src/lib.rs` (export), `crates/gateway/src/connectors/mod.rs:49-81` (Manifest), `crates/gateway/src/contract.rs:76-82` (GatewayMsg)

**Interfaces:**
- Produces:
```rust
pub struct VenueFill { pub exec_id: String, pub venue_order_id: String, pub client_order_id: Option<String>,
  pub account_ref: String, pub symbol: String, pub buy: bool, pub qty: f64, pub price: f64,
  pub fee: Option<(f64, String)>, pub executed_at: Option<DateTime<Utc>> }
pub enum VenueOrderStatus { Working, Filled, Canceled, Rejected { reason: String }, Expired }
pub struct VenueOrderUpdate { pub venue_order_id: String, pub client_order_id: Option<String>, pub account_ref: String,
  pub symbol: String, pub buy: bool, pub order_type: String, pub qty: f64, pub limit_px: Option<f64>,
  pub status: VenueOrderStatus, pub cum_qty: f64, pub ts: Option<DateTime<Utc>> }
pub enum VenueEvent { Order(VenueOrderUpdate), Fill(VenueFill), Fee { exec_id: String, account_ref: String, amount: f64, currency: String }, Connected, Disconnected }
pub struct Cursor(pub String);
pub struct PullPage { pub fills: Vec<VenueFill>, pub next: Cursor, pub more: bool }
pub struct PullHints { pub intraday_secs: u64, pub max_history: Option<chrono::Duration>, pub per_symbol: bool }
#[async_trait] pub trait ExecutionSource: Send + Sync {
  async fn fills_since(&self, account_ref: &str, scope: &str, from: Option<&Cursor>, since: DateTime<Utc>) -> Result<PullPage, BrokerError>;
  async fn open_order_snapshot(&self, account_ref: &str) -> Result<Vec<VenueOrderUpdate>, BrokerError>;
  async fn scopes(&self, account_ref: &str) -> Result<Vec<String>, BrokerError>; // [""] unless per_symbol
  fn hints(&self) -> PullHints;
}
```
- `Manifest.multi_account: bool`; `Connected.source: Option<Arc<dyn ExecutionSource>>`
- `GatewayMsg::Venue { connection: String, event: VenueEvent }` (added beside `Exec`; `Exec` removed in Task 12)
- `EngineHandle::venue(&self, connection: &str, event: VenueEvent)`

- [ ] **Step 1: Failing test** `manifest_multi_account_only_ibkr`: iterate `connectors::all()`, assert `multi_account == matches!(code, "IBKR" | "IBKR_TWS")`.
- [ ] **Step 2: Run** `cargo test -p gateway manifest_multi_account_only_ibkr` → FAIL (no field).
- [ ] **Step 3: Implement** the types, trait, field (set per connector), `GatewayMsg::Venue`, `EngineHandle::venue`. `Connected.source` = `None` for all connectors for now. Engine `run` logs and ignores `Venue` until Task 4.
- [ ] **Step 4: Run** `cargo test -p gateway && cargo build` → PASS.
- [ ] **Step 5: Commit** `feat(gateway): VenueEvent and ExecutionSource contract`.

---

### Task 3: Ingest decision (pure core)

**Files:**
- Create: `src/ingest/mod.rs`, `src/ingest/decide.rs`

**Interfaces:**
- Consumes: Task 2 types.
- Produces:
```rust
pub struct KnownOrder { pub order_id: Uuid, pub qty: f64, pub cum_qty: f64, pub external: bool, pub terminal: bool }
pub struct Rule { pub account_id: Option<Uuid>, pub client_id_prefix: Option<String>, pub portfolio_id: Uuid, pub priority: i32 }
pub struct ConnPolicy { pub policy: Policy, pub default_portfolio: Option<Uuid>, pub rules: Vec<Rule> }
pub enum Policy { Adopt, Break }
pub struct Lookup { pub by_client_uuid: Option<KnownOrder>, pub by_venue_id: Option<KnownOrder>, pub fill_seen: bool, pub open_break: bool }
pub enum BreakKind { PolicyBreak, NoBook, Overfill, UnknownInstrument }
pub enum Decision { Duplicate, Apply { order_id: Uuid }, Grow { order_id: Uuid, new_qty: f64 },
  Adopt { portfolio_id: Uuid }, AttachToBreak, Break(BreakKind) }
pub fn decide(event: &VenueEvent, account_id: Uuid, lookup: &Lookup, policy: &ConnPolicy) -> Decision
pub fn attribute(client_id: Option<&str>, account_id: Uuid, policy: &ConnPolicy) -> Option<Uuid>
```
Order: duplicate (fill_seen) → open_break → client UUID match → venue id match → attribute (rules by ascending `priority`, a rule matches when every set field matches) → default → Break. Overfill: fill would push `cum_qty` past `qty` — OMS order ⇒ `Break(Overfill)`, external order ⇒ `Grow`.

- [ ] **Step 1: Failing tests** in `decide.rs`:
  - `seen_fill_is_duplicate` → `Decision::Duplicate`
  - `client_uuid_wins_over_venue_id`
  - `fill_without_client_id_matches_by_venue_order_id` → `Apply { order_id }` of the OMS order (Review Focus 2)
  - `rule_by_prefix_beats_default` (`p2-abc`, rule `p2-` → path2 portfolio)
  - `lower_priority_number_wins_on_overlap`
  - `no_rule_no_default_is_break_nobook` → `Break(NoBook)`
  - `policy_break_breaks_even_with_default` → `Break(PolicyBreak)`
  - `open_break_attaches` → `AttachToBreak`
  - `overfill_on_oms_order_is_a_break` (qty 10, cum 8, fill 3) → `Break(Overfill)` (Review Focus 5)
  - `overfill_on_external_order_grows` → `Grow { new_qty: 11.0 }`
- [ ] **Step 2: Run** `cargo test -p oms ingest::decide` → FAIL.
- [ ] **Step 3: Implement** `decide`, `attribute`.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(ingest): pure decision for venue events`.

---

### Task 4: Ingest apply (engine shell)

**Files:**
- Create: `src/ingest/apply.rs`
- Modify: `src/order_engine.rs:90-132` (handle `GatewayMsg::Venue`), `src/order_entry.rs:32` (`Origin::External` doc: any non-OMS order)
- Reuse/move: adopt-order construction from `src/colo/booking.rs:110-190` (SubmitOrder + RouteOrder via aggregate, `insert_order_state`, `append_events_in_tx`, `publish_appended`) and `ensure_instrument` (`booking.rs:~90`) into `apply.rs`, generalized over connector's `InstrumentProvider`.

**Interfaces:**
- Consumes: `decide` (Task 3), schema (Task 1).
- Produces: `pub async fn ingest(pool: &PgPool, bus: &EventBus, connection: &str, event: VenueEvent) -> Result<Decision, IngestError>`; `pub async fn load_lookup(...)`, `pub async fn load_policy(pool, connection) -> ConnPolicy`; account resolved from `(connection, account_ref)` via `account.external_account_ref` (single-account connectors: the connection's only account).
- Fill booking: insert `venue_fill` row and the fill event **in one transaction**; unique violation ⇒ `Duplicate`, nothing else written. Order status from `VenueOrderUpdate`: Canceled/Rejected/Expired/Filled close the order (booking any missing qty first, as `reports_for` does today).
- Adopted orders: `Origin::External`, actor `ingest:<connection>`, qty = update qty or fill qty, venue order id stored in `order_state.external_order_id`.
- Breaks: upsert into open `recon_break` keyed by venue order, appending the event to `events_json`.

- [ ] **Step 1: Failing DB tests** (`#[ignore]`):
  - `external_fill_adopts_into_default_book` — one order `origin='EXTERNAL'`, portfolio = default, cum = fill qty, position moved
  - `concurrent_stream_and_pull_book_once` — two `ingest` calls of the same fill via `tokio::join!` → one `venue_fill`, cum_qty = qty once (Review Focus 1)
  - `fill_first_then_update_grows_qty` — fill 2, then `Order{qty 5, cum 2}` → order qty 5, cum 2, one fill (Review Focus 3)
  - `oms_order_fill_by_client_uuid_books` — seeded routed OMS order, fill with its UUID → booked on it, no new order
  - `canceled_update_closes_with_missing_fill_booked` — cum 3 booked 1 → fill of 2 then Canceled
  - `break_collects_later_events` — policy break, fill then cancel → one open break with 2 events, no order
  - `late_fee_books_once` — Fill then `Fee` twice → one fee row
  - `unlisted_symbol_is_unknown_instrument_break` — provider lacks symbol → open break `UnknownInstrument`, no order
- [ ] **Step 2: Run** `cargo test -p oms ingest::apply -- --ignored --skip real_docker` → FAIL.
- [ ] **Step 3: Implement** `ingest` + engine wiring (`GatewayMsg::Venue` → `guarded(… ingest …)` like `apply_exec`).
- [ ] **Step 4: Run** → PASS; also `cargo test -p oms order_engine` still PASS.
- [ ] **Step 5: Commit** `feat(ingest): adopt, book and break venue events in the engine`.

---

### Task 5: Reconciler (stream + intraday pulls)

**Files:**
- Create: `src/reconciler.rs`
- Modify: `src/reload.rs:119` (`build_registry`: start/stop a reconciler per connection that has `Connected.source`), `crates/gateway/src/connectors/mod.rs:94` (`ExecStream` gets an `mpsc::Sender<VenueEvent>` sink instead of `EngineHandle`; keep the `EngineHandle` variant until Task 12 for unmigrated connectors)

**Interfaces:**
- Consumes: `ExecutionSource`, `EngineHandle::venue`, `recon_cursor`.
- Produces: `pub struct Reconciler`; `pub fn spawn(connection: String, source: Arc<dyn ExecutionSource>, events: mpsc::Receiver<VenueEvent>, engine: EngineHandle, pool: PgPool) -> ReconcilerHandle`; `ReconcilerHandle::pull_now(&self)`; `ReconcilerHandle::sweep(&self, since: DateTime<Utc>) -> SweepReport` (used by Task 6).
- Behavior: forward stream events; on `Connected`, on timer (`intraday_poll_secs` or `hints().intraday_secs`) and on `pull_now`: for each account × `scopes()`, page `fills_since` from cursor (missing cursor ⇒ insert cursor "now" and skip history), send each fill through ingest and **await** its result, advance cursor only after the page fully ingested; then `open_order_snapshot` → `VenueEvent::Order` per order, plus synthetic close for OMS-working orders absent from the snapshot (port of `recon_orders::diff` `ResolveTerminal`). Coalesce overlapping triggers (one running, one queued).
- Ingest needs a reply: `GatewayMsg::Venue` carries `Option<oneshot::Sender<Result<Decision, IngestError>>>`; stream path sends `None`.

- [ ] **Step 1: Failing tests** with a `FakeSource` (pages from a Vec, counts calls):
  - `new_account_starts_cursor_at_now_without_history`
  - `pages_until_more_is_false_and_saves_last_cursor`
  - `cursor_not_advanced_when_ingest_fails` (ingest returns Err on page 2 → cursor = end of page 1) (Review Focus 4)
  - `triggers_while_busy_coalesce_into_one_rerun`
  - `working_order_missing_from_snapshot_is_resolved`
- [ ] **Step 2: Run** `cargo test -p oms reconciler -- --ignored --skip real_docker` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(recon): per-connection reconciler with intraday pulls`.

---

### Task 6: End-of-day sweep

**Files:**
- Create: `src/jobs/eod_sweep.rs`
- Modify: `src/jobs/mod.rs` (register), `src/reconciler.rs` (`sweep`)

**Interfaces:**
- Consumes: `ReconcilerHandle::sweep`.
- Produces: `pub enum Lookback { D1, W1, M1, Y1, Y5, Y10 }` with `FromStr` for `1d|1w|1m|1y|5y|10y` and `fn duration(&self) -> chrono::Duration`; `pub struct SweepReport { pub requested_from: DateTime<Utc>, pub obtained_from: DateTime<Utc>, pub booked: u32, pub duplicates: u32, pub breaks: u32 }`.
- Sweep pulls with `since = now - lookback`, clamped to `hints().max_history`; does **not** move intraday cursors; runs daily at 00:15 UTC (my call) and on demand via API (Task 13). Report logged and stored as the job's last result.

- [ ] **Step 1: Failing tests:** `lookback_parses_all_six_values` (and rejects `2w`); `sweep_clamps_to_venue_max_history` (hint 7d, lookback 1m → `obtained_from = now-7d`); `sweep_counts_duplicates_not_rebooks` (FakeSource returns already-booked fill → `duplicates == 1, booked == 0`).
- [ ] **Step 2: Run** `cargo test -p oms eod_sweep -- --ignored --skip real_docker` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(recon): end-of-day sweep with configurable lookback`.

---

### Task 7: Binance connector

**Files:**
- Modify: `crates/gateway/src/connectors/binance/stream.rs:398-448` (`handle_execution_report` → `VenueEvent`s, no UUID-skip; also folds in `follow_events` `:112-158`), `binance/adapter.rs:184` (`my_trades` → `ExecutionSource::fills_since`, scope = symbol, cursor = last trade id; `scopes` = symbols with OMS orders or open orders on the account, as `colo::follow::recon_symbols` does), `binance/mod.rs` (`Connected.source`, ExecStream → sink)
- Test: `binance/stream.rs` tests with existing `testdata`

**Interfaces:** Consumes Task 2 types; produces `impl ExecutionSource for BinanceAdapter` with `hints() = { intraday_secs: 60, max_history: None, per_symbol: true }`.

- [ ] **Step 1: Failing tests:** `trade_on_non_uuid_client_id_is_a_venue_fill` (was skipped); `cancel_uses_orig_client_id_C`; `new_order_report_is_order_update_working`; `my_trades_page_maps_to_venue_fills_with_next_cursor`.
- [ ] **Step 2: Run** `cargo test -p gateway binance` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p gateway binance` → PASS.
- [ ] **Step 5: Commit** `feat(binance): emit venue events and pull fills`.

---

### Task 8: Alpaca connector

**Files:** `crates/gateway/src/connectors/alpaca/stream.rs`, `alpaca/adapter.rs`, `alpaca/mod.rs`
**Interfaces:** `ExecutionSource` via `GET /v2/account/activities?activity_types=FILL&after=…&page_token=…` (cursor = last activity id), `hints() = { 60, None, false }`.

- [ ] **Step 1: Failing tests:** `trade_update_fill_maps_to_venue_fill` (testdata), `dashboard_order_without_uuid_is_kept`, `activities_page_maps_and_cursors`.
- [ ] **Step 2: Run** `cargo test -p gateway alpaca` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(alpaca): emit venue events and pull fills`.

---

### Task 9: Bybit, OKX, Coinbase connectors

One commit per connector; same step cycle as Task 8.
- Bybit: private `order`/`execution` → events; pull `/v5/execution/list` (cursor = `nextPageCursor`, `startTime`), `max_history = 2y`.
- OKX: `orders` channel → events; pull `/api/v5/trade/fills` (3 days) and `/fills-history` (beyond), cursor = `billId`, `max_history = 1y`.
- Coinbase: `user` channel → events; pull `/api/v3/brokerage/orders/historical/fills` (cursor = API cursor).
Tests per connector: `<venue>_fill_without_oms_id_is_kept`, `<venue>_pull_page_maps_and_cursors` using `testdata`.

- [ ] **Step 1–5 per connector** (tests → fail → implement → pass → commit `feat(<venue>): emit venue events and pull fills`).

---

### Task 10: IBKR TWS connector

**Files:** `crates/gateway/src/connectors/ibkr_tws/client.rs:26,181` (client id from config), `ibkr_tws/stream.rs:74-157` (remove `unknown_order` fault; emit events), `ibkr_tws/adapter.rs` (`ExecutionSource` via `reqExecutions` filtered by account, `max_history = 7d`, `intraday_secs = 120`), `ibkr_tws/mod.rs` (credential field `master_client_id`, default `0`; when 0 call `reqAutoOpenOrders(true)`)

**Interfaces:** `VenueFill.account_ref` = IBKR `acctNumber`; `Fee` from `commissionReport` keyed by `exec_id`. Execution corrections (`<base>.NN`): emit a fresh pull trigger instead of a fill (keep `note_exec` logic) → reconciler `pull_now`.

- [ ] **Step 1: Failing tests:** `manual_tws_execution_is_a_venue_fill_not_a_fault` (health stays up); `execution_carries_account_ref`; `commission_maps_to_fee_event`; `corrected_execution_triggers_pull`; `client_id_comes_from_credentials`.
- [ ] **Step 2: Run** `cargo test -p gateway ibkr_tws` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(ibkr-tws): master client id, venue events, execution pulls`.

---

### Task 11: IBKR FIX account tag + FIX dialect

**Files:** `crates/gateway/src/fix/dialect.rs:407-476` (`parse_standard_exec_report` → `VenueEvent`; keep `is_ack` for submit), the FIX NewOrderSingle builder in `fix/dialect.rs` (add tag 1 `Account` from `external_account_ref` when non-empty), `connectors/ibkr/fix.rs`, `connectors/binance/fix.rs`.
FIX has no pull source here (`Connected.source = None`); recon for FIX uses the open-order snapshot only via REST where the venue has one (Binance FIX shares the Binance REST source).

- [ ] **Step 1: Failing tests:** `new_order_single_carries_account_tag_1`; `empty_account_ref_omits_tag_1`; `exec_report_with_non_uuid_clordid_is_kept` (was `None`).
- [ ] **Step 2: Run** `cargo test -p gateway fix` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(fix): account tag 1 and venue events`.

---

### Task 12: Remove old paths

**Files:**
- Delete: `src/recon_orders.rs`, `GatewayMsg::Exec`/`Reconcile`, `ExecEvent`, `VenueReport`, `ExecKind` (keep only what `ingest` needs internally), `FollowEvent`, `ExternalFill`, `FollowHandle`, `src/colo/booking.rs` fill path, `src/colo/follow.rs` gap-fill (`fill_gap`, `recon_*`).
- Modify: colo deployment registration (`src/colo/deployment.rs:318-477`) — on register, set the strategy's connection attribution rule (`account_id` = strategy account → strategy portfolio) instead of following; `src/colo/ingest.rs` `observe_external_fill` callers switch to `ingest`.

- [ ] **Step 1: Failing test** `colo_strategy_fill_lands_in_strategy_portfolio_via_rule` (register deployment, ingest a Binance fill on its account → order in strategy portfolio).
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Delete and rewire**; `cargo build` clean with no dead-code warnings for removed types.
- [ ] **Step 4: Run** full suite `cargo test --workspace && cargo test --workspace -- --ignored --skip real_docker` → PASS.
- [ ] **Step 5: Commit** `refactor: one ingest path; drop order recon and colo follow booking`.

---

### Task 13: Admin API

**Files:** Create `src/recon_admin_api.rs`; modify router in `src/handlers.rs`/`src/admin.rs` (admin-only, like `BrokerConnections`).

**Interfaces (JSON):**
- `PATCH /admin/broker-connections/{code}/recon` `{ default_portfolio_id?, external_policy?, eod_lookback?, intraday_poll_secs? }`
- `GET|POST /admin/broker-connections/{code}/attribution-rules`, `PUT|DELETE …/{id}` `{ account_id?, client_id_prefix?, portfolio_id, priority }`
- `POST /admin/broker-connections/{code}/sweep` `{ lookback? }` → `SweepReport`
- `GET /admin/recon-breaks?status=open`; `POST /admin/recon-breaks/{id}/adopt` `{ portfolio_id }` (replays `events_json` through `ingest` with that portfolio forced, then `resolved`); `POST …/{id}/dismiss`.
- Manifest exposure: existing connector/venue listing gains `multi_account`.

- [ ] **Step 1: Failing tests** (authz route tests pattern, `src/authz_route_tests.rs`): `recon_routes_are_admin_only`; `rule_without_account_or_prefix_is_400`; `adopting_a_break_books_its_fills_and_resolves_it`; `invalid_lookback_is_400`; `external_order_cancel_routes_by_venue_order_id` (cancel on an adopted order calls `cancel_order(external_order_id, symbol)`; amend endpoint refuses it with 409).
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(api): recon settings, attribution rules, breaks`.

---

### Task 14: Cockpit UI

**Files:** `cockpit/src/pages/admin/BrokerConnections.tsx` (recon section: default book, policy, EOD lookback select with the six values, poll secs, "Sweep now"; attribution rules table), `cockpit/src/pages/admin/Accounts.tsx` (accounts list only for `multi_account` connectors; hidden + auto-created otherwise), Create `cockpit/src/pages/admin/Breaks.tsx` (open breaks: venue order, kind, event count, Adopt-into-book select, Dismiss), nav entry with open-break count badge.

- [ ] **Step 1: Failing component tests** (existing cockpit test setup): `lookback_select_offers_six_values`; `accounts_hidden_for_single_account_connector`; `adopt_break_posts_portfolio`.
- [ ] **Step 2: Run** `cd cockpit && npm test` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `npm test && npm run build` → PASS.
- [ ] **Step 5: Commit** `feat(cockpit): recon settings, attribution rules, breaks page`.

---

### Task 15: Docs + graph

**Files:** `docs-site/src/content/docs/concepts/` new page `reconciliation.md` (layers, attribution, breaks, multi-account, IBKR master client id setup, Binance sub-account = own connection); `docs-site/.../changelog.md`.

- [ ] **Step 1: Write docs.**
- [ ] **Step 2: Run** `cd docs-site && npm run build` → PASS; `graft build`.
- [ ] **Step 3: Commit** `docs: external orders and reconciliation`.
