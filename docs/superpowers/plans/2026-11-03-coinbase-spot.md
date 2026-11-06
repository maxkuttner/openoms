# Coinbase Spot (LIVE) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Trade Coinbase spot (Advanced Trade) from openOMS, LIVE only: credentials, instruments, orders, fills, reconciliation, prices and day P&L, proven by a small real order filled from the trader UI.

**Architecture:** Mirror the Bybit integration.
- **Adapter:** `src/adapters/coinbase.rs` (REST, ES256 JWT per request).
- **Fills:** `src/coinbase_stream.rs` (authenticated `user` channel, cumulative order state turned into fills against the OMS's own `cum_qty`).
- **Prices:** `src/coinbase_feed.rs` (public `ticker` channel marking `COINBASE` instruments).
- **Rest of the wiring:** a `BrokerCredentials::Coinbase` variant, `Broker::Coinbase` (always LIVE), a LIVE-only venue entry, and registration in `reload.rs` with stream restart on Reload.

**Tech Stack:** Rust; `p256` 0.13 (already in Cargo.lock via openidconnect) for ES256; reqwest; tokio-tungstenite; sqlx; the cockpit and docs unchanged in shape.

**Spec:** `docs/superpowers/specs/2026-11-03-coinbase-spot-design.md`

## Global Constraints

- **Scope:** spot only, LIVE only.
  - REST base `https://api.coinbase.com/api/v3/brokerage`.
  - User stream `wss://advanced-trade-ws-user.coinbase.com`; market data `wss://advanced-trade-ws.coinbase.com`.
- **JWT:** one per REST request and one per WebSocket subscribe, as follows.
  - Header: `alg: ES256`, `typ: JWT`, `kid: <key name>`, `nonce: <16 random bytes hex>`.
  - Claims: `iss: "cdp"`, `sub: <key name>`, `nbf: now`, `exp: now + 120`.
  - REST tokens add `uri: "<METHOD> api.coinbase.com<path>"`, where the path excludes the query; WebSocket tokens have no `uri`.
  - Signature: raw 64-byte r‖s, base64url without padding.
- **Credential:**
  - fields `api_key` (key name `organizations/…/apiKeys/…`) and `private_key` (EC PEM, SEC1 `EC PRIVATE KEY` or PKCS#8);
  - labels "API key", "Private key (PEM)"; both secret; no section;
  - a PEM pasted with literal `\n` escapes is accepted.
- **Instruments:**
  - venue `COINBASE`, symbol = `product_id` (`BTC-USD`), currency = `quote_currency_id`, native id = `base_currency_id`;
  - `product_type == "SPOT"` only;
  - tradeable iff `status == "online"` and none of `trading_disabled`, `is_disabled`, `cancel_only`, `view_only`.
- **Orders:** `client_order_id` = the OMS order UUID; DAY → GTC; `IOC` → `sor_limit_ioc`; `FOK` → `limit_limit_fok`.
- **Fills:** a fill is `coinbase cumulative_quantity − OMS cum_qty`, never per message. The execution path does not dedupe by execution id (`execution.rs:119` only skips terminal orders).
- **Test button:** passes only with `can_trade && !can_transfer`.
- **Environment:** `Broker::Coinbase.environment()` is always `"LIVE"`; the connection code is `coinbase-live`.
- **Tests:** network-free unit tests (recorded JSON). DB tests stay `#[ignore]`. Never create databases on the shared dev Postgres; never handle the user's key.
- **Commits:** commit only on branch `feat/coinbase-spot`; never stage `.gitignore`, `.ignore`, or `docs/superpowers/plans/2026-09-28-options-search-chain-strategy.md`.

## Review Focus

1. A repeated or reordered user-channel message (snapshot after reconnect, two updates with the same cumulative) must book nothing new. Pinned in Task 4 (`a_repeat_books_nothing`).
2. Create-order answering HTTP 200 with `success: false` must become `BrokerRejected(<failure reason>)`, never a routed order. Pinned in Task 2 (`create_failure_is_a_rejection`).
3. A JWT that Coinbase rejects because of a wrong `uri` (query string included, wrong host) fails every signed call. Pinned in Task 1 (`rest_uri_excludes_the_query`).
4. `BTC-USD@COINBASE` must not be subscribed on the Bybit feed (an invalid topic can fail Bybit's whole subscribe). Alpaca must never be asked for its daily stats. Pinned in Task 5 (`bybit_skips_dashed_symbols`) and Task 6 (`coinbase_serves_only_coinbase_symbols`).
5. A key with transfer permission must fail **Test**. Pinned in Task 3 (`a_key_that_can_transfer_fails_the_test`).

---

### Task 1: Coinbase adapter core — JWT, envelope, spot catalog

**Files:** create `src/adapters/coinbase.rs`; modify `src/adapters/mod.rs` (`pub mod coinbase;`), `Cargo.toml` (`p256 = { version = "0.13", features = ["ecdsa", "pem", "pkcs8"] }`); fixtures `src/adapters/testdata/coinbase/products_spot.json` (trimmed real `GET /market/products?product_type=SPOT`: BTC-USD, ETH-EUR, one edited to `trading_disabled: true`, one edited to `product_type: "FUTURE"`).

**Produces:**
- `CoinbaseAdapter::new(key_name: String, private_key_pem: &str) -> Result<Self, String>`, `key_name()`.
- `pub fn jwt(&self, uri: Option<&str>) -> String`; `pub(crate) fn rest_uri(method: &str, path: &str) -> String`.
- `async fn get(&self, path: &str, query: &str, signed: bool) -> Result<Value, BrokerError>` and `async fn post(&self, path: &str, body: &Value)`. Non-2xx → `BrokerRejected` with the body's `message`/`error`.
- `pub(crate) fn parse_spot_products(body: &Value) -> Vec<BrokerInstrument>`; `list_spot_products()` (public `/market/products?product_type=SPOT`); `impl InstrumentProvider`.
- `pub async fn key_permissions(&self) -> Result<Value, BrokerError>`.

- [ ] **Step 1: Failing tests**
  - `jwt_header_and_claims_follow_coinbase`: decode the header and claims of `jwt(Some("GET api.coinbase.com/api/v3/brokerage/accounts"))`. Assert:
    - header `alg`, `typ`, `kid`, and a 32-hex-char `nonce`;
    - claims `iss "cdp"`, `sub`, `exp - nbf == 120`, and `uri`.
  - `jwt_signature_verifies_with_the_public_key`: a P-256 key pair generated in the test; the signature (64 raw bytes) verifies over `header.claims`.
  - `websocket_jwt_has_no_uri`.
  - `rest_uri_excludes_the_query`: `rest_uri("GET", "/api/v3/brokerage/orders/historical/batch?order_status=OPEN") == "GET api.coinbase.com/api/v3/brokerage/orders/historical/batch"`.
  - `a_pem_pasted_with_literal_newlines_is_accepted` and `a_bad_pem_is_refused`.
  - `spot_products_are_coinbase_venue_rows`, for BTC-USD:
    - venue `COINBASE`, currency `USD`, native id `BTC`;
    - `price_increment` 0.01, `size_increment` 1e-8, `price_precision` 2;
    - `min_quantity` = `base_min_size`, `max_quantity` = `base_max_size`, `min_notional` = `quote_min_size`;
    - tradeable.

    The disabled product is listed but not tradeable; the FUTURE product is absent.
- [ ] **Step 2:** `cargo test --bin oms adapters::coinbase`. Expected: compile failure, no `CoinbaseAdapter`.
- [ ] **Step 3:** Implement. Sign with `p256::ecdsa::SigningKey` and `Signature::to_bytes()`. Load the key with `SecretKey::from_sec1_pem`, falling back to `from_pkcs8_pem`. A missing body field is an error, never a zero.
- [ ] **Step 4:** Same command. Expected: all pass.
- [ ] **Step 5:** Commit `feat(coinbase): Advanced Trade adapter core and spot catalog`.

### Task 2: Orders, reconciliation reads, daily stats

**Files:** `src/adapters/coinbase.rs`; fixtures `create_ok.json`, `create_failed.json`, `cancel_ok.json`, `orders_open.json`, `order_filled.json`, documented shapes with invented values.

**Produces:**
- Pure functions:
  - `pub(crate) fn decimal(x: f64) -> String` (as Bybit's);
  - `order_body(req) -> Result<Value, BrokerError>`;
  - `parse_create(body) -> Result<String /*order_id*/, BrokerError>`;
  - `parse_cancel(body) -> Result<(), BrokerError>`;
  - `order_outcome(status) -> BrokerOrderOutcome`;
  - `parse_order_state(order) -> BrokerOrderState`;
  - `parse_open_orders(body) -> Vec<BrokerOpenOrder>`;
  - `parse_prev_prices(body, wanted) -> Vec<(String, f64)>`.
- `impl BrokerAdapter`:
  - `submit_order`: `POST /orders`;
  - `cancel_order`: `POST /orders/batch_cancel {"order_ids":[id]}`;
  - `open_orders`: `GET /orders/historical/batch?order_status=OPEN`, following `cursor` while `has_next`;
  - `order_status`: `GET /orders/historical/{id}`;
  - `daily_stats`: public `/market/products?product_type=SPOT`, previous close = `price / (1 + price_percentage_change_24h / 100)`.

- [ ] **Step 1: Failing tests**
  - `market_orders_are_sized_in_the_base_coin`: a buy gives `{"market_market_ioc":{"base_size":"0.0001"}}`, side `BUY`, `client_order_id` = UUID, `product_id` = symbol.
  - `limit_tif_maps_to_coinbase_configurations`: GTC/DAY → `limit_limit_gtc{base_size, limit_price, post_only:false}`; IOC → `sor_limit_ioc`; FOK → `limit_limit_fok`. A missing limit price and an unknown type are errors.
  - `decimal_drops_float_noise`.
  - `create_ok_returns_the_order_id`.
  - `create_failure_is_a_rejection`: `success:false` with `new_order_failure_reason: "INSUFFICIENT_FUND"` → `BrokerRejected` containing it.
  - `cancel_failure_is_a_rejection`.
  - `order_status_maps_coinbase_states`: FILLED → Filled; CANCELLED, EXPIRED → Canceled; FAILED → Rejected; OPEN, PENDING, CANCEL_QUEUED → Working.
  - `open_orders_and_order_state_read_cumulative_and_average`.
  - `previous_close_comes_from_the_24h_change`: price 102, change 2 → 100.
- [ ] **Step 2:** `cargo test --bin oms adapters::coinbase`. Expected: compile failure.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Same command; all pass.
- [ ] **Step 5:** Commit `feat(coinbase): spot order entry, recon reads and daily stats`.

### Task 3: Credentials, LIVE-only venue, catalog sync, docs

**Files:**
- `src/credentials.rs`: `BrokerCredentials::Coinbase { api_key, private_key }`, both hidden on redaction.
- `src/credentials_api.rs`: `parse_broker` `"COINBASE"`, and `test_broker`, which uses `key_permissions` and fails with "the key can transfer funds; create one with View and Trade only" or "the key cannot trade".
- `src/setup/brokers.rs`: `Broker::Coinbase`, code `COINBASE`, `environment()` always `"LIVE"`, `build_coinbase`.
- `src/jobs/api.rs`.
- `src/venues.rs`: a Coinbase entry with **only** a LIVE environment (label "live account", keys URL `https://portal.cdp.coinbase.com/projects/api-keys`), `universe: Some("About 900 spot pairs")`. The venue test becomes "LIVE always, PAPER where offered".
- `src/reload.rs`: temporary `classify` and `build_registry` arms (Failed / nothing), replaced in Task 6.
- Database: `db/migrations/ods/public/0025_INSERT_COINBASE_VENUE.sql` inserts venue `COINBASE`; also add it to `db/scripts/seed_crypto_venues.sql`. `src/setup/database/assets.rs`: public migration count 25.
- Docs: `docs-site/src/content/docs/integrations/coinbase.md` (same shape as `bybit.md`) with:
  - a `:::caution` "LIVE only, every order is real money";
  - a `## Credentials` table with rows `| API key |`, `| Private key (PEM) |`, created under CDP portal → API keys with View and Trade permissions, an ECDSA key (not Ed25519), and no Transfer;
  - a note that there are no CHF pairs (quote currencies include USD, USDC, EUR, GBP).

  Add it to the sidebar in `docs-site/astro.config.mjs`.

**Test:** existing venue/credential/docs tests, plus:
- `coinbase_is_live_only_with_key_and_private_key`;
- `a_coinbase_credential_round_trips_and_redacts_both_fields`;
- `a_key_that_can_transfer_fails_the_test`, on a pure `judge_permissions(&Value) -> TestOutcome`.

- [ ] Steps: failing tests → run (`cargo test --bin oms venues:: credentials` fails) → implement → `cargo test --bin oms`, `cd cockpit && npm test && npx tsc --noEmit`, `cd docs-site && npm run check` all pass → commit `feat(coinbase): credentials, live-only venue, catalog sync and docs`.

### Task 4: User stream — cumulative state to fills

**Files:**
- Create `src/coinbase_stream.rs`.
- Modify `src/main.rs` (`mod coinbase_stream;`).
- Modify `src/recon_orders.rs`: `pub(crate) async fn apply_broker_progress(pool, kafka, order_id, cum: f64, fill_price: f64, venue, actor, tx)`, which books `cum − oms cum_qty` (> EPS) as a Fill with `execution_id = format!("{venue}-{order_id}-{cum}")` and does nothing otherwise.
- Fixtures: `ws_user_snapshot.json`, `ws_user_partial.json`, `ws_user_filled.json`, `ws_user_cancelled.json`, from the AsyncAPI `Order` schema.

**Produces:**
- `pub(crate) fn subscribe_messages(adapter) -> [Value; 2]`: `{"type":"subscribe","channel":"user","jwt":…}` and the same for `heartbeats`.
- `pub(crate) enum Update { Progress { cum, avg }, Terminal(BrokerOrderState) }`.
- `pub(crate) fn updates(msg) -> Vec<(Uuid, Update)>` over `events[].orders[]`, keyed by `client_order_id` (non-UUID skipped):
  - FILLED, CANCELLED, EXPIRED, FAILED → `Terminal`;
  - OPEN or PENDING with `cumulative_quantity > 0` → `Progress`.
- `pub(crate) fn fill_price(prev: Option<(f64, f64)>, cum: f64, avg: f64) -> f64` = `(avg·cum − avg₀·cum₀)/(cum − cum₀)`, or `avg` when there is no previous state or no positive delta.
- `run(...)`, same signature as `bybit_stream::run`. It:
  - subscribes within 5 s of connecting, with a fresh JWT each time;
  - marks the stream live on the first `user` snapshot;
  - reconciles on that snapshot and every 20 s;
  - keeps the last `(cum, avg)` per order in memory for `fill_price`;
  - sends `Terminal` through `apply_broker_terminal` and `Progress` through `apply_broker_progress`.

- [ ] **Step 1: Failing tests**
  - `subscribe_messages_carry_a_fresh_jwt_and_no_uri`.
  - `a_partial_fill_is_progress_with_cumulative_and_average`.
  - `filled_cancelled_expired_failed_are_terminal`.
  - `a_repeat_books_nothing`: the same partial twice yields two `Progress` updates with equal `cum`. The booking rule is "cum − OMS cum"; assert on the pure `missing_qty(oms_cum, cum)` helper this task adds to `recon_orders.rs` (and use it inside `apply_broker_progress`).
  - `fill_price_is_the_price_of_the_increment`: (cum 1 @ 100) then (cum 3 @ 102) → 103.
  - `a_non_uuid_client_order_id_is_ignored`.
- [ ] **Step 2:** `cargo test --bin oms coinbase_stream recon_orders`. Expected: compile failure.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Same command; all pass.
- [ ] **Step 5:** Commit `feat(coinbase): user stream turns cumulative order state into fills`.

### Task 5: Coinbase price feed

**Files:**
- Create `src/coinbase_feed.rs` (`LiveQuoteFeed`, code `COINBASE`; candidates `SPOT`/`CRYPTO`/`venue: Some("COINBASE")`; identity symbology). It subscribes `ticker` for the symbols plus `heartbeats`, and emits `Quote` from `best_bid`/`best_ask`, skipping events where either is empty.
- Modify `src/feeds.rs` (`ALL`) and `src/main.rs` (spawn it beside the Bybit feed).
- Modify `src/bybit_feed.rs`: `to_feed_symbol` returns `None` for a symbol containing `-`.
- Add `db/migrations/ods/oms/0031_INSERT_COINBASE_FEED_POLICY.sql` (`('COINBASE','SPOT',10)`); `assets.rs` oms count 32.
- Fixture `ws_ticker.json`.

- [ ] **Step 1: Failing tests**
  - `a_ticker_event_is_a_quote_from_best_bid_and_ask`;
  - `an_empty_side_emits_nothing`;
  - `coinbase_feed_covers_only_coinbase_venue`;
  - `bybit_skips_dashed_symbols` (in `bybit_feed.rs`);
  - `every_feed_is_reachable_by_its_own_code` (existing; now includes COINBASE).
- [ ] **Step 2:** `cargo test --bin oms coinbase_feed bybit_feed feeds`. Expected: failure.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Pass.
- [ ] **Step 5:** Commit `feat(coinbase): public ticker feed marks Coinbase instruments`.

### Task 6: Registration, Reload, day-P&L wiring

**Files:**
- `src/reload.rs`:
  - `classify` Coinbase → `Registered`;
  - `build_registry` registers `"COINBASE"` and fills `RegistrationOutput.coinbase_adapters`;
  - `coinbase_exec_stream_code`;
  - `restart_coinbase_stream`;
  - `ReloadStreams::coinbase` plus the live impl.
- `src/admin.rs` `apply_reload`: restart each `coinbase_adapters` entry, and stop the stream when the registry has no `COINBASE`.
- `src/main.rs`: boot spawn.
- `src/daily_stats_poller.rs`: add `COINBASE` to `CRYPTO_VENUES` and a `"COINBASE" => venue == "COINBASE"` arm.
- `src/reload_tests.rs`: `FakeStreams::coinbase`, and Coinbase rows in the ignored lifecycle test.

**Tests:**
- `a_coinbase_credential_applies_on_reload_without_a_restart`;
- `coinbase_exec_stream_code_is_its_own`;
- `a_configured_coinbase_connection_is_registered_and_handed_back`;
- `coinbase_serves_only_coinbase_symbols`, including that `ALPACA` no longer serves `COINBASE`.

- [ ] Steps: failing tests → run (compile failure) → implement → `cargo test`, `cargo clippy -q --bin oms` (no new warnings in touched files) → commit `feat(coinbase): register, reload and stream Coinbase connections`.

### Task 7: Live check with the user

- [ ] `oms database migrate` on the dev DB (0025 public, 0031 oms). Restart the dev OMS. Expected:
  - log `bootstrap: created broker_connection coinbase-live`;
  - the venue dropdown offers "Coinbase — Crypto spot pairs" with only LIVE.
- [ ] Load the universe from `coinbase-live`: it uses the public products call, so it may work before credentials. Expected: about 900 instruments, and `BTC-USD@COINBASE` gets a price from the feed.
- [ ] **User:** creates an ECDSA CDP key with View and Trade only and saves it in `coinbase-live`. **Test** passes; the stream shows live.
- [ ] **User:** links an account to `coinbase-live` and places a market buy worth a few EUR/USD from the trader UI. Expected: **Filled** with Coinbase's quantity and price.
- [ ] A far-off limit order is placed, then cancelled from the UI.
- [ ] Full checks: `cargo test`, cockpit `npm test`/`tsc`/`build`, `check-api-base.sh`, `check-palette.sh`, docs `npm run check`.
