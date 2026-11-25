# IBKR Futures Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Trade CME equity index futures (ES, MES, NQ) through IBKR TWS with front-month resolution, roll warnings and a one-click calendar-spread roll.

**Architecture:** The IBKR TWS connector becomes generic over security type (STK/FUT/OPT/BAG). A `future_root` table carries the roll rule; contracts are ordinary `instrument` rows synced daily from IBKR. The hourly expiry sweep raises `ROLL_DUE` alerts through a new generic alert channel (table → Kafka `alerts` topic → `UiNotice::Alerts`). A roll is a parent order on a spread instrument, routed as one IBKR BAG order, with two child orders that receive the leg fills.

**Tech Stack:** Rust (axum, sqlx/Postgres, rdkafka, `ibapi =5.7.0`), React + TypeScript + shadcn/ui cockpit.

**Spec:** `docs/superpowers/specs/2026-11-25-ibkr-futures-design.md`

## Global Constraints

- Futures route through **IBKR TWS only** (`broker_code = 'IBKR_TWS'`); the FIX IBKR connector (`IBKR`) refuses them before sending.
- Roots seeded: `ES` (×50), `MES` (×5), `NQ` (×20), venue `XCME`, IBKR exchange `CME`, currency `USD`, asset class `INDEX`, `contract_months = 'HMUZ'`, `roll_offset_days = 8`, `roll_warn_days = 3`.
- Multiplier and tick always come from IBKR contract details, never from `future_root`.
- Contract `instrument.symbol` and `broker_instrument.broker_symbol` = IBKR `localSymbol` (`ESZ6`); `native_id` = conId. Never key a future by IBKR `symbol` (that is the root).
- Roll date = `expiry_date − roll_offset_days` calendar days, venue-local (America/Chicago).
- `expires_at` for `FUTURE` rows comes from IBKR last trade date + time + time zone; the calendar-derived recompute must not touch `FUTURE` rows.
- Natural `code` PK for catalog/config tables; UUID for order/event entities.
- The OMS never places an order nobody clicked: no auto-roll.
- `PERPETUAL` added to `instrument_class`; `SWAP` stays. Expiry/front-month/roll code selects `instrument_class = 'FUTURE' AND expiry_date IS NOT NULL`.
- Cockpit: shadcn/ui only.
- DB tests are `#[ignore]`; run with `cargo test -- --ignored --skip real_docker` (never without `--skip real_docker`).
- No compatibility shims (single user, pre-release).

## Review Focus

1. **Children vanish from recon.** Child orders are never at the broker on their own; the reconciler's "working order gone from venue" pass must skip `parent_order_id IS NOT NULL`, or it will resolve/cancel live roll legs. → test in Task 11.
2. **Parent fill gap.** IBKR reports cum qty on the BAG order, but the parent books no fills; `fill_gap` must not flag parents, or recon pulls forever. → test in Task 11.
3. **Two expiries, one root, one session.** Fills on ESZ6 and ESH7 the same day must land on different instruments (IBKR execution `symbol` is "ES" for both). → test in Task 3.
4. **Front month on the roll date boundary**, evaluated in Chicago time, not UTC (a UTC-evening check on roll day must already return the next contract). → test in Task 6.
5. **Short position roll** maps to buy-near/sell-far children and a correctly signed BAG; quantity edited below position rolls only that much. → test in Task 10.

---

### Task 1: Reference schema — XCME, future_root, PERPETUAL, future_spread

**Files:**
- Create: `db/migrations/ods/public/0028_FUTURES_REFERENCE.sql`
- Modify: `src/setup/database/seed.rs` (only if venues/calendars are seeded there rather than in migrations — follow `0025_INSERT_COINBASE_VENUE.sql`)
- Test: `src/setup/database/` schema test module (follow `src/ingest/schema_tests.rs` pattern), new `futures_schema_tests` in `src/futures/mod.rs` once Task 6 creates it — for now put the test in `src/expiry.rs` tests module

