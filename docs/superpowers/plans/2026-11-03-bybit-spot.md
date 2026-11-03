# Bybit Spot Broker Connection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Trade Bybit spot from openOMS (instruments, orders, fills, reconciliation, day P&L) the way Binance spot works today, proven by a testnet market order filled from the trader UI.

**Architecture:** A REST adapter `src/adapters/bybit.rs` (v5 API, HMAC-SHA256) implementing `BrokerAdapter` and `InstrumentProvider`; a private WebSocket task `src/bybit_stream.rs` feeding `process_execution_report`; a `BrokerCredentials::Bybit` variant; `Broker::Bybit` for catalog sync; registration in `reload.rs` that, unlike Binance, restarts the stream on Reload (Alpaca-style). The venue entry in `src/venues.rs` drives the cockpit.

**Tech Stack:** Rust (axum, reqwest, tokio-tungstenite, hmac 0.12 + sha2 0.10 already in Cargo.toml, sqlx), React/Mantine cockpit, Starlight docs.

**Spec:** `docs/superpowers/specs/2026-11-03-bybit-spot-design.md`

## Global Constraints

- Spot only. No perpetuals, futures, options, margin, FIX, amend, or Bybit "demo trading". PAPER = `https://api-testnet.bybit.com` + `wss://stream-testnet.bybit.com/v5/private`; LIVE = `https://api.bybit.com` + `wss://stream.bybit.com/v5/private`.
- Signing: headers `X-BAPI-API-KEY`, `X-BAPI-TIMESTAMP` (ms), `X-BAPI-SIGN`, `X-BAPI-RECV-WINDOW` = `5000`; sign `timestamp + api_key + recv_window + (query string | JSON body)` with HMAC-SHA256 over the secret, lowercase hex.
- Every v5 response is `{"retCode":0,"retMsg":"OK","result":{…}}`, often with HTTP 200 on failure: `retCode != 0` is an error carrying `retMsg`.
- Instruments are venue `BYBIT` (own rows, not Binance's). `orderLinkId` = the OMS order UUID unchanged. Market orders send `marketUnit: "baseCoin"`.
- Credential field names `api_key`, `api_secret`; drawer labels "API key", "API secret"; both secret; no section.
- Connection codes `bybit-paper` / `bybit-live`; `Broker::Bybit.environment()` reads `BYBIT_ENV` (default `PAPER`).
- Reload applies a Bybit credential without restart and (re)starts its stream.
- No help text in the UI; explanations go in `docs-site/src/content/docs/integrations/bybit.md`.
- Network-free unit tests (recorded JSON, known-answer signature). DB tests stay `#[ignore]`. Never create databases on the shared dev Postgres; never handle the user's Bybit key.
- Commit only with the user's go-ahead; never commit `.gitignore`, `.ignore`, or `docs/superpowers/plans/2026-09-28-options-search-chain-strategy.md`.

## Review Focus

1. HTTP 200 with `retCode != 0` (insufficient balance, bad precision, unknown symbol) must become `BrokerError::BrokerRejected(retMsg)`, never a "routed" order with an empty `orderId`. Pinned in Task 1 (`envelope_*` tests) and Task 2 (`submit_rejection_*`).
2. Quantities like `0.1 + 0.2` must reach Bybit as `"0.3"`, not `"0.30000000000000004"`, which Bybit rejects on precision. Pinned in Task 2 (`decimal_*`).
3. Double-counted fills. The execution path does **not** dedupe by `execution_id` today: it only skips events for orders already terminal (`execution.rs:119`), a gap Binance shares and that is out of scope here. So Bybit must take fills from exactly one source: `execution.spot` `Trade` entries. `order.spot` updates (`PartiallyFilled`, `Filled`) must never produce a Fill too. Pinned in Task 4 (`order_updates_never_produce_fills`).
4. `PartiallyFilledCanceled` must end the order as canceled while its earlier fills stand; `Deactivated` (an untriggered order cancelled by the venue) counts as canceled, not rejected. Pinned in Task 4 (`status_*`) and Task 2 (`order_status_*`).
5. Reload toggling Bybit off (Disabled, cleared credential, deleted row) must stop the private stream, and toggling on must not leave two streams running. Pinned in Task 5 (`reload_*`).

---

### Task 1: Bybit adapter core — signing, envelope, instrument catalog

**Files:**
- Create: `src/adapters/bybit.rs`
- Modify: `src/adapters/mod.rs:1-2` (add `pub mod bybit;`)
- Test: inline `#[cfg(test)] mod tests` in `src/adapters/bybit.rs`; recorded samples under `src/adapters/testdata/bybit/` (`instruments_spot.json`).

**Interfaces:**
- Produces:
  - `pub struct BybitAdapter` (Clone), `pub fn new(api_key: String, api_secret: String, environment: &str) -> Self` (LIVE vs anything else → PAPER hosts).
  - `pub fn api_key(&self) -> &str`, `pub fn private_ws_url(&self) -> &'static str`.
  - `pub fn sign(&self, payload: &str) -> String` (HMAC-SHA256 hex over `payload` with the secret; the WS auth reuses it).
  - `fn rest_signature(&self, ts: i64, payload: &str) -> String` = `sign(&format!("{ts}{api_key}5000{payload}"))`.
  - `async fn get(&self, path: &str, query: &str, signed: bool) -> Result<serde_json::Value, BrokerError>` and `async fn post(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value, BrokerError>`; both return the envelope's `result`.
  - `pub(crate) fn envelope(body: serde_json::Value) -> Result<serde_json::Value, BrokerError>`.
  - `pub(crate) fn parse_spot_instruments(result: &serde_json::Value) -> Vec<BrokerInstrument>`; `pub async fn list_spot_instruments(&self)`; `impl InstrumentProvider for BybitAdapter`.
  - `pub async fn api_key_info(&self) -> Result<serde_json::Value, BrokerError>` (GET `/v5/user/query-api`, signed).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn signature_matches_a_known_answer() {
    // HMAC-SHA256(key="secret", "1700000000000key5000category=spot") in lowercase hex,
    // computed once with `openssl dgst -sha256 -hmac secret` and pasted here.
    let a = BybitAdapter::new("key".into(), "secret".into(), "PAPER");
    assert_eq!(a.rest_signature(1700000000000, "category=spot"), "<hex from openssl>");
}

#[test]
fn envelope_ok_returns_result() {
    let r = envelope(serde_json::json!({"retCode":0,"retMsg":"OK","result":{"x":1}})).unwrap();
    assert_eq!(r["x"], 1);
}

#[test]
fn envelope_nonzero_retcode_is_a_rejection_with_the_message() {
    let e = envelope(serde_json::json!({"retCode":170131,"retMsg":"Insufficient balance.","result":{}})).unwrap_err();
    assert!(matches!(e, BrokerError::BrokerRejected(ref m) if m.contains("Insufficient balance")));
}

#[test]
fn spot_instruments_are_bybit_venue_rows_with_lot_and_tick_sizes() {
    let sample: serde_json::Value = serde_json::from_str(include_str!("testdata/bybit/instruments_spot.json")).unwrap();
    let list = parse_spot_instruments(&sample["result"]);
    let btc = list.iter().find(|i| i.broker_symbol == "BTCUSDT").unwrap();
    assert_eq!(btc.definition.venue, "BYBIT");
    assert_eq!(btc.definition.currency, "USDT");
    assert_eq!(btc.definition.native_id.as_deref(), Some("BTC"));
    assert_eq!(btc.definition.instrument_class, "SPOT");
    assert_eq!(btc.definition.price_increment, 0.01);   // priceFilter.tickSize in the sample
    assert_eq!(btc.definition.size_increment, 0.000001); // lotSizeFilter.basePrecision
    assert_eq!(btc.min_quantity, Some(0.000048));        // lotSizeFilter.minOrderQty
    assert_eq!(btc.min_notional, Some(1.0));             // lotSizeFilter.minOrderAmt
    assert!(btc.is_tradeable);
    // A pair whose status is not "Trading" is listed but not tradeable.
    assert!(!list.iter().find(|i| i.broker_symbol == "HALTUSDT").unwrap().is_tradeable);
}
```

The sample is a trimmed copy of a real `GET https://api-testnet.bybit.com/v5/market/instruments-info?category=spot` response (public, no key) holding BTCUSDT plus one entry edited to `"status":"PreLaunch"`, `"symbol":"HALTUSDT"`. Set the asserted numbers from the BTCUSDT entry actually saved.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --bin oms adapters::bybit`
Expected: compile failure, "cannot find type `BybitAdapter`".

- [ ] **Step 3: Implement the interfaces above** in `src/adapters/bybit.rs`, mirroring `binance.rs`'s layout and module doc (auth, symbol mapping). `definition.provider_exchange` and `broker_exchange` are `Some("BYBIT")`; `price_precision` from the tick size's decimals; `contract_size: 1.0`; `max_quantity` from `maxOrderQty`; `max_notional` from `maxOrderAmt`. Signed GETs sign the exact query string sent.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin oms adapters::bybit`
Expected: 4 passed.

- [ ] **Step 5: Commit** — `feat(bybit): v5 REST adapter core and spot catalog`

---

### Task 2: Bybit order entry, reconciliation reads, daily stats

**Files:**
- Modify: `src/adapters/bybit.rs`
- Test: same module; samples `open_orders.json`, `order_history.json`, `tickers_spot.json` under `src/adapters/testdata/bybit/` (shapes from Bybit's v5 docs; values invented).

**Interfaces:**
- Consumes: Task 1's `post`, `get`, `envelope`.
- Produces:
  - `pub(crate) fn decimal(x: f64) -> String`: at most 10 fractional digits, trailing zeros and a trailing `.` trimmed.
  - `pub(crate) fn order_body(req: &BrokerOrderRequest) -> Result<serde_json::Value, BrokerError>`.
  - `pub(crate) fn order_outcome(status: &str) -> BrokerOrderOutcome`.
  - `pub(crate) fn parse_open_orders(result: &Value) -> Vec<BrokerOpenOrder>`; `pub(crate) fn parse_order_state(order: &Value) -> BrokerOrderState`; `pub(crate) fn parse_prev_prices(result: &Value, wanted: &[String]) -> Vec<(String, f64)>`.
  - `impl BrokerAdapter for BybitAdapter`: `submit_order` (POST `/v5/order/create`, returns `result.orderId`), `cancel_order` (POST `/v5/order/cancel` with `category`, `symbol`, `orderId`), `open_orders` (GET `/v5/order/realtime?category=spot&openOnly=0`), `order_status` (GET `/v5/order/realtime?category=spot&orderId=…`, falling back to `/v5/order/history` when the list is empty), `daily_stats` (public GET `/v5/market/tickers?category=spot`, filtered to `symbols`).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn decimal_drops_float_noise_and_trailing_zeros() {
    assert_eq!(decimal(0.1 + 0.2), "0.3");
    assert_eq!(decimal(0.00001), "0.00001");
    assert_eq!(decimal(2.0), "2");
}

#[test]
fn market_buy_is_sized_in_the_base_coin() {
    let b = order_body(&req("market", "buy", 0.001, None, "day")).unwrap();
    assert_eq!(b["category"], "spot");
    assert_eq!(b["side"], "Buy");
    assert_eq!(b["orderType"], "Market");
    assert_eq!(b["qty"], "0.001");
    assert_eq!(b["marketUnit"], "baseCoin");
    assert_eq!(b["orderLinkId"], ORDER_ID); // the 36-char UUID, unchanged
}

#[test]
fn limit_carries_price_and_maps_day_to_gtc() {
    let b = order_body(&req("limit", "sell", 1.5, Some(65000.5), "day")).unwrap();
    assert_eq!(b["orderType"], "Limit");
    assert_eq!(b["price"], "65000.5");
    assert_eq!(b["timeInForce"], "GTC");
    assert_eq!(order_body(&req("limit", "buy", 1.0, Some(1.0), "ioc")).unwrap()["timeInForce"], "IOC");
    assert!(order_body(&req("limit", "buy", 1.0, None, "gtc")).is_err());
    assert!(order_body(&req("stop", "buy", 1.0, None, "gtc")).is_err());
}

#[test]
fn submit_rejection_surfaces_retmsg() {
    // envelope() is the only path submit_order reads a response through.
    let e = envelope(serde_json::json!({"retCode":170136,"retMsg":"Order quantity exceeded lower limit.","result":{}})).unwrap_err();
    assert!(e.to_string().contains("lower limit"));
}

#[test]
fn order_status_maps_every_bybit_terminal_state() {
    assert_eq!(order_outcome("Filled"), BrokerOrderOutcome::Filled);
    assert_eq!(order_outcome("Cancelled"), BrokerOrderOutcome::Canceled);
    assert_eq!(order_outcome("PartiallyFilledCanceled"), BrokerOrderOutcome::Canceled);
    assert_eq!(order_outcome("Deactivated"), BrokerOrderOutcome::Canceled);
    assert_eq!(order_outcome("Rejected"), BrokerOrderOutcome::Rejected);
    assert_eq!(order_outcome("New"), BrokerOrderOutcome::Working);
    assert_eq!(order_outcome("PartiallyFilled"), BrokerOrderOutcome::Working);
}

#[test]
fn open_orders_and_order_state_read_cum_exec_qty_and_avg_price() {
    // open_orders.json: one BTCUSDT order, orderLinkId = ORDER_ID, cumExecQty "0.0005".
    // order_history.json: one Filled order, cumExecQty "0.001", avgPrice "65000".
    // tickers_spot.json: BTCUSDT prevPrice24h "64000", ETHUSDT prevPrice24h "3000".
    // Asserts: open order fields; state Filled/0.001/65000; avgPrice "" → avg_px 0.0;
    // parse_prev_prices(.., &["BTCUSDT"]) == [("BTCUSDT", 64000.0)].
}
```

`req(order_type, side, qty, limit, tif)` is a test helper building a `BrokerOrderRequest` with `order_id: ORDER_ID` (`"3f6b1c2e-8a4d-4e5f-9b21-1c2d3e4f5a6b"`) and `symbol: "BTCUSDT"`.

- [ ] **Step 2: Run them to verify they fail** — `cargo test --bin oms adapters::bybit`. Expected: compile failure on `decimal` / `order_body`.

- [ ] **Step 3: Implement the interfaces above.** TIF: `IOC`, `FOK` pass through, everything else `GTC` (Binance's rule). `orderStatus` values come from Bybit's v5 order-status enum.

- [ ] **Step 4: Run tests to verify they pass** — same command. Expected: Task 1 + Task 2 tests all pass.

- [ ] **Step 5: Commit** — `feat(bybit): spot order entry, recon reads and daily stats`

---

### Task 3: Bybit credentials, venue entry, catalog sync and docs

**Files:**
- Modify: `src/credentials.rs` (variant, `Redact`, tests), `src/credentials_api.rs` (`parse_broker` arm `"BYBIT"`, `test_broker` arm), `src/setup/brokers.rs` (`Broker::Bybit`, `bybit_env()`, `provider_for`, `build_bybit`), `src/jobs/api.rs:150-154` (`"BYBIT" => Broker::Bybit`), `src/venues.rs` (Bybit entry; new `universe: Option<&'static str>` on `Venue`), `src/reload.rs` (the match arms the new variant forces: `expected_kind` → `"BYBIT"`; a temporary `classify` arm → `Failed("Bybit trading is not wired yet")` and a matching `build_registry` arm that registers nothing, both replaced in Task 5), `cockpit/src/venues.ts` (`universe: string | null`), `cockpit/src/components/CredentialsPanel.tsx` (delete `UNIVERSE_COPY`; prompt reads `venue.name` / `venue.universe`), `docs-site/src/content/docs/integrations/bybit.md`, `docs-site/src/content/docs/integrations/binance.md` (Prices row: Bybit is now a venue too).
- Test: `src/venues.rs`, `src/credentials.rs`, `cockpit/src/venues.test.ts`, `docs-site/test/venue-credentials.test.mjs` (existing tests, which pick up Bybit).

**Interfaces:**
- Consumes: Task 1's `BybitAdapter::new`, `api_key_info`, `InstrumentProvider`.
- Produces: `BrokerCredentials::Bybit { api_key: String, api_secret: String }` (serde tag `"Bybit"`); `Broker::Bybit` with `code() == "BYBIT"`, `connection_code() == "bybit-paper"` under the default env; `Venue.universe` (`"About 2,000 spot pairs"` for Binance, `"About 14,400 stocks and ETFs"` for Alpaca, `"About 500 spot pairs"` for Bybit; `None` for IBKR).

- [ ] **Step 1: Write the failing tests**

```rust
// src/venues.rs — extend the existing Binance prefill test's neighbour:
#[test]
fn bybit_is_a_key_and_secret_venue_on_its_testnet() {
    let bybit = venues().into_iter().find(|v| v.code == "BYBIT").expect("BYBIT listed");
    let names: Vec<_> = bybit.fields.iter().map(|f| (f.name, f.label, f.secret, f.section)).collect();
    assert_eq!(names, [("api_key", "API key", true, None), ("api_secret", "API secret", true, None)]);
    assert_eq!(bybit.environments[0].label, "spot testnet");
    assert_eq!(bybit.environments[0].keys_url, Some("https://testnet.bybit.com/app/user/api-management"));
}

// src/credentials.rs
#[test]
fn a_bybit_credential_round_trips_and_redacts_both_fields() {
    let c = BrokerCredentials::Bybit { api_key: "k".into(), api_secret: "s".into() };
    let back: BrokerCredentials = serde_json::from_slice(&serde_json::to_vec(&c).unwrap()).unwrap();
    assert!(matches!(back, BrokerCredentials::Bybit { .. }));
    assert!(redacted_fields(&c).iter().all(|f| f.secret && f.value.is_none()));
}
```

The existing `every_venue_the_server_accepts_is_listed_and_nothing_else` and `each_venues_fields_are_exactly_its_credentials_fields` tests, and the docs test for `BYBIT`, then fail until all three places agree.

- [ ] **Step 2: Run them to verify they fail** — `cargo test --bin oms venues:: credentials::`. Expected: `BYBIT listed` panic, and no `Bybit` variant.

- [ ] **Step 3: Implement.**
  - `test_broker` for Bybit calls `api_key_info()` under `TEST_TIMEOUT` → `Passed` / `Failed(msg)` / `timed_out("Bybit")`.
  - `bybit.md` becomes a venue page shaped like `binance.md`: "What it covers" table (Order entry REST; Executions private WebSocket; Instruments spot pairs; Prices Bybit's public book), `## Credentials` table (rows `| API key |`, `| API secret |`: create a **System-generated** HMAC key at testnet.bybit.com → API management with Read-Write spot trading permission), Loading instruments, Paper or live (`BYBIT_ENV`), and Limits (spot only, no FIX, BTCUSDT may appear twice in the picker, once per venue).
  - Keep the docs' description frontmatter ≤ 1 line.

- [ ] **Step 4: Run tests to verify they pass**
  - `cargo test --bin oms venues:: credentials:: setup::brokers`. Expected: all pass.
  - `cd cockpit && npm test && npx tsc --noEmit`. Expected: pass, clean.
  - `cd docs-site && npm run check`. Expected: pass, including `BYBIT's docs page explains every credentials field`.

- [ ] **Step 5: Commit** — `feat(bybit): credentials, venue entry, catalog sync and docs`

---

### Task 4: Bybit private execution stream

**Files:**
- Create: `src/bybit_stream.rs`
- Modify: `src/main.rs` (`mod bybit_stream;`)
- Test: inline module; samples `ws_execution.json`, `ws_order_cancelled.json`, `ws_order_rejected.json`, `ws_order_filled.json` under `src/adapters/testdata/bybit/`.

**Interfaces:**
- Consumes: Task 1's `BybitAdapter::{api_key, sign, private_ws_url}`; `crate::execution::process_execution_report`; `crate::recon_orders::run_order_reconcile`; `crate::stream_supervisor::{supervise, Session, StreamResult}`.
- Produces:
  - `pub async fn run(environment: &'static str, pool: PgPool, kafka: Option<KafkaClient>, adapter: Arc<BybitAdapter>, health: StreamHandle, position_changed_tx: Option<mpsc::Sender<()>>)`: same shape as `binance_stream::run`; supervisor name `"BYBIT/PAPER"` / `"BYBIT/LIVE"`.
  - `pub(crate) fn auth_message(adapter: &BybitAdapter, expires_ms: i64) -> serde_json::Value`: `{"op":"auth","args":[api_key, expires_ms, sign("GET/realtime{expires_ms}")]}`.
  - `pub(crate) fn reports(msg: &serde_json::Value) -> Vec<(Uuid, ExecutionReport)>`: `execution.spot` entries with `execType == "Trade"` → `Fill { execution_id: execId, fill_qty: execQty, fill_price: execPrice, venue: "BYBIT" }`; `order.spot` entries with `orderStatus` `Cancelled` / `PartiallyFilledCanceled` / `Deactivated` → `Canceled { reason: rejectReason if not "EC_NoError", venue }`; `Rejected` → `Reject { reason: rejectReason, venue }`; everything else, and any `orderLinkId` that isn't a UUID, → nothing.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn auth_signs_get_realtime_with_the_expiry() {
    let a = BybitAdapter::new("key".into(), "secret".into(), "PAPER");
    let m = auth_message(&a, 1700000005000);
    assert_eq!(m["op"], "auth");
    assert_eq!(m["args"][0], "key");
    assert_eq!(m["args"][2], a.sign("GET/realtime1700000005000"));
}

#[test]
fn fill_uses_exec_id_qty_and_price() {
    let r = reports(&sample("ws_execution.json"));
    assert_eq!(r.len(), 1);
    match &r[0].1 {
        ExecutionReport::Fill { execution_id, fill_qty, fill_price, venue } => {
            assert_eq!(execution_id, "<execId in the sample>");
            assert_eq!(*fill_qty, 0.001);
            assert_eq!(*fill_price, 65000.0);
            assert_eq!(venue, "BYBIT");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn status_partially_filled_canceled_and_deactivated_are_cancels_rejected_is_a_reject() { /* ws_order_cancelled.json holds one Cancelled, one PartiallyFilledCanceled, one Deactivated, one New; ws_order_rejected.json one Rejected */ }

#[test]
fn a_non_uuid_order_link_id_is_ignored() { /* orders placed outside the OMS */ }

#[test]
fn order_updates_never_produce_fills() {
    // order.spot with orderStatus "PartiallyFilled" and "Filled" (cumExecQty > 0): no reports at all.
    assert!(reports(&sample("ws_order_filled.json")).is_empty());
}
```

- [ ] **Step 2: Run them to verify they fail** — `cargo test --bin oms bybit_stream`. Expected: compile failure, `reports` not found.

- [ ] **Step 3: Implement** `bybit_stream.rs` on `binance_stream.rs`'s pattern:
  - connect `private_ws_url()`, send `auth_message` with `expires = now + 10_000`;
  - on `{"op":"auth","success":true}` send `{"op":"subscribe","args":["order.spot","execution.spot"]}`;
  - on a successful subscribe ack: `health.set_live()` and `run_order_reconcile(pool, kafka, adapter, "BYBIT", "bybit", tx)`;
  - every 20 s send `{"op":"ping"}` and run the same reconcile;
  - apply `reports(..)` through `process_execution_report(.., "bybit", ..)`;
  - an auth or subscribe failure returns `Err`, so the supervisor backs off.

- [ ] **Step 4: Run tests to verify they pass** — same command. Expected: 5 passed.

- [ ] **Step 5: Commit** — `feat(bybit): private order and execution stream`

---

### Task 5: Registration, Reload and day-P&L wiring

**Files:**
- Modify:
  - `src/reload.rs`: `classify` arm `BrokerCredentials::Bybit { .. } => Registered`; `build_registry` arm registers `"BYBIT"` and pushes onto a new `RegistrationOutput.bybit_adapters: Vec<(&'static str, Arc<BybitAdapter>)>`; `pub(crate) fn bybit_exec_stream_code(env) -> String` (`"bybit-paper:exec"`); `pub async fn restart_bybit_stream(env_name, adapter, deps, streams)`; `ReloadStreams::bybit(..)` plus `LiveReloadStreams` impl.
  - `src/admin.rs` (`apply_reload`):
    - restart each Bybit environment whose outcome `restarts_execution_stream` and that has an entry in `bybit_adapters`;
    - add `BYBIT` to the end-of-pass "registry has none → `stop_execution_stream`" loop.
  - `src/main.rs`: boot spawns `restart_bybit_stream` for each `bybit_adapters` entry, after the Binance loop.
  - `src/daily_stats_poller.rs`: `broker_serves_venue` arm `"BYBIT" => venue == "BYBIT"`.
  - `src/reload_tests.rs`: any test double implementing `ReloadStreams` gets a recording `bybit`.
- Test: `src/reload.rs` tests, `src/reload_tests.rs`, `src/daily_stats_poller.rs` tests.

**Interfaces:**
- Consumes: Task 3's `BrokerCredentials::Bybit`, Task 4's `bybit_stream::run`, Task 1's `BybitAdapter::new`.
- Produces: `RegistrationOutput { registry, report, alpaca_creds, binance_rest_adapters, bybit_adapters }`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_bybit_credential_applies_on_reload_without_a_restart() {
    let c = conn("bybit-paper", "ACTIVE", CredentialState::Configured(BrokerCredentials::Bybit { api_key: "k".into(), api_secret: "s".into() }));
    assert!(matches!(classify(&c, false), ConnectionOutcome::Registered));
}

#[test]
fn bybit_exec_stream_code_is_per_environment() {
    assert_eq!(bybit_exec_stream_code("PAPER"), "bybit-paper:exec");
    assert_ne!(bybit_exec_stream_code("PAPER"), binance_exec_stream_code("PAPER"));
}

#[test]
fn bybit_serves_only_bybit_symbols_for_daily_stats() {
    assert!(broker_serves_venue("BYBIT", "BYBIT"));
    assert!(!broker_serves_venue("BYBIT", "BINANCE"));
    assert!(!broker_serves_venue("BINANCE", "BYBIT"));
}
```

In `reload_tests.rs`, following the existing Alpaca reload tests' fake-`ReloadStreams` pattern:
- `reload_registers_bybit_and_restarts_its_stream_once`: the recording double sees exactly one `bybit("PAPER", ..)` call.
- `reload_with_bybit_disabled_stops_its_stream`: `stop_execution_stream` is reached for `bybit-paper:exec`.

- [ ] **Step 2: Run them to verify they fail** — `cargo test --bin oms reload daily_stats_poller`. Expected: compile failure on `bybit_exec_stream_code` / `bybit_adapters`.

- [ ] **Step 3: Implement** the wiring above. The `Error`-carry-forward arm in `build_registry` needs no change: `current.get("BYBIT", env)` already covers it through the generic path.

- [ ] **Step 4: Run the whole suite**
  - `cargo test`. Expected: all pass, the same ignored count as before plus none new.
  - `cargo clippy -q --bin oms`. Expected: no warnings in touched files.

- [ ] **Step 5: Commit** — `feat(bybit): register, reload and stream Bybit connections`

---

### Task 6: End-to-end testnet check (with the user)

**Files:** none, unless the check finds a bug (then a failing test first, in the owning task's file).

- [ ] **Step 1: Restart the dev OMS** on the branch. Expected: log line `bootstrap: created broker_connection bybit-paper`; Broker connections lists `bybit-paper`, and the venue dropdown includes "Bybit — Crypto spot pairs".
- [ ] **Step 2: User step.** The user creates a testnet API key (System-generated, Read-Write, spot) at testnet.bybit.com, pastes it into **bybit-paper → Credentials**, and presses **Test** then **Save**. Expected: "credential verified"; reload outcome "applied immediately"; "Live execution streams" shows BYBIT PAPER live.
- [ ] **Step 3: Load the universe** from the prompt or the **Load** button. Expected: Loaded, roughly 500 instruments; BTCUSDT@BYBIT is searchable.
- [ ] **Step 4: User step.** Create (or reuse) a portfolio account linked to `bybit-paper`; in the trader UI, market-buy 0.001 BTCUSDT@BYBIT. Expected: the blotter shows **Filled** with Bybit's fill price and quantity 0.001 within a few seconds.
- [ ] **Step 5: Limit + cancel.** Limit-buy 0.001 at roughly half the market price, then cancel from the UI. Expected: Working, then Canceled, with the cancel arriving over the stream (log `bybit` actor).
- [ ] **Step 6: Restart reconcile.** Place another far-off limit, stop the OMS, cancel it on testnet.bybit.com, start the OMS. Expected: the order shows Canceled after boot without user action.
- [ ] **Step 7: Full checks:** `cargo test`, `cd cockpit && npm test && npx tsc --noEmit && npm run build`, `./cockpit/check-api-base.sh`, `./cockpit/check-palette.sh`, `cd docs-site && npm run check`. Expected: all pass.
