# IBKR TWS API Connector Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A new `IBKR_TWS` broker connection that trades US and non-US stocks through an OMS-managed IB Gateway container, at parity with the Alpaca connector.

**Architecture:** A new connector in `crates/gateway/src/connectors/ibkr_tws/` behind the existing `Connector` trait. A supervisor owns one Docker container (IB Gateway + IBC) per connection and one `ibapi` client; the adapter, execution stream and delayed market data all share that client through a `TwsClient` trait seam so they are testable without IBKR. Instruments are resolved on demand (CSV / search) by conId and saved through the existing `load_universe` path. Risk gains per-currency notional handling and a per-connector price-band exemption.

**Tech Stack:** Rust (tokio, axum, sqlx), `ibapi` crate (pin exact version with `cargo add ibapi@5.7.0` at Task 5, currently latest), Docker CLI, IB Gateway + IBC image `ghcr.io/gnzsnz/ib-gateway` (pin by digest), React/Mantine cockpit.

**Spec:** `docs/superpowers/specs/2026-11-10-ibkr-tws-connector-design.md`

## Global Constraints

- Connector code `IBKR_TWS`, credential kind `IbkrTws`, display name "Interactive Brokers (TWS API)". The FIX connector `IBKR` is not changed.
- Instrument identity is the IBKR conId, stored as `native_id`. Orders always go out by conId.
- IBKR password never on a command line, in logs, in the repo or in plain-text config; env file mode 0600, deleted right after `docker run`.
- Gateway API port bound to `127.0.0.1` only.
- Market data type 3 (delayed) only.
- Amounts stay in the instrument's own currency; no FX conversion.
- Price band is skipped for `IBKR_TWS` by an explicit manifest flag; every other venue keeps "band without reference refuses".
- A cancel while the gateway is not connected fails with a transient `BrokerError`, never queued.
- `oms setup sync-broker IBKR_TWS --symbols` calls the running OMS admin API; it never starts Docker.
- Build env: `OPENSSL_DIR=/opt/homebrew/opt/openssl@4 OPENSSL_ROOT_DIR=/opt/homebrew/opt/openssl@4`. DB tests only against a disposable Postgres, never `oms-postgres`.
- Conventional commits. No push without user approval.

## Review Focus