**Interfaces:**
- Produces: tables `future_root(code PK, venue, name, asset_class, currency, broker_symbol, broker_exchange, contract_months, roll_offset_days INT, roll_warn_days INT, status)`, `future_spread(instrument_id BIGINT PK → instrument, near_id BIGINT → instrument, far_id BIGINT → instrument, UNIQUE(near_id, far_id))`; venue `XCME` with calendar (timezone `America/Chicago`, close_time `16:00`); `instrument_class` CHECK includes `PERPETUAL`.

- [ ] **Step 1: Write the failing test** `futures_reference_rows_exist` (ignored DB test): asserts `SELECT roll_offset_days, roll_warn_days FROM future_root WHERE code='ES'` = `(8, 3)`; three roots with `broker_exchange='CME'`; `venue` `XCME` exists with calendar timezone `America/Chicago`; inserting an instrument with `instrument_class='PERPETUAL'` succeeds.
- [ ] **Step 2: Run** `cargo test futures_reference_rows_exist -- --ignored` → FAIL (relation does not exist).
- [ ] **Step 3: Write the migration.** CHECK swap like `0023_ALTER_INSTRUMENT_ADD_EXPIRED_STATUS.sql` (`instrument_instrument_class_check`). Seed ES/MES/NQ per Global Constraints.
- [ ] **Step 4: Run** the test → PASS.
- [ ] **Step 5: Commit** `feat(db): futures reference data (XCME, future_root, future_spread)`

### Task 2: Generic IBKR contracts in the TWS client

**Files:**
- Modify: `crates/gateway/src/connectors/ibkr_tws/client.rs` (`TwsContract` L68, `ContractQuery` L139, `stock_contract` L392, `to_tws_contract` L402, `place` L491, `contract_details` L555, `prev_close` L584, `FakeTws` L633)
- Modify: `crates/gateway/src/connectors/ibkr_tws/exchanges.rs` (add `("CME", "XCME")`)
- Modify: wherever quotes build a contract for subscription (`stream.rs` / `mod.rs` quote path) — use the generic builder

**Interfaces:**
- Produces:
  - `pub enum SecType { Stk, Fut, Opt, Bag }` with `as_ibkr() -> &'static str` (`"STK" | "FUT" | "OPT" | "BAG"`) and `parse(&str) -> Option<SecType>`.
  - `ContractQuery { symbol, exchange, currency, con_id, sec_type: SecType /* default Stk */, include_expired: bool }`.
  - `TwsContract` gains `sec_type: SecType, local_symbol: String, multiplier: f64 /* 1.0 for STK */, last_trade: Option<NaiveDate>, last_trade_at: Option<DateTime<Utc>>, strike: Option<f64>, right: Option<String>`.
  - `fn ib_contract(con_id: i64, sec_type: SecType, exchange: &str, primary: &str, currency: &str) -> Result<Contract, TwsError>` replacing `stock_contract`.
  - `TwsOrder` gains `sec_type: SecType` and `legs: Vec<TwsLeg>` where `pub struct TwsLeg { pub con_id: i64, pub ratio: i32, pub buy: bool }` (empty for single orders).
  - `FakeTws::add_contract(&self, c: TwsContract)` so tests can register FUT contracts.

- [ ] **Step 1: Failing tests** in `client.rs` tests:
  - `fut_query_asks_for_fut_on_cme`: `FakeTws` with ESZ6 (conId 1, `local_symbol "ESZ6"`, `multiplier 50.0`) and ESH7; `contract_details(&ContractQuery{ symbol: Some("ES"), exchange: Some("CME"), sec_type: SecType::Fut, ..})` returns both, each with `sec_type == SecType::Fut`.
  - `last_trade_converts_contract_zone_to_utc`: a helper `last_trade_instant("20261218", Some("08:30"), "US/Central")` returns `2026-12-18T14:30:00Z`.
  - `ib_contract_for_fut_uses_exchange_not_smart`: `ib_contract(1, SecType::Fut, "CME", "", "USD")` → `security_type` FUT, exchange `CME`.
- [ ] **Step 2: Run** `cargo test -p gateway ibkr_tws::client` → FAIL (compile errors).
- [ ] **Step 3: Implement.** `contract_details` maps ibapi `ContractDetails` (`contract.local_symbol`, `contract.multiplier`, `contract.last_trade_date_or_contract_month`, `last_trade_time`, `time_zone_id`) — verify field names against ibapi 5.7.0 docs. Add `pub(crate) fn last_trade_instant(date: &str, time: Option<&str>, tz: &str) -> Option<DateTime<Utc>>` (chrono-tz; map IBKR `US/Central` etc.). When `time` is absent, return `None` (sync then leaves `expires_at` NULL and logs).
- [ ] **Step 4: Run** tests → PASS; run `cargo test -p gateway` → all existing IBKR tests pass (STK default unchanged).
- [ ] **Step 5: Commit** `feat(ibkr_tws): generic security types in contract lookup and routing`

### Task 3: Fill and order mapping by local symbol

**Files:**
- Modify: `crates/gateway/src/connectors/ibkr_tws/client.rs` (`TwsExecution` L124, `to_tws_execution` L328)
- Modify: `crates/gateway/src/connectors/ibkr_tws/adapter.rs` (`fill_from_exec` L349, `order_update` L390, `con_id_for` L171, `contract` L147 symbol cache)

**Interfaces:**
- Consumes: `TwsContract.local_symbol`, `SecType` (Task 2).
- Produces: `TwsExecution` gains `local_symbol: String, con_id: i64, sec_type: SecType`. `VenueFill.symbol` for IBKR = `local_symbol` (equals `symbol` for STK). Executions with `sec_type == Bag` are dropped by `fills_from_executions` (debug log, no fill).

- [ ] **Step 1: Failing tests** in `adapter.rs` tests:
  - `two_expiries_of_one_root_map_to_two_symbols`: executions `symbol "ES"` with `local_symbol "ESZ6"` and `"ESH7"` → fills with `symbol` `"ESZ6"` and `"ESH7"`.
  - `stock_fill_symbol_is_unchanged`: STK exec `symbol "AAPL", local_symbol "AAPL"` → `"AAPL"`.
  - `bag_execution_is_not_a_fill`: exec with `sec_type Bag` → `fills_from_executions` returns no fill for it.
- [ ] **Step 2: Run** `cargo test -p gateway ibkr_tws::adapter` → FAIL.
- [ ] **Step 3: Implement**; the adapter's `symbols` cache keys by `local_symbol`.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `fix(ibkr_tws): key fills by localSymbol so futures expiries stay apart`

### Task 4: Broker instrument mapping by security type

**Files:**
- Modify: `crates/gateway/src/connectors/ibkr_tws/resolve.rs` (`to_broker_instrument` L119)

**Interfaces:**
- Consumes: `TwsContract` fields (Task 2).
- Produces: `pub fn to_broker_instrument(c: &TwsContract, root: Option<&RootSpec>) -> Result<BrokerInstrument, String>` with `pub struct RootSpec { pub code: String, pub asset_class: String }`.
  - STK: unchanged (EQUITY/SPOT, `contract_size 1.0`); `root` ignored.
  - FUT: requires `root`; `symbol = local_symbol`, `venue` from `to_mic(exchange)` (FUT has no primary exchange — use `exchange`), `asset_class = root.asset_class`, `instrument_class "FUTURE"`, `contract_size = multiplier`, `derivative = Some(DerivativeDef { underlying_symbol: root.code, expiry_date: last_trade, expires_at: last_trade_at, option_kind: None, strike: None })` — extend the gateway `DerivativeDef` with `expires_at: Option<DateTime<Utc>>` if missing; `broker_symbol = local_symbol`, `broker_exchange = exchange`.
  - OPT: EQUITY/OPTION, `contract_size = multiplier`, `underlying_symbol = symbol`, strike, `option_kind` from right (`C`→CALL, `P`→PUT), `symbol = local_symbol`.
  - Missing multiplier/last trade for FUT/OPT → `Err` naming the conId.