1. **Ambiguous symbol in the CSV** (`SAP` with no exchange) → row is `Ambiguous` with every candidate listed, never silently saved as the first match. Test in Task 7.
2. **OMS restarts while the gateway container is running** → container adopted, not recreated (no second login, no IBKR "duplicate session" kick). Test in Task 4.
3. **Fill arrives for an order placed before an OMS restart** → mapped back through `orderRef` to the OMS order id even though the in-memory id map is empty. Test in Task 6.
4. **Wrong IBKR password** → state `LoginFailed`, supervisor stops retrying (IBKR locks accounts after repeated failures). Test in Task 4.
5. **Non-USD stock with only a USD notional default** → order passes on quantity limits; notional check off, not rejected. Test in Task 1.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/risk/src/evaluate.rs` | per-currency notional resolution, band exemption |
| `src/strategies/guard.rs`, `src/order_entry.rs` | pass the exemption from the connection's manifest |
| `src/positions.rs`, `cockpit/src/trade/pages/Positions.tsx` | currency per position, per-currency totals |
| `crates/gateway/src/connectors/mod.rs` | `Manifest.price_band_exempt`, register `IbkrTws` |
| `crates/gateway/src/connectors/ibkr_tws/mod.rs` | connector, manifest, venue, credentials |
| `.../ibkr_tws/exchanges.rs` | IBKR exchange code ↔ MIC table |
| `.../ibkr_tws/gateway.rs` | Docker container supervisor, states |
| `.../ibkr_tws/client.rs` | `TwsClient` trait + `ibapi` implementation + fake |
| `.../ibkr_tws/adapter.rs` | `BrokerAdapter`, order ids, error map, `DailyStats` |
| `.../ibkr_tws/stream.rs` | executions/commissions → engine, reconnect catch-up |
| `.../ibkr_tws/resolve.rs` | CSV parse, contract resolution, `BrokerInstrument` mapping |
| `src/instruments_import.rs` | admin endpoints resolve / save / search, nightly refresh |
| `src/setup/brokers.rs` | `--symbols` flag calling the admin API |
| `cockpit/src/components/IbkrImport.tsx`, `cockpit/src/pages/Instruments.tsx` | file picker, preview, search |
| `docs-site/src/content/docs/integrations/interactive-brokers.md` | TWS API docs |

---

### Task 1: Per-currency notional and price-band exemption in risk

**Files:**
- Modify: `crates/risk/src/evaluate.rs:85-134`, `crates/risk/src/types.rs` (Limits unchanged)
- Modify: `src/strategies/guard.rs:81-88,180-193`, `src/order_entry.rs:645-739`, `src/colo/deployment.rs:615`
- Modify: `crates/gateway/src/connectors/mod.rs:48-70` (add field), every existing `MANIFEST` (`price_band_exempt: false`)
- Test: `crates/risk/src/tests.rs`, `src/strategies/guard.rs` tests

**Interfaces:**
- Produces: `pub fn resolve_limits(defaults: &Limits, overrides: &Limits, quote_currency: &str, band_exempt: bool) -> Result<Limits, Reject>`; `Manifest.price_band_exempt: bool`; `guard::effective_limits(strategy, override_row, quote_currency, band_exempt: bool)`.

- [ ] **Step 1: Write failing tests** in `crates/risk/src/tests.rs`:
  - `inherited_notional_into_a_non_usd_quote_is_dropped_not_rejected`: defaults `max_order_notional=Some(1000.0)`, overrides empty, quote `"EUR"` → `Ok`, resolved `max_order_notional == None`, `max_position_notional == None`, `max_order_qty` still inherited.
  - `an_explicit_eur_override_applies`: override `max_order_notional=Some(500.0)`, quote `"EUR"` → resolved `Some(500.0)`.
  - `a_band_exempt_venue_resolves_no_band`: defaults `price_band_pct=Some(5.0)`, `band_exempt=true` → resolved `price_band_pct == None`.
  - Keep `a_price_band_without_a_reference_refuses` passing with `band_exempt=false`.
  - Replace `inherited_notional_needs_a_usd_like_quote` (its old behaviour is gone by spec decision).
- [ ] **Step 2: Run** `cargo test -p risk` → new tests FAIL (arity / behaviour).
- [ ] **Step 3: Implement.** In `resolve_limits`: when quote is not in `USD_LIKE`, an inherited (not overridden) notional field resolves to `None`; `band_exempt` forces `price_band_pct = None`. Thread `band_exempt` from `gateway::connectors::by_code(broker_code).map(|c| c.manifest().price_band_exempt).unwrap_or(false)` in `order_entry.rs` (broker code from the order's account connection) into `guard::check` → `effective_limits`. Colo deployment passes `false`.
- [ ] **Step 4: Run** `cargo test -p risk && cargo test -p gateway && cargo test --bin oms strategies::` → PASS.
- [ ] **Step 5: Commit** `feat(risk): per-currency notional and a per-venue price-band exemption`

### Task 2: Position currency and per-currency totals

**Files:**
- Modify: `src/positions.rs:111-125` (+ its query), `cockpit/src/trade/pages/Positions.tsx:10-131`
- Test: `src/positions.rs` tests, `cockpit/src/trade/pages/Positions.test.tsx` (create if absent)

**Interfaces:**
- Produces: `Position.currency: String` (instrument's currency); cockpit `totalsByCurrency(rows: Position[]): Map<string, {realized, unrealized, day}>`.

- [ ] **Step 1: Failing tests:** Rust — a position on an EUR instrument serializes `"currency":"EUR"`. TS — `totalsByCurrency` over one USD row (realized 10) and one EUR row (realized 5) returns two entries `USD→10`, `EUR→5`, never `15`.
- [ ] **Step 2: Run** `cargo test --bin oms positions::` and `cd cockpit && npm test -- Positions` → FAIL.
- [ ] **Step 3: Implement.** Join `instrument.currency` in the positions query. Summary cards render one line per currency, label suffixed with the code (e.g. "Realized P&L · EUR"); a single-currency book looks as today plus the code.
- [ ] **Step 4: Run** both test commands + `npm run build` → PASS.
- [ ] **Step 5: Commit** `feat(positions): totals per currency`

### Task 3: `IBKR_TWS` connector skeleton and exchange map

**Files:**
- Create: `crates/gateway/src/connectors/ibkr_tws/mod.rs`, `.../ibkr_tws/exchanges.rs`
- Modify: `crates/gateway/src/connectors/mod.rs` (`pub mod ibkr_tws;`, add `&ibkr_tws::IbkrTws` to both `all()` arms after `okx`), `crates/gateway/src/connectors/testdata/venues.json`

**Interfaces:**
- Produces:
  - `pub struct IbkrTwsCredentials { pub username: String, pub password: String }` (serde).
  - `static MANIFEST`: `code "IBKR_TWS"`, `credential_kind "IbkrTws"`, `redaction [("username", Clear), ("password", Hidden)]`, `live_only false`, `universe true`, `quotes QuoteScope::NonCrypto`, `exec_stream true`, `session_bound true`, `price_band_exempt true`.
  - Venue: name "Interactive Brokers (TWS API)", description "Stocks worldwide via IB Gateway (Docker)", `universe: Some("Imported on demand from a CSV or search")`, environments "paper trading account" with IBKR portal URLs, fields `username` "IBKR username", `password` secret.
  - `exchanges::to_mic(ibkr: &str) -> Option<&'static str>`, `exchanges::from_mic(mic: &str) -> Option<&'static str>`; table covers at least NASDAQ→XNAS, NYSE→XNYS, ARCA→ARCX, AMEX→XASE, BATS→BATS, IBIS→XETR, FWB→XFRA, LSE→XLON, SBF→XPAR, AEB→XAMS, EBS→XSWX, BVME→XMIL, BM→XMAD, SFB→XSTO, TSE→XTKS, SEHK→XHKG, ASX→XASX, TSE (Canada: key `TSX`)→XTSE.
  - `connect` returns `Err("not yet wired")` until Task 6; `verify` → `TestOutcome::NotTestable("checked when the gateway logs in")`.

- [ ] **Step 1: Failing tests** in `exchanges.rs`: `maps_ibis_to_xetr`, `round_trips_every_entry` (for each pair `from_mic(to_mic(x)) == x`), `unknown_exchange_is_none` (`to_mic("FOO") == None`). In `connectors/mod.rs` existing `venues_json_is_unchanged` will fail until golden updated.
- [ ] **Step 2: Run** `cargo test -p gateway connectors::` → FAIL.
- [ ] **Step 3: Implement**, then regenerate `testdata/venues.json` by appending the new venue object (only addition; existing bytes unchanged — diff must show one added object).
- [ ] **Step 4: Run** `cargo test -p gateway` → PASS.
- [ ] **Step 5: Commit** `feat(ibkr): TWS API connector skeleton and exchange map`

### Task 4: Gateway container supervisor

**Files:**
- Create: `crates/gateway/src/connectors/ibkr_tws/gateway.rs`

**Interfaces:**
- Produces:
  - `pub trait GatewayDocker: Send + Sync { fn inspect(&self, name: &str) -> Option<ContainerInfo>; fn run(&self, spec: &RunSpec) -> Result<(), String>; fn stop_rm(&self, name: &str) -> Result<(), String>; fn logs_tail(&self, name: &str, lines: usize) -> String; fn available(&self) -> bool; }`
  - `pub struct ContainerInfo { pub running: bool, pub api_port: Option<u16> }`
  - `pub struct RunSpec { pub name: String, pub image: &'static str, pub api_port: u16, pub env_file: PathBuf }`
  - `pub enum GatewayState { Starting, Connected, AwaitingTwoFactor, LoginFailed(String), Disconnected }`
  - `pub fn container_name(connection_code: &str) -> String` → `oms-ibgw-<lowercased code>`.
  - `pub fn classify_logs(tail: &str) -> Option<GatewayState>` — IBC lines: "Second Factor Authentication" → `AwaitingTwoFactor`; "Login failed" / "invalid username or password" → `LoginFailed`.
  - `pub async fn ensure_running(docker: &dyn GatewayDocker, code: &str, env: &str, creds: &IbkrTwsCredentials, state_dir: &Path) -> Result<u16, GatewayState>` — adopt if a running container of that name exists (return its port); else write env file (`TWS_USERID`, `TWS_PASSWORD`, `TRADING_MODE=paper|live`, `READ_ONLY_API=no`) mode 0600, `docker run -d --name … -p 127.0.0.1:<port>:<4002 paper|4001 live> --env-file …`, delete env file, return port. Port = first free in 4100..4199.
  - `pub struct DockerCli;` implementing the trait with `std::process::Command`.

- [ ] **Step 1: Failing tests** with a `FakeDocker` (records `run` calls, scripted `inspect`/logs):
  - `a_running_container_is_adopted_not_recreated` → `run` never called, returns its port.
  - `a_missing_container_is_started_on_localhost` → one `run`, port in 4100..4199.
  - `the_env_file_is_gone_after_start_and_was_0600` (fake records mode when `run` is called).
  - `the_password_is_not_in_the_run_spec_debug` (`format!("{:?}", spec)` lacks password).
  - `a_failed_login_is_classified_and_not_retried`: `classify_logs("... Login failed ...") == Some(LoginFailed(_))`.
  - `two_factor_prompt_is_classified`.
  - `no_docker_is_a_clear_error` → `available()==false` gives `LoginFailed("Docker is not available …")`-style message; choose `Err(GatewayState::LoginFailed("Docker is required for IBKR_TWS but is not running".into()))`.
- [ ] **Step 2: Run** `cargo test -p gateway ibkr_tws::gateway` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(ibkr): run and adopt the IB Gateway container`

### Task 5: `TwsClient` seam over `ibapi`

**Files:**
- Create: `crates/gateway/src/connectors/ibkr_tws/client.rs`
- Modify: `crates/gateway/Cargo.toml` (`ibapi = "=5.7.0"`)

**Interfaces:**
- Produces:
  - `pub struct TwsContract { pub con_id: i64, pub symbol: String, pub exchange: String, pub primary_exchange: String, pub currency: String, pub min_tick: f64, pub lot_size: Option<f64>, pub name: String, pub isin: Option<String> }`
  - `pub struct TwsOrder { pub order_id: i32, pub con_id: i64, pub exchange: String, pub primary_exchange: String, pub currency: String, pub action: Side, pub qty: f64, pub order_type: OrderKind /*Mkt|Lmt(f64)*/, pub tif: Tif /*Day|Gtc*/, pub order_ref: String, pub account: String }`
  - `pub enum TwsEvent { Status { order_id: i32, order_ref: String, status: String, filled: f64, remaining: f64 }, Execution { exec_id: String, order_id: i32, order_ref: String, qty: f64, price: f64, time: DateTime<Utc> }, Commission { exec_id: String, amount: f64, currency: String }, Error { order_id: Option<i32>, code: i32, message: String }, Disconnected }`
  - `#[async_trait] pub trait TwsClient: Send + Sync { async fn next_order_id(&self) -> Result<i32, TwsError>; async fn place(&self, o: &TwsOrder) -> Result<(), TwsError>; async fn cancel(&self, order_id: i32) -> Result<(), TwsError>; async fn open_orders(&self) -> Result<Vec<(i32, String /*order_ref*/, TwsOrderState)>, TwsError>; async fn completed_orders(&self) -> Result<Vec<(i32, String, TwsOrderState)>, TwsError>; async fn executions_today(&self) -> Result<Vec<TwsEvent>, TwsError>; async fn contract_details(&self, q: &ContractQuery) -> Result<Vec<TwsContract>, TwsError>; async fn matching_symbols(&self, pattern: &str) -> Result<Vec<TwsContract>, TwsError>; async fn prev_close(&self, con_id: i64) -> Result<Option<f64>, TwsError>; async fn managed_accounts(&self) -> Result<Vec<String>, TwsError>; fn events(&self) -> tokio::sync::broadcast::Receiver<TwsEvent>; fn connected(&self) -> bool; }`
  - `pub struct ContractQuery { pub symbol: Option<String>, pub exchange: Option<String>, pub currency: Option<String>, pub con_id: Option<i64> }` (sec type always STK, exchange default SMART).
  - `pub struct IbapiClient` (connect `127.0.0.1:<port>`, client id `1`, `reqMarketDataType(3)` on connect; `prev_close` = delayed snapshot close, else one `1 D` daily bar close, else `None`).
  - `#[cfg(test)] pub struct FakeTws` scripted for later tasks.
- [ ] **Step 1: Failing test** `fake_tws_hands_out_increasing_ids` (FakeTws compiles against the trait; ids 100,101,…).
- [ ] **Step 2: Run** → FAIL. **Step 3:** implement trait, `IbapiClient`, `FakeTws`. **Step 4:** `cargo build -p gateway && cargo test -p gateway ibkr_tws::client` → PASS.
- [ ] **Step 5: Commit** `feat(ibkr): a TWS client seam over ibapi`

### Task 6: Adapter, connect wiring, execution stream

**Files:**
- Create: `.../ibkr_tws/adapter.rs`, `.../ibkr_tws/stream.rs`
- Modify: `.../ibkr_tws/mod.rs` (`connect`, `verify`)

**Interfaces:**
- Consumes: Task 4 `ensure_running`, `GatewayState`; Task 5 `TwsClient`.
- Produces:
  - `pub struct IbkrTwsAdapter { client: Arc<dyn TwsClient>, ... }` implementing `BrokerAdapter` and `DailyStats`.
  - `submit_order`: requires `req.native_id` (conId) else `BrokerError` reject "instrument has no IBKR conId; import it first"; looks up stored `broker_exchange` via `req.symbol`… use contract details cache keyed by conId for exchange/currency; `order_ref = req.order_id`; returns `external_order_id = order_id.to_string()`.
  - `cancel_order`: when `!client.connected()` → `BrokerError::Transient("IBKR gateway reconnecting; retry")` (use the existing transient variant name in `adapter.rs`).
  - `open_orders` / `order_status`: map by `order_ref` first, IBKR id second.
  - `pub fn map_error(code: i32, msg: &str) -> ErrorClass` with `enum ErrorClass { Reject, Transient, NoSuchOrder, Info }`: 201→Reject, 202→Info (cancelled), 135/10147/10148→NoSuchOrder, 1100/1101/1102/2110/504→Transient, 2104/2106/2158→Info; unknown <2000 → Reject, ≥2000 → Info.
  - `stream::run(client, health: StreamHandle, engine: EngineHandle)`: forwards `Execution` as fills keyed by `exec_id`; `Commission` enriches the fill (same path Binance fees use); on `Disconnected` → `health.set_connecting()`, on reconnect calls `executions_today` + `completed_orders` and replays them (idempotent by exec id), then `set_live`.
  - `connect`: spawns supervisor task: `ensure_running` → `IbapiClient::connect` with backoff 1s→60s; publishes `GatewayState` into the stream health (`AwaitingTwoFactor`/`LoginFailed` as error text); `LoginFailed` stops the loop.
  - `verify`: `NotTestable` before save (login needs the container). The spec's "Verify connection checks the account" is met after connect: the supervisor calls `managed_accounts()` and only then `set_live`; an empty list is an error state "IBKR returned no account".
- [ ] **Step 1: Failing tests** (FakeTws):
  - `an_order_goes_out_by_con_id_with_the_oms_id_as_order_ref`.
  - `an_order_without_a_con_id_is_rejected_before_sending`.
  - `a_cancel_while_disconnected_is_transient`.
  - `a_fill_for_an_order_from_before_a_restart_maps_by_order_ref` (fresh adapter, empty map, Execution with `order_ref="<uuid>"` → engine fill for that uuid).
  - `a_replayed_execution_is_not_booked_twice`.
  - `error_codes_map` table test over the codes above.
- [ ] **Step 2: Run** `cargo test -p gateway ibkr_tws` → FAIL. **Step 3:** implement. **Step 4:** PASS + `cargo test --workspace`.
- [ ] **Step 5: Commit** `feat(ibkr): orders and fills over the TWS API`

### Task 7: Contract resolution and CSV import (backend + CLI)

**Files:**
- Create: `.../ibkr_tws/resolve.rs`, `src/instruments_import.rs`
- Modify: `src/main.rs` (routes), `src/setup/brokers.rs:132-193` (`--symbols <path>`), `.../ibkr_tws/mod.rs` (`universe_provider` → `Err("IBKR instruments are imported from a CSV or search")`)

**Interfaces:**
- Produces:
  - `pub struct CsvRow { pub line: usize, pub symbol: Option<String>, pub exchange: Option<String>, pub currency: Option<String>, pub con_id: Option<i64> }`; `pub fn parse_csv(text: &str) -> Result<Vec<CsvRow>, String>` (header required; columns any order; `conid` or `symbol` required per row; exchange accepted as IBKR code or MIC, normalised via `exchanges::from_mic`).
  - `pub enum Resolution { Found(TwsContract), Ambiguous(Vec<TwsContract>), NotFound(String) }`; `pub async fn resolve(client: &dyn TwsClient, row: &CsvRow) -> Resolution`.
  - `pub fn to_broker_instrument(c: &TwsContract) -> Result<BrokerInstrument, String>`: `native_id=con_id`, `symbol`, `venue="IBKR_TWS"`, `currency`, `asset_class="EQUITY"`, `instrument_class="STOCK"` (match Alpaca's values), `price_increment=min_tick`, `lot_size`, `size_increment=lot_size.unwrap_or(1.0)`, `provider_exchange=to_mic(primary_exchange)` (unknown → `Err`), `broker_symbol=symbol`, `broker_exchange=Some(primary_exchange)`, identifiers.isin.
  - Admin endpoints (admin auth):
    - `POST /admin/brokers/IBKR_TWS/instruments/resolve` body `{csv: String}` → `[{line, status: "found"|"ambiguous"|"not_found", candidates: [Contract], message}]`.
    - `POST /admin/brokers/IBKR_TWS/instruments` body `{con_ids: [i64]}` → re-resolves each conId and saves via `load_universe` with an in-memory `InstrumentProvider` returning those `BrokerInstrument`s → `UniverseLoadReport`.
    - `GET /admin/brokers/IBKR_TWS/instruments/search?q=` → candidates.
    - All return 503 `"IBKR gateway is not connected"` when the client is down.
  - CLI `oms setup sync-broker IBKR_TWS --symbols file.csv`: calls resolve, saves unique `found`, prints ambiguous (with candidates) and not-found by line, exit code 1 if any; uses the configured admin URL/password like other admin CLI calls.
- [ ] **Step 1: Failing tests:**
  - `parse_csv`: header any order; a row with neither symbol nor conid → error naming line; `XETR` normalised to `IBIS`.
  - `ambiguous_symbol_lists_every_candidate_and_saves_nothing` (FakeTws returns SAP/NYSE and SAP/IBIS for row `SAP,,`).
  - `exchange_narrows_to_one` (`SAP,XETR,EUR` → `Found` IBIS).
  - `unknown_primary_exchange_is_reported_not_guessed`.
  - `saving_twice_is_idempotent` (DB test, `#[ignore]` like the other DB suites, via `COLO_TEST_DATABASE_URL`-style disposable DB).
- [ ] **Step 2: Run** → FAIL. **Step 3:** implement. **Step 4:** `cargo test -p gateway ibkr_tws::resolve && cargo test --bin oms instruments_import` (+ `--ignored` on disposable DB) → PASS.
- [ ] **Step 5: Commit** `feat(ibkr): import instruments from a CSV, resolved by conId`

### Task 8: Nightly refresh

**Files:** Modify `src/instruments_import.rs`, `src/main.rs` (spawn at serve).

**Interfaces:** `pub async fn refresh_ibkr_instruments(pool: &PgPool, client: &dyn TwsClient) -> RefreshReport { updated, untradeable }`; scheduled daily 06:00 UTC while the connection exists; conId with no contract details → `instrument_xref.is_tradeable = false` (never deleted).

- [ ] **Step 1: Failing tests** (DB, ignored): `a_vanished_con_id_becomes_untradeable_not_deleted`; `a_changed_min_tick_is_updated`.
- [ ] **Step 2–4:** run FAIL → implement → PASS.
- [ ] **Step 5: Commit** `feat(ibkr): refresh imported instruments nightly`

### Task 9: Cockpit import and search

**Files:**
- Create: `cockpit/src/components/IbkrImport.tsx`, `cockpit/src/api/ibkrImport.ts` (+ `.test.ts`)
- Modify: `cockpit/src/pages/Instruments.tsx` (button "Import from IBKR" shown when an `IBKR_TWS` connection exists)

**Interfaces:** `resolveCsv(csv: string): Promise<ResolveRow[]>`, `saveConIds(ids: number[]): Promise<LoadReport>`, `searchContracts(q: string): Promise<Contract[]>`; pure helper `selectedConIds(rows: ResolveRow[], picks: Record<number, number>): number[]` (found rows + picked ambiguous rows).

- [ ] **Step 1: Failing test** `selectedConIds` — found rows included, ambiguous only when picked, not_found never.
- [ ] **Step 2: Run** `cd cockpit && npm test -- ibkrImport` → FAIL.
- [ ] **Step 3: Implement:** modal with `FileInput accept=".csv"` → preview table (line, symbol, status badge, name · exchange MIC · currency · tick, a `Select` of candidates for ambiguous) → "Save N instruments" → report. Search tab: text box → results → "Add". 503 shows "Start the IBKR connection first".
- [ ] **Step 4: Run** `npm test && npm run build && npm run lint` → PASS.
- [ ] **Step 5: Commit** `feat(cockpit): import IBKR instruments from a CSV or search`

### Task 10: Docs and paper-account check

**Files:** Modify `docs-site/src/content/docs/integrations/interactive-brokers.md`; update `HANDOFF.md` (untracked) with results.

- [ ] **Step 1:** Write the TWS section: Docker required; create the connection (paper first); states incl. "awaiting 2FA" for live and weekly re-approval; delayed prices; CSV format with the example `symbol,exchange,currency` / `AAPL,,USD` / `SAP,XETR,EUR` / `VOD,XLON,GBP`; per-currency P&L and limits; price band not applied; known gap: fills older than the current day after a long outage. FIX section kept, marked experimental. `cd docs-site && npm run build` → PASS.
- [ ] **Step 2: Live paper check (needs the user's IBKR paper login, entered by the user in the cockpit):** import the example CSV; limit buy far from market then cancel; market buy 1 AAPL fills and shows a position; `docker restart oms-ibgw-…` → OMS reconnects, open limit order found by recon; restart the OMS → container adopted (no new container id). Record outcomes.
- [ ] **Step 3: Commit** `docs(ibkr): the TWS API connection`