- Callers (`src/instruments_import.rs` `save` L212, `resolve_csv`) pass `None`.

- [ ] **Step 1: Failing tests:** `fut_maps_to_future_with_multiplier_and_expiry` (ESZ6 → `("ESZ6","XCME","INDEX","FUTURE")`, `contract_size 50.0`, derivative underlying `"ES"`, expiry `2026-12-18`); `opt_maps_to_option_with_strike_and_kind`; `fut_without_root_is_refused`.
- [ ] **Step 2: Run** `cargo test -p gateway ibkr_tws::resolve` → FAIL.
- [ ] **Step 3: Implement.** Ensure `save_instruments` (`src/instruments_import.rs:279`) writes `instrument_derivative.expires_at` when the definition carries it.
- [ ] **Step 4: Run** → PASS; `cargo build` clean.
- [ ] **Step 5: Commit** `feat(ibkr_tws): map FUT and OPT contracts to catalog instruments`

### Task 5: Futures contract sync + expiry recompute skips futures

**Files:**
- Create: `src/futures/mod.rs` (module root), `src/futures/sync.rs`
- Modify: `src/main.rs` (spawn daily sync loop, register module), `src/expiry.rs` (`recompute_expires_at` L62), admin routes where `instruments_import::routes()` is mounted (add `POST /admin/futures/sync`)

**Interfaces:**
- Consumes: `ContractQuery{sec_type: Fut}`, `to_broker_instrument(c, Some(&RootSpec))`, `save_instruments`.
- Produces:
  - `pub async fn sync_roots(pool: &PgPool, client: &dyn TwsClient) -> Result<SyncReport, String>`; `pub struct SyncReport { pub roots: usize, pub contracts: usize, pub skipped: Vec<String> }`.
  - `pub async fn run(pool: PgPool)` — loop every 24 h (first run 60 s after start), skips with an info log when no IBKR_TWS client is live.

- [ ] **Step 1: Failing tests:**
  - `sync_upserts_every_listed_contract_of_each_root` (ignored DB test, `FakeTws` with ESZ6/ESH7/MESZ6): after `sync_roots`, `instrument` has ESZ6 with `contract_size 50`, `instrument_class 'FUTURE'`, `instrument_derivative.underlying_symbol 'ES'`, `expires_at` = the fake's `last_trade_at`; `broker_instrument.native_id` = conId; report `contracts == 3`.
  - `sync_is_idempotent`: second run changes nothing (row counts equal).
  - `recompute_leaves_futures_expiry_alone`: set a FUTURE row's `expires_at` to a broker instant; `recompute_expires_at` returns 0 for it and the value is unchanged.
- [ ] **Step 2: Run** `cargo test futures::sync -- --ignored --skip real_docker` → FAIL.
- [ ] **Step 3: Implement.** Query per `future_root WHERE status='ACTIVE'`; drop contracts whose `last_trade_at` is past. Recompute SQL gains `AND i.instrument_class <> 'FUTURE'`.
- [ ] **Step 4: Run** → PASS; also `cargo test expiry -- --ignored --skip real_docker`.
- [ ] **Step 5: Commit** `feat(futures): daily IBKR contract sync for future roots`

### Task 6: Front-month resolution and `/futures/roots`

**Files:**
- Create: `src/futures/front.rs`, `src/futures/api.rs`
- Modify: `src/main.rs` (routes + OpenAPI registration, follow `instruments_api` pattern)

**Interfaces:**
- Produces:
  - `pub fn roll_date(expiry: NaiveDate, roll_offset_days: i32) -> NaiveDate`.
  - `pub fn venue_today(now: DateTime<Utc>) -> NaiveDate` (America/Chicago).
  - `pub async fn front_and_next(pool: &PgPool, root: &str, today: NaiveDate) -> Result<(Option<FutureContract>, Option<FutureContract>), sqlx::Error>`; `pub struct FutureContract { pub instrument_id: i64, pub symbol: String, pub expiry_date: NaiveDate, pub roll_date: NaiveDate, pub contract_size: f64 }`. Front = earliest `ACTIVE` FUTURE with `underlying_symbol = root` and `roll_date > today`.
  - `GET /futures/roots` → `[{ code, name, venue, front: FutureContract|null, next: FutureContract|null }]` (any authenticated user).

- [ ] **Step 1: Failing tests:**
  - `es_dec_2026_rolls_on_dec_10`: `roll_date(2026-12-18, 8) == 2026-12-10`.
  - `front_switches_on_roll_date_in_chicago` (ignored DB): ESZ6 (exp 2026-12-18) + ESH7 (exp 2027-03-19); `today=2026-12-09` → front ESZ6, next ESH7; `today=2026-12-10` → front ESH7.
  - `venue_today_is_chicago`: `venue_today(2026-12-10T03:00Z) == 2026-12-09`; `venue_today(2026-12-10T07:00Z) == 2026-12-10`.
  - `expired_contract_is_never_front`: status `EXPIRED` row is skipped.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(futures): front-month resolution and roots endpoint`

### Task 7: Connector instrument-class gate (FIX IBKR refuses futures)

**Files:**
- Modify: `crates/gateway/src/connectors/mod.rs` (`Manifest` L49), every connector's manifest literal (`alpaca`, `binance`, `bybit`, `coinbase`, `okx`, `ibkr`, `ibkr_tws`)
- Modify: `src/order_entry.rs` (`submit` L493, after the instrument row is loaded ~L624)

**Interfaces:**
- Produces: `Manifest.instrument_classes: &'static [&'static str]`. Values: `IBKR` `["SPOT"]`; `IBKR_TWS` `["SPOT","FUTURE","OPTION"]`; `ALPACA` `["SPOT","OPTION"]`; crypto connectors `["SPOT"]`. Submit rejects with message `"<CONNECTOR> cannot trade <class> instruments"` (HTTP 422) before risk.

- [ ] **Step 1: Failing test** `fix_ibkr_refuses_a_future` (ignored DB, in `order_entry` tests, pattern of `options_grant_blocks_option_order` L1379): FUTURE instrument listed on an `IBKR` connection → submit error message `"IBKR cannot trade FUTURE instruments"`; same instrument on `IBKR_TWS` passes the gate.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** → PASS; `cargo test -p gateway` green.
- [ ] **Step 5: Commit** `feat(order-entry): refuse instrument classes a connector cannot trade`

### Task 8: Valuation dispatch on instrument class

**Files:**
- Modify: `src/positions.rs` (`value` L104 and its callers — find with `graft callers value`)

**Interfaces:**
- Produces: `pub fn value(class: InstrumentClass, net_qty: f64, avg_cost: f64, contract_size: f64, mid: f64) -> Valuation`; `pub enum InstrumentClass { Spot, Future, Option, Perpetual, Other }` with `from_db(&str)`. All arms use the linear formula today; a comment marks where an inverse arm goes.

- [ ] **Step 1: Failing test** `future_values_with_multiplier`: `value(Future, 2.0, 6000.0, 50.0, 6010.0)` → `market_value 601000.0`, `unrealized_pnl 1000.0`.
- [ ] **Step 2: Run** `cargo test positions::tests::future_values` → FAIL.
- [ ] **Step 3: Implement**; update existing callers and tests to pass the class.
- [ ] **Step 4: Run** `cargo test positions` → PASS.
- [ ] **Step 5: Commit** `refactor(positions): dispatch valuation on instrument class`

### Task 9: Alert channel + ROLL_DUE in the expiry sweep

**Files:**
- Create: `db/migrations/ods/oms/0079_ALERTS.sql`, `src/alerts.rs`
- Modify: `src/kafka.rs` (`alerts_topic` config, `OutboxTopics.alerts`), `src/kafka_outbox.rs` (drain alerts like `fetch_position_batch` L84 / `ack_positions` L120; `pending_count` L137), `src/live_updates.rs` (`UiNotice::Alerts` L41 + `Conflator`), `src/expiry.rs` (`run_once` L114), `src/main.rs` (routes)

**Interfaces:**
- Consumes: `front_and_next`, `roll_date`, `venue_today` (Task 6).
- Produces:
  - Table `alert(id UUID PK, kind TEXT, portfolio_id UUID → portfolio, instrument_id BIGINT, dedupe_key TEXT UNIQUE, payload JSONB, created_at, acknowledged_at, published_at)`.
  - `pub async fn raise(pool: &PgPool, a: NewAlert) -> Result<Option<Uuid>, sqlx::Error>` (`INSERT … ON CONFLICT (dedupe_key) DO NOTHING`; `None` when deduped); `pub struct NewAlert { kind: &'static str, portfolio_id: Uuid, instrument_id: i64, dedupe_key: String, payload: Value }`.
  - `pub async fn roll_warnings(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error>` called from `run_once`: positions with `net_qty <> 0` in FUTURE contracts where `venue_today(now) >= roll_date − roll_warn_days`; dedupe `ROLL_DUE:<portfolio_id>:<instrument_id>`; payload `{root, contract, next_contract, next_instrument_id, net_qty, roll_date, expires_at}`.
  - `GET /alerts` (unacknowledged, filtered to portfolios the caller can view — reuse the positions access filter), `POST /alerts/{id}/ack` (204).
  - Kafka topic config key `KAFKA_ALERTS_TOPIC`, default `alerts`; record key = portfolio id.

- [ ] **Step 1: Failing tests** (ignored DB):
  - `roll_due_fires_once_per_position`: long 4 ESZ6 in portfolio P; `roll_warnings(2026-12-07T15:00Z)` → 1; again → 0; alert payload `next_contract == "ESH7"`.
  - `roll_due_waits_for_the_window`: `now = 2026-12-06T15:00Z` → 0.
  - `flat_position_gets_no_warning`.
  - `alerts_are_published_then_marked`: drain with a fake sink → one record on topic `alerts`, `published_at` set.
  - `UiNotice::Alerts` serializes as `{"type":"alerts"}` (unit).
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement.** Raising an alert also emits `UiNotice::Alerts` via the existing live-updates bus.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(alerts): generic alert channel and futures roll warnings`

### Task 10: Roll orders — parent + children, spread instrument

**Files:**
- Create: `db/migrations/ods/oms/0080_ORDER_PARENT.sql`, `src/rolls.rs`
- Modify: `src/order_entry.rs` (`insert_order_state` L132 accepts parent; `cancel` L946 cascades), `src/domain/orders/events.rs` (`parent_order_id` on the created event), `src/main.rs` (route)

**Interfaces:**
- Consumes: risk path inside `submit` (children are checked as normal orders), `front_and_next`.
- Produces:
  - `order_state.parent_order_id UUID NULL REFERENCES order_state(order_id)`, index on it.
  - `pub async fn ensure_spread(pool: &PgPool, near_id: i64, far_id: i64) -> Result<i64, sqlx::Error>` — creates/returns instrument `symbol "<NEAR>-<FAR>"`, venue `XCME`, class `FUTURE`, tick/precision of the near leg, `contract_size` of the near leg, no derivative row, no `broker_instrument` row, plus `future_spread` row.
  - `POST /orders/roll` body `RollRequest { portfolio_id: Uuid, from_instrument_id: i64, to_instrument_id: i64, qty: f64 /* >0 */, order_type: "market"|"limit", limit_price: Option<f64> /* spread price */, time_in_force: "day" }` → `{ parent_order_id, child_order_ids: [Uuid; 2] }`.
  - `pub async fn submit_roll(state: &AppState, auth: &AuthContext, req: RollRequest) -> Result<RollIds, ApiError>`: position sign decides sides — long: child sell `from`, child buy `to`; short: child buy `from`, child sell `to`. Both children go through the normal risk check; any rejection rejects all three. Parent + children inserted in one transaction; children get status `routed` when the parent is routed.
  - Cancel of a parent cancels the BAG at the broker; cancel of a child is refused (`"cancel the roll, not a leg"`).

- [ ] **Step 1: Failing tests** (ignored DB):
  - `long_roll_creates_sell_near_buy_far_children`: long 4 ESZ6 → children `(ESZ6, sell, 4)`, `(ESH7, buy, 4)`, both `parent_order_id = parent`.
  - `short_roll_reverses_the_children`: short 2 → `(ESZ6, buy, 2)`, `(ESH7, sell, 2)`.
  - `partial_qty_rolls_only_that_much`: long 4, qty 1 → children qty 1.
  - `qty_above_position_is_refused` and `roll_without_position_is_refused`.
  - `a_risk_reject_on_one_leg_rejects_the_roll`: notional limit below one leg → parent and both children `rejected`.
  - `spread_instrument_is_created_once`: two rolls → one `ESZ6-ESH7` instrument, one `future_spread` row.
  - `cancelling_a_child_is_refused`.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(orders): futures roll as parent order with two leg children`

### Task 11: BAG routing, leg-fill ingest, recon exemptions

**Files:**
- Modify: `crates/gateway/src/adapter.rs` (`BrokerOrderRequest` L12), `crates/gateway/src/connectors/ibkr_tws/adapter.rs` (`submit_order` L214, `cancel_order` L264), `crates/gateway/src/connectors/ibkr_tws/client.rs` (`place` builds BAG + `combo_legs`), `src/order_entry.rs` (`persist_routed` L260 fills `legs` for a parent; cascade routed/rejected/cancelled to children), `src/ingest/apply.rs` (lookup L120–L200, `fill_gap` L176), `src/ingest/decide.rs` (`Lookup` L40), `src/reconciler.rs` (`snapshot` working query L574)
- Test: `src/ingest/apply_tests.rs`, `src/reconciler_tests.rs`, `crates/gateway/src/connectors/ibkr_tws/adapter.rs` tests

**Interfaces:**
- Consumes: `TwsOrder.legs`/`TwsLeg`, `SecType::Bag` (Task 2), `parent_order_id` (Task 10), `future_spread` (Task 1).
- Produces:
  - `BrokerOrderRequest.legs: Vec<BrokerLeg>` (empty for single orders); `pub struct BrokerLeg { pub native_id: String, pub side: String /* "buy"|"sell" */, pub ratio: u32 }`. For a parent, `persist_routed` fills `legs` from the children's `broker_instrument.native_id` and sides; BAG `action` is always BUY with leg actions carrying direction (avoids the spread side convention; document in code), `qty` = parent qty, `limit_price` = spread price as entered.
  - IBKR adapter: non-empty `legs` → `TwsOrder{ sec_type: Bag, legs, exchange: "CME", order_ref: parent id }`; any other connector receiving `legs` → `BrokerError::BrokerRejected("multi-leg orders are not supported on <code>")`.
  - Ingest: a fill whose `client_order_id` resolves to a parent is redirected to the child whose instrument's `broker_symbol` equals `fill.symbol`; no match → recon break `UnknownInstrument`. Venue order updates for the parent apply to the parent only (status/cum in spread units); terminal parent status (`canceled`, `rejected`, `expired`) cascades to unfilled children. `fill_gap` returns `false` for parents.
  - Reconciler `snapshot`: working query adds `AND os.parent_order_id IS NULL`.

- [ ] **Step 1: Failing tests:**
  - adapter: `a_roll_goes_out_as_one_bag_with_two_legs` — `FakeTws` records one `TwsOrder` with `sec_type Bag`, legs `[(1, sell), (2, buy)]`, `order_ref` = parent id.
  - adapter: `legs_on_a_single_leg_connector_are_refused` (Alpaca adapter).
  - ingest: `leg_fills_land_on_their_children` — fills `ESZ6 sell 4 @ 6000`, `ESH7 buy 4 @ 6040`, both `client_order_id = parent` → child positions: ESZ6 net 0 (from long 4), ESH7 net 4; parent `cum_qty` untouched by fills.
  - ingest: `parent_status_update_is_not_a_fill_gap`.
  - ingest: `cancelled_parent_cancels_unfilled_children`.
  - reconciler: `children_are_not_resolved_as_missing` — working children absent from the venue snapshot remain `routed`.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p gateway` and `cargo test -- --ignored --skip real_docker ingest reconciler` → PASS.
- [ ] **Step 5: Commit** `feat(ibkr_tws): route rolls as BAG orders and book leg fills to children`

### Task 12: Cockpit — root picker, roll badge, alert banner, roll dialog

**Files:**
- Modify: `clients/web/src/api/types.ts` (`FutureRoot`, `FutureContract`, `Alert`, `UiNotice` `alerts`), `clients/web/src/api/hooks.ts` (`useFutureRoots`, `useAlerts`, `useAckAlert`, `useSubmitRoll`), `clients/web/src/api/liveUpdatesCore.ts` (`alerts` → invalidate alerts query), `clients/web/src/components/trading/InstrumentSearchModal.tsx` (roots first: "ES → ESZ6 (Dec 2026)", selecting picks the front contract's instrument id), `clients/web/src/pages/trading/Positions.tsx` (roll badge + row action)
- Create: `clients/web/src/components/trading/RollDialog.tsx`, `clients/web/src/components/trading/AlertBanner.tsx` (mounted in the trading layout next to `TradeLayout`, `clients/web/src/pages/trading/Trade.tsx:161`)
- Test: colocated `*.test.tsx` (follow the existing vitest setup)

**Interfaces:**
- Consumes: `GET /futures/roots`, `GET /alerts`, `POST /alerts/{id}/ack`, `POST /orders/roll` (Tasks 6, 9, 10).
- Produces: badge shown when `venueToday >= roll_date − roll_warn_days` for a futures position (computed from `/futures/roots` + position, not from alerts). Dialog copy for long 4 ESZ6: legs line `Sell 4 ESZ6 · Buy 4 ESH7`; fields qty (default full, max position), type (Limit default, Market), spread price (default: `mid(far) − mid(near)` from marks, editable); submit button `Roll`. Banner text: `ESZ6 rolls in {n} days — {qty} contracts {long|short} in {portfolio} · Roll · Dismiss` (Dismiss = ack).

- [ ] **Step 1: Failing tests:** `RollDialog` renders `Sell 4 ESZ6 · Buy 4 ESH7` for long 4 and `Buy 2 ESZ6 · Sell 2 ESH7` for short 2; qty input caps at position; `InstrumentSearchModal` lists `ES → ESZ6` first for query `es`; `AlertBanner` Dismiss calls ack.
- [ ] **Step 2: Run** `npm --prefix clients/web test` → FAIL.
- [ ] **Step 3: Implement** with shadcn components only (`Dialog`, `Badge`, `Alert`, `Button`, `Input`, `Select`).
- [ ] **Step 4: Run** tests + `npm --prefix clients/web run build` → PASS.
- [ ] **Step 5: Commit** `feat(cockpit): futures roots in search, roll badge, alerts and roll dialog`

### Task 13: Docs, graph refresh, paper verification

**Files:**
- Modify: user docs for IBKR TWS (futures section: roots, front month, roll, delayed data, FIX refuses futures), `CHANGELOG`/release notes per existing convention
- Run: `graft build`

- [ ] **Step 1:** Write the docs section; run `graft build`.
- [ ] **Step 2: Paper check (manual, IBKR paper, MES):** sync → MESZ6/MESH7 present with `contract_size 5`; buy 1 front via `MES` in search; position P&L moves ×5; temporarily set `roll_warn_days` high for MES → badge + banner + Kafka `alerts` record; Roll → one BAG at IBKR, children filled, positions MESZ6 0 / MESH7 1; restart the OMS mid-roll (working LMT far from market) → parent still working, children untouched, cancel works. Record outcomes (pass/fail per item) in the release notes.
- [ ] **Step 3: Commit** `docs: futures via IBKR TWS`
