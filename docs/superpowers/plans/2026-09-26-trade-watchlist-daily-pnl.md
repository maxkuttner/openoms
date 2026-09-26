# Live Watchlist + Day P&L / Net Exposure Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A trader can maintain a personal watchlist with live ticking prices, and the Positions & P&L page gains two real stat cards (Day P&L, Net Exposure) — the only pieces of `Design.pdf`'s trade screen that are backed by real or cheaply-obtainable data.

**Architecture:** A new `DailyStatsStore` (mirrors the existing `MarkStore` exactly: in-memory `instrument_id -> {prev_close, ts}`) sits alongside it, populated every 5 minutes by a poller that calls a new optional `daily_stats` method on `BrokerAdapter` (same default-`NotConfigured`-and-skip idiom this trait already uses for `open_orders`/`order_status`), implemented for Alpaca (REST snapshot) and Binance (public 24hr ticker) to start. A new `watchlist_item` table persists which instruments each principal is watching. The browser polls a new `GET /marks` endpoint (merging both stores) on the same interval convention `TradeBlotter` already uses — no new transport.

**Tech Stack:** Rust/axum/sqlx (backend), React/TypeScript/Mantine/TanStack Query (`cockpit/src/trade`), Postgres migrations.

**Spec:** `docs/superpowers/specs/2026-09-26-trade-watchlist-daily-pnl-design.md`

## Global Constraints

- No new transport: the frontend polls REST via React Query, matching `TradeBlotter.tsx`'s existing `refetchInterval` pattern. No WebSocket.
- A field with no data is `null` in API responses, never a fabricated `0` — this is a hard rule this codebase already enforces elsewhere (`Positions.tsx`'s existing unrealized-P&L handling) and every task below must preserve it.
- `oms` schema tables reference `instrument_id` as `TEXT`, not a real foreign key to `public.instrument` (cross-schema FK avoided deliberately, per `position` table's existing convention) — `watchlist_item` follows the same shape.
- `BrokerAdapter`'s new `daily_stats` method follows the exact idiom `open_orders`/`order_status` already use: a default trait-method body returning `Err(BrokerError::NotConfigured(...))`, overridden only by adapters that support it.
- This codebase has no HTTP-mocking library (checked: no mockito/wiremock anywhere; existing adapter tests — `alpaca_exchange_to_mic` etc. — only unit-test pure functions, never the real network calls). Every adapter task below follows the same shape: a pure, unit-tested parsing function, called by a thin, not-directly-unit-tested async network wrapper.
- Out of scope (per spec's Non-goals): price chart, order book, stop/stop-limit orders, buying power, restricted-list checks, order routing, iceberg/trail, est. commission, sector allocation, VaR, beta-weighted delta, margin utilization, auto-seeding the watchlist from held positions.

## Review Focus

- **Watchlisting an equity symbol (e.g. AAPL) with no live-quote feed registered** — only Binance, Bybit, and OPRA implement `LiveQuoteFeed` today (checked: `grep -rl "impl LiveQuoteFeed"`); no equity feed exists. A watchlisted equity will correctly show `bid`/`ask`/`mid` as `null` forever in this deployment. That's expected, not a bug — Task 8's tests must assert `/marks` returns nulls for an instrument with no `MarkStore` entry, not error or fabricate a price.
- **`daily_stats` populates independently of live marks** — an instrument can have a `prev_close` with no live `bid`/`ask` (or vice versa); Task 8's merge logic must treat the two stores as fully independent, never requiring one to render the other.
- **Duplicate watchlist add** — `POST /watchlist` for an instrument already on the list must be idempotent (200/204), never a 500 from a unique-constraint violation. Task 1's table needs the composite PK for this to even be possible; Task 8's handler must catch the conflict explicitly.
- **Cross-principal watchlist access** — every watchlist query must scope by the session's own `principal_id` (from `Extension<AuthContext>`), the same way `list_portfolios` does. Task 8's tests must assert principal A can never see or delete principal B's watchlist row, even by guessing an instrument_id.
- **One adapter's `daily_stats` failing must not block another's** — a rate-limited or unreachable Alpaca must not prevent Binance's stats from updating that poll cycle. Task 6's poller must process adapters independently and log-and-continue per adapter, not per poll cycle.

---

## Task 1: `watchlist_item` table

**Files:**
- Create: `db/migrations/ods/oms/0025_CREATE_WATCHLIST_ITEM_TABLE.sql`

**Interfaces:**
- Produces: table `watchlist_item(principal_id UUID, instrument_id TEXT, created_at TIMESTAMPTZ)`, PK `(principal_id, instrument_id)` — Task 8's handlers query/write this directly.

- [ ] **Step 1: Write the migration**

```sql
-- What a principal is watching for live price/day-change on the trade screen.
-- No surrogate id: the natural key is the pair itself, and there is nothing
-- else to reference it by. instrument_id is TEXT, not a real FK, matching
-- position's own instrument_id column — oms-schema tables don't take a
-- cross-schema FK into public.instrument.
CREATE TABLE watchlist_item (
    principal_id   UUID NOT NULL REFERENCES principal(id),
    instrument_id  TEXT NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (principal_id, instrument_id)
);
```

- [ ] **Step 2: Run the migration against your dev database**

Run: `cargo run -- database migrate`
Expected: output names `0025_CREATE_WATCHLIST_ITEM_TABLE` as applied.

- [ ] **Step 3: Verify the table exists**

Run: `cargo run -- database status`
Expected: no pending migrations; `0025_CREATE_WATCHLIST_ITEM_TABLE` listed as applied.

- [ ] **Step 4: Commit**

```bash
git add db/migrations/ods/oms/0025_CREATE_WATCHLIST_ITEM_TABLE.sql
git commit -m "feat(db): add watchlist_item table"
```

---

## Task 2: `DailyStatsStore`

**Files:**
- Create: `src/daily_stats.rs`
- Modify: `src/main.rs:60` (add `mod daily_stats;` next to the existing `mod marks;`)

**Interfaces:**
- Produces: `DailyStat { prev_close: f64, ts: DateTime<Utc> }`, `DailyStatsStore::new() -> Self`, `.set(instrument_id: i64, prev_close: f64)`, `.get(instrument_id: i64) -> Option<DailyStat>`, `.get_all() -> HashMap<i64, DailyStat>` — Task 6 (poller) writes via `.set`, Task 8 (handler) reads via `.get`/`.get_all`.

- [ ] **Step 1: Write the failing test**

```rust
// bottom of src/daily_stats.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_then_get_round_trips() {
        let store = DailyStatsStore::new();
        store.set(42, 100.50);
        let stat = store.get(42).expect("just set");
        assert_eq!(stat.prev_close, 100.50);
    }

    #[test]
    fn missing_instrument_is_none_not_zero() {
        let store = DailyStatsStore::new();
        assert!(store.get(999).is_none());
    }

    #[test]
    fn get_all_returns_every_entry() {
        let store = DailyStatsStore::new();
        store.set(1, 10.0);
        store.set(2, 20.0);
        let all = store.get_all();
        assert_eq!(all.len(), 2);
        assert_eq!(all[&1].prev_close, 10.0);
        assert_eq!(all[&2].prev_close, 20.0);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib daily_stats:: -- --nocapture`
Expected: FAIL — `daily_stats` module doesn't exist yet (won't even compile).

- [ ] **Step 3: Write the implementation**

```rust
// src/daily_stats.rs
//! Previous-close cache, sibling to `MarkStore` (src/marks.rs) but a different
//! cadence and source: `MarkStore` is written continuously by the streaming
//! quote feeds, this is written every few minutes by `daily_stats_poller`
//! calling each broker adapter's own snapshot/24hr-stats endpoint. Kept
//! separate rather than folded into `Mark` because the two are populated
//! independently and one being absent must never block the other.

use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

#[derive(Clone, Copy)]
pub struct DailyStat {
    pub prev_close: f64,
    pub ts: DateTime<Utc>,
}

#[derive(Clone, Default)]
pub struct DailyStatsStore {
    inner: Arc<RwLock<HashMap<i64, DailyStat>>>,
}

impl DailyStatsStore {
    pub fn new() -> Self { Self::default() }

    pub fn set(&self, instrument_id: i64, prev_close: f64) {
        if let Ok(mut m) = self.inner.write() {
            m.insert(instrument_id, DailyStat { prev_close, ts: Utc::now() });
        }
    }

    pub fn get(&self, instrument_id: i64) -> Option<DailyStat> {
        self.inner.read().ok().and_then(|m| m.get(&instrument_id).copied())
    }

    pub fn get_all(&self) -> HashMap<i64, DailyStat> {
        self.inner.read().ok().map(|m| m.clone()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_then_get_round_trips() {
        let store = DailyStatsStore::new();
        store.set(42, 100.50);
        let stat = store.get(42).expect("just set");
        assert_eq!(stat.prev_close, 100.50);
    }

    #[test]
    fn missing_instrument_is_none_not_zero() {
        let store = DailyStatsStore::new();
        assert!(store.get(999).is_none());
    }

    #[test]
    fn get_all_returns_every_entry() {
        let store = DailyStatsStore::new();
        store.set(1, 10.0);
        store.set(2, 20.0);
        let all = store.get_all();
        assert_eq!(all.len(), 2);
        assert_eq!(all[&1].prev_close, 10.0);
        assert_eq!(all[&2].prev_close, 20.0);
    }
}
```

- [ ] **Step 4: Register the module**

In `src/main.rs`, next to the existing `mod marks;` (line 60):

```rust
mod daily_stats;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib daily_stats:: -- --nocapture`
Expected: PASS — 3 tests.

- [ ] **Step 6: Commit**

```bash
git add src/daily_stats.rs src/main.rs
git commit -m "feat(marks): add DailyStatsStore alongside MarkStore"
```

---

## Task 3: `BrokerAdapter::daily_stats` default method

**Files:**
- Modify: `src/adapters/mod.rs` (trait `BrokerAdapter`, right after the existing `order_status` default method)

**Interfaces:**
- Consumes: `BrokerError` (already defined in this file).
- Produces: `async fn daily_stats(&self, symbols: &[String]) -> Result<Vec<(String, f64)>, BrokerError>` on every `BrokerAdapter` — Task 4/5 override it for Alpaca/Binance; Task 6's poller calls it on every registered adapter and treats `Err(BrokerError::NotConfigured(_))` as "skip, not an error."

- [ ] **Step 1: Write the failing test**

```rust
// bottom of src/adapters/mod.rs, inside the existing test module if one
// exists, or a new `#[cfg(test)] mod tests { use super::*; ... }` block.
struct BareAdapter;

#[async_trait::async_trait]
impl BrokerAdapter for BareAdapter {
    async fn submit_order(&self, _req: &BrokerOrderRequest) -> Result<BrokerOrderResponse, BrokerError> {
        unimplemented!()
    }
    async fn cancel_order(&self, _external_order_id: &str, _symbol: &str) -> Result<(), BrokerError> {
        unimplemented!()
    }
}

#[tokio::test]
async fn daily_stats_defaults_to_not_configured() {
    let adapter = BareAdapter;
    let result = adapter.daily_stats(&["AAPL".to_string()]).await;
    assert!(matches!(result, Err(BrokerError::NotConfigured(_))));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib adapters::tests::daily_stats_defaults_to_not_configured -- --nocapture`
Expected: FAIL — `daily_stats` is not a method on `BrokerAdapter` yet.

- [ ] **Step 3: Add the default method**

In `src/adapters/mod.rs`, in the `BrokerAdapter` trait, immediately after `order_status`'s closing brace:

```rust
    /// Previous close per symbol, for day-change display — not part of order
    /// routing or reconciliation. Adapters that don't support it return
    /// `NotConfigured` (the default) and are silently skipped by the poller
    /// that calls this (`daily_stats_poller`); one adapter having no snapshot
    /// endpoint must never stop another's from updating.
    async fn daily_stats(&self, _symbols: &[String]) -> Result<Vec<(String, f64)>, BrokerError> {
        Err(BrokerError::NotConfigured(
            "daily stats not supported by this adapter".to_string(),
        ))
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib adapters::tests::daily_stats_defaults_to_not_configured -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/adapters/mod.rs
git commit -m "feat(adapters): add optional daily_stats method to BrokerAdapter"
```

---

## Task 4: Alpaca `daily_stats`

**Files:**
- Modify: `src/adapters/alpaca.rs`

**Interfaces:**
- Consumes: `BrokerAdapter::daily_stats` signature from Task 3; `AlpacaAdapter`'s existing `client: Client`, `api_key`, `api_secret` fields.
- Produces: a pure `parse_alpaca_snapshots(json: &serde_json::Value) -> Vec<(String, f64)>` function (unit-tested directly, per this repo's existing convention of testing parsing logic, not the network call) and the `daily_stats` override that calls it.

- [ ] **Step 1: Write the failing test**

```rust
// in src/adapters/alpaca.rs's existing `mod tests` block
#[test]
fn parses_prev_close_from_snapshot_response() {
    let body: serde_json::Value = serde_json::from_str(
        r#"{
            "AAPL": {"prevDailyBar": {"c": 227.16}},
            "MSFT": {"prevDailyBar": {"c": 414.10}}
        }"#,
    )
    .unwrap();
    let mut stats = parse_alpaca_snapshots(&body);
    stats.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(stats, vec![("AAPL".to_string(), 227.16), ("MSFT".to_string(), 414.10)]);
}

#[test]
fn a_symbol_missing_prev_daily_bar_is_skipped_not_zeroed() {
    let body: serde_json::Value = serde_json::from_str(
        r#"{"AAPL": {"prevDailyBar": {"c": 227.16}}, "NEWLIST": {}}"#,
    )
    .unwrap();
    let stats = parse_alpaca_snapshots(&body);
    assert_eq!(stats, vec![("AAPL".to_string(), 227.16)]);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib adapters::alpaca::tests::parses_prev_close -- --nocapture`
Expected: FAIL — `parse_alpaca_snapshots` doesn't exist yet.

- [ ] **Step 3: Write the implementation**

Add near the top of `src/adapters/alpaca.rs`, alongside the other free functions (e.g. near `alpaca_exchange_to_mic`):

```rust
/// Alpaca's market-data API is a separate host from the trading API
/// (`api.alpaca.markets`/`paper-api.alpaca.markets`) and is not
/// environment-specific — the same data host serves both live and paper
/// accounts.
const ALPACA_DATA_URL: &str = "https://data.alpaca.markets";

/// Pulls `prevDailyBar.c` (previous close) out of a
/// `GET /v2/stocks/snapshots` response. A symbol with no `prevDailyBar` (a
/// brand-new listing, or one Alpaca hasn't backfilled yet) is skipped, never
/// reported as a zero previous close.
fn parse_alpaca_snapshots(body: &serde_json::Value) -> Vec<(String, f64)> {
    let Some(map) = body.as_object() else { return Vec::new() };
    map.iter()
        .filter_map(|(symbol, snapshot)| {
            let prev_close = snapshot.get("prevDailyBar")?.get("c")?.as_f64()?;
            Some((symbol.clone(), prev_close))
        })
        .collect()
}
```

Then, in the `impl BrokerAdapter for AlpacaAdapter` block, add:

```rust
    async fn daily_stats(&self, symbols: &[String]) -> Result<Vec<(String, f64)>, BrokerError> {
        if symbols.is_empty() {
            return Ok(Vec::new());
        }
        let url = format!(
            "{}/v2/stocks/snapshots?symbols={}",
            ALPACA_DATA_URL,
            symbols.join(",")
        );
        let resp = self
            .client
            .get(&url)
            .header("APCA-API-KEY-ID", &self.api_key)
            .header("APCA-API-SECRET-KEY", &self.api_secret)
            .send()
            .await
            .map_err(|e| BrokerError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(BrokerError::BrokerRejected(format!(
                "snapshot request failed: {}",
                resp.status()
            )));
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| BrokerError::Network(e.to_string()))?;
        Ok(parse_alpaca_snapshots(&body))
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib adapters::alpaca::tests::parses_prev_close --lib adapters::alpaca::tests::a_symbol_missing -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/adapters/alpaca.rs
git commit -m "feat(alpaca): implement daily_stats via the snapshot endpoint"
```

---

## Task 5: Binance `daily_stats`

**Files:**
- Modify: `src/adapters/binance.rs`

**Interfaces:**
- Consumes: `BrokerAdapter::daily_stats` signature from Task 3; `BinanceAdapter`'s existing `client: Client`, `base_url` fields.
- Produces: a pure `parse_binance_24hr_stats(json: &serde_json::Value) -> Vec<(String, f64)>` function and the `daily_stats` override.

- [ ] **Step 1: Write the failing test**

```rust
// in src/adapters/binance.rs's existing `mod tests` block
#[test]
fn parses_prev_close_price_from_24hr_ticker_array() {
    let body: serde_json::Value = serde_json::from_str(
        r#"[
            {"symbol": "BTCUSDT", "prevClosePrice": "64000.50"},
            {"symbol": "ETHUSDT", "prevClosePrice": "3200.10"}
        ]"#,
    )
    .unwrap();
    let mut stats = parse_binance_24hr_stats(&body);
    stats.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        stats,
        vec![("BTCUSDT".to_string(), 64000.50), ("ETHUSDT".to_string(), 3200.10)]
    );
}

#[test]
fn an_entry_with_unparseable_price_is_skipped_not_zeroed() {
    let body: serde_json::Value = serde_json::from_str(
        r#"[{"symbol": "BTCUSDT", "prevClosePrice": "64000.50"}, {"symbol": "BROKEN", "prevClosePrice": "not-a-number"}]"#,
    )
    .unwrap();
    let stats = parse_binance_24hr_stats(&body);
    assert_eq!(stats, vec![("BTCUSDT".to_string(), 64000.50)]);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib adapters::binance::tests::parses_prev_close_price -- --nocapture`
Expected: FAIL — `parse_binance_24hr_stats` doesn't exist yet.

- [ ] **Step 3: Write the implementation**

Add near the other free functions in `src/adapters/binance.rs`:

```rust
/// `GET /api/v3/ticker/24hr?symbols=[...]` returns an array; `prevClosePrice`
/// is a string (Binance quotes all prices as strings to avoid float
/// round-tripping ambiguity). An entry whose price doesn't parse is skipped,
/// never reported as a zero previous close.
fn parse_binance_24hr_stats(body: &serde_json::Value) -> Vec<(String, f64)> {
    let Some(entries) = body.as_array() else { return Vec::new() };
    entries
        .iter()
        .filter_map(|entry| {
            let symbol = entry.get("symbol")?.as_str()?.to_string();
            let price: f64 = entry.get("prevClosePrice")?.as_str()?.parse().ok()?;
            Some((symbol, price))
        })
        .collect()
}
```

Then, in the `impl BrokerAdapter for BinanceAdapter` block, add:

```rust
    async fn daily_stats(&self, symbols: &[String]) -> Result<Vec<(String, f64)>, BrokerError> {
        if symbols.is_empty() {
            return Ok(Vec::new());
        }
        // Public endpoint — no signing needed, same as exchangeInfo.
        let symbols_json = serde_json::to_string(symbols).unwrap_or_default();
        let url = format!(
            "{}/api/v3/ticker/24hr?symbols={}",
            self.base_url,
            urlencoding::encode(&symbols_json)
        );
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| BrokerError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(BrokerError::BrokerRejected(format!(
                "24hr ticker request failed: {}",
                resp.status()
            )));
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| BrokerError::Network(e.to_string()))?;
        Ok(parse_binance_24hr_stats(&body))
    }
```

Check whether `urlencoding` is already a dependency:

Run: `grep -n '^urlencoding' Cargo.toml`

If it's not there, add `symbols_json.replace('"', "%22").replace(',', "%2C").replace('[', "%5B").replace(']', "%5D")` inline instead of the `urlencoding::encode` call, to avoid a new dependency for one call site — check the grep result before choosing.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib adapters::binance::tests::parses_prev_close_price --lib adapters::binance::tests::an_entry_with_unparseable -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/adapters/binance.rs
git commit -m "feat(binance): implement daily_stats via the public 24hr ticker"
```

---

## Task 6: Interesting-set + poller

**Files:**
- Modify: `src/adapters/mod.rs` (add an iterator accessor to `BrokerRegistry`)
- Create: `src/daily_stats_poller.rs`
- Modify: `src/main.rs:64` (add `mod daily_stats_poller;` next to `mod mark_router;`)

**Interfaces:**
- Consumes: `BrokerRegistry` (Task 3's `daily_stats` on every adapter), `DailyStatsStore::set` (Task 2), `PgPool`.
- Produces: `pub fn interesting_instruments(pool: &PgPool) -> ...` (a pure-enough, testable query-building step — see below) and `pub async fn run(pool: PgPool, registry: Arc<ArcSwap<BrokerRegistry>>, stats: DailyStatsStore)`, spawned once in `main.rs` — no other task depends on calling this directly.

- [ ] **Step 1: Add a registry iterator (no test — thin accessor, exercised by Step 6's poller test)**

In `src/adapters/mod.rs`, in `impl BrokerRegistry`, add:

```rust
    /// Every registered (broker_code, environment, adapter) triple. Used by
    /// tasks that want to try something on all of them and skip the ones
    /// that don't support it (`daily_stats_poller`), as opposed to `get`,
    /// which needs to already know which one it wants.
    pub fn iter(&self) -> impl Iterator<Item = (&(String, String), &Arc<dyn BrokerAdapter>)> {
        self.adapters.iter()
    }
```

- [ ] **Step 2: Write the failing test for the interesting-set query**

```rust
// bottom of the new src/daily_stats_poller.rs
#[cfg(test)]
mod tests {
    use super::*;

    // interesting_instrument_ids is a pure function over two id lists — the
    // union, deduplicated. The SQL that PRODUCES those two lists (watchlist
    // rows, held positions) is exercised by Task 8's/Task 1's own DB-backed
    // tests; this only tests the merge logic, which is where a bug ("forgot
    // to dedupe", "used intersection instead of union") would actually hide.
    #[test]
    fn unions_and_dedupes_watchlist_and_held() {
        let watchlist = vec![1i64, 2, 3];
        let held = vec![2i64, 3, 4];
        let mut result = interesting_instrument_ids(&watchlist, &held);
        result.sort();
        assert_eq!(result, vec![1, 2, 3, 4]);
    }

    #[test]
    fn empty_inputs_produce_empty_output() {
        assert_eq!(interesting_instrument_ids(&[], &[]), Vec::<i64>::new());
    }
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test --lib daily_stats_poller:: -- --nocapture`
Expected: FAIL — module doesn't exist yet.

- [ ] **Step 4: Write the implementation**

```rust
// src/daily_stats_poller.rs
//! Periodically refreshes `DailyStatsStore` from every registered broker
//! adapter's own snapshot/24hr-stats endpoint (`BrokerAdapter::daily_stats`),
//! for whatever instruments are "interesting" right now: watched, or held.
//! Independent of any HTTP request — a request for `/marks` never blocks on
//! a broker call.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use sqlx::PgPool;
use tracing::{info, warn};

use crate::adapters::BrokerRegistry;
use crate::daily_stats::DailyStatsStore;

const POLL_INTERVAL_SECS: u64 = 300;

/// The union of two instrument-id lists, deduplicated. A pure merge step,
/// factored out so the "did we dedupe/union correctly" question doesn't need
/// a database to answer.
pub fn interesting_instrument_ids(watchlisted: &[i64], held: &[i64]) -> Vec<i64> {
    let mut set: HashSet<i64> = HashSet::new();
    set.extend(watchlisted.iter().copied());
    set.extend(held.iter().copied());
    set.into_iter().collect()
}

async fn watchlisted_instrument_ids(pool: &PgPool) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar::<_, String>("SELECT DISTINCT instrument_id FROM watchlist_item")
        .fetch_all(pool)
        .await
        .map(|rows| rows.into_iter().filter_map(|s| s.parse().ok()).collect())
}

async fn held_instrument_ids(pool: &PgPool) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT instrument_id FROM position WHERE net_qty <> 0",
    )
    .fetch_all(pool)
    .await
    .map(|rows| rows.into_iter().filter_map(|s| s.parse().ok()).collect())
}

/// instrument.symbol for a set of ids, as `(id, symbol)` — the poller needs
/// symbols to call each adapter with; the stores it writes to are keyed by
/// id (matching `MarkStore`).
async fn symbols_for(pool: &PgPool, ids: &[i64]) -> Result<Vec<(i64, String)>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, (i64, String)>(
        "SELECT id, symbol FROM instrument WHERE id = ANY($1) AND status = 'ACTIVE'",
    )
    .bind(ids)
    .fetch_all(pool)
    .await
}

async fn poll_once(pool: &PgPool, registry: &BrokerRegistry, stats: &DailyStatsStore) {
    let watchlisted = watchlisted_instrument_ids(pool).await.unwrap_or_else(|e| {
        warn!(error = %e, "daily stats: failed to load watchlist instruments");
        Vec::new()
    });
    let held = held_instrument_ids(pool).await.unwrap_or_else(|e| {
        warn!(error = %e, "daily stats: failed to load held instruments");
        Vec::new()
    });
    let ids = interesting_instrument_ids(&watchlisted, &held);
    let by_id = match symbols_for(pool, &ids).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!(error = %e, "daily stats: failed to resolve symbols, skipping this pass");
            return;
        }
    };
    if by_id.is_empty() {
        return;
    }
    let symbol_to_id: std::collections::HashMap<String, i64> =
        by_id.iter().map(|(id, sym)| (sym.clone(), *id)).collect();
    let symbols: Vec<String> = by_id.iter().map(|(_, sym)| sym.clone()).collect();

    for ((broker_code, environment), adapter) in registry.iter() {
        match adapter.daily_stats(&symbols).await {
            Ok(results) => {
                let mut updated = 0;
                for (symbol, prev_close) in results {
                    if let Some(&id) = symbol_to_id.get(&symbol) {
                        stats.set(id, prev_close);
                        updated += 1;
                    }
                }
                if updated > 0 {
                    info!(broker_code, environment, updated, "daily stats: refreshed");
                }
            }
            Err(crate::adapters::BrokerError::NotConfigured(_)) => {
                // This adapter doesn't support it. Not an error — most won't.
            }
            Err(e) => {
                warn!(broker_code, environment, error = %e, "daily stats: adapter call failed, others continue");
            }
        }
    }
}

pub async fn run(pool: PgPool, registry: Arc<ArcSwap<BrokerRegistry>>, stats: DailyStatsStore) {
    let mut interval = tokio::time::interval(Duration::from_secs(POLL_INTERVAL_SECS));
    loop {
        interval.tick().await;
        poll_once(&pool, &registry.load(), &stats).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unions_and_dedupes_watchlist_and_held() {
        let watchlist = vec![1i64, 2, 3];
        let held = vec![2i64, 3, 4];
        let mut result = interesting_instrument_ids(&watchlist, &held);
        result.sort();
        assert_eq!(result, vec![1, 2, 3, 4]);
    }

    #[test]
    fn empty_inputs_produce_empty_output() {
        assert_eq!(interesting_instrument_ids(&[], &[]), Vec::<i64>::new());
    }
}
```

- [ ] **Step 5: Register the module**

In `src/main.rs`, next to the existing `mod mark_router;` (line 64):

```rust
mod daily_stats_poller;
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --lib daily_stats_poller:: -- --nocapture`
Expected: PASS — 2 tests.

- [ ] **Step 7: Run the full test suite to catch any signature mismatch**

Run: `cargo test --lib 2>&1 | tail -40`
Expected: compiles; the crate won't yet build cleanly until Task 7 wires `AppState`/`main.rs` — if this step fails only on the not-yet-spawned `run` function being unused, that's expected and resolved by Task 7, not a regression to fix here.

- [ ] **Step 8: Commit**

```bash
git add src/adapters/mod.rs src/daily_stats_poller.rs src/main.rs
git commit -m "feat(daily-stats): add the interesting-set poller"
```

---

## Task 7: Wire `DailyStatsStore` and the poller into `AppState`/`main.rs`

**Files:**
- Modify: `src/app_state.rs` (field + accessor, mirroring `marks`)
- Modify: `src/main.rs` (spawn the poller near the existing `mark_router::run` spawn)

**Interfaces:**
- Consumes: `DailyStatsStore` (Task 2), `daily_stats_poller::run` (Task 6).
- Produces: `AppState::daily_stats(&self) -> &DailyStatsStore` — Task 8's handlers call this.

- [ ] **Step 1: Add the field**

In `src/app_state.rs`, next to the existing `marks: MarkStore` field (around line 136):

```rust
    daily_stats: crate::daily_stats::DailyStatsStore,
```

- [ ] **Step 2: Initialize it in the constructor**

In `AppState::new`, next to `marks: MarkStore::new(),` (around line 181):

```rust
            daily_stats: crate::daily_stats::DailyStatsStore::new(),
```

- [ ] **Step 3: Add the accessor**

Next to the existing `pub fn marks(&self) -> &MarkStore { &self.marks }` (around line 223):

```rust
    pub fn daily_stats(&self) -> &crate::daily_stats::DailyStatsStore { &self.daily_stats }
```

- [ ] **Step 4: Spawn the poller**

In `src/main.rs`, immediately after the existing `tokio::spawn(mark_router::run(...))` line (around line 1041):

```rust
    tokio::spawn(daily_stats_poller::run(
        state.pool().clone(),
        state.registry_handle(), // see Step 5 — registry() returns a Guard, not the Arc itself
        state.daily_stats().clone(),
    ));
```

- [ ] **Step 5: Expose the underlying `Arc<ArcSwap<BrokerRegistry>>`**

`AppState::registry()` returns an `arc_swap::Guard`, which doesn't outlive the call and can't be held across `.await` points inside a long-running task — the poller needs the `Arc<ArcSwap<BrokerRegistry>>` itself so it can call `.load()` fresh every poll. Add, next to `registry()` in `src/app_state.rs`:

```rust
    /// The registry's own swappable handle, for a long-running task
    /// (`daily_stats_poller::run`) that needs to `.load()` fresh on every
    /// iteration rather than holding one `Guard` for its whole lifetime.
    pub fn registry_handle(&self) -> Arc<ArcSwap<BrokerRegistry>> {
        self.registry.clone()
    }
```

- [ ] **Step 6: Build**

Run: `cargo build 2>&1 | tail -40`
Expected: compiles cleanly.

- [ ] **Step 7: Run the full test suite**

Run: `cargo test --lib 2>&1 | tail -20`
Expected: all pass, including Task 2/3/4/5/6's new tests.

- [ ] **Step 8: Commit**

```bash
git add src/app_state.rs src/main.rs
git commit -m "feat(daily-stats): wire DailyStatsStore and the poller into AppState"
```

---

## Task 8: `/watchlist` CRUD and `/marks` endpoint

**Files:**
- Modify: `src/handlers.rs` (new handlers)
- Modify: `src/main.rs` (route registration, in the `orders_router` chain around line 1239)

**Interfaces:**
- Consumes: `Extension<AuthContext>` (existing, `auth.principal_id: Uuid`), `state.marks()` (existing), `state.daily_stats()` (Task 7), `watchlist_item` table (Task 1).
- Produces: `GET /watchlist`, `POST /watchlist`, `DELETE /watchlist/:instrument_id`, `GET /marks?instrument_ids=1,2,3` — Task 9's `Watchlist.tsx` and Task 10's `Positions.tsx` call these.

- [ ] **Step 1: Write the failing tests**

Add to `src/handlers.rs`'s existing `mod tests` block (it already has `test_pool()` and `seed_principal(pool, code) -> (Uuid, String)` — reuse both exactly; don't invent new seeding helpers). Instrument rows aren't pre-seeded, so insert one directly the same way the existing actor-attribution test does (`SELECT`s a seeded `venue`/`currency` code, then `INSERT INTO instrument ... RETURNING id`):

```rust
async fn seed_instrument(pool: &sqlx::PgPool, symbol_suffix: &str) -> i64 {
    let venue: String = sqlx::query_scalar("SELECT code FROM venue ORDER BY code LIMIT 1")
        .fetch_one(pool)
        .await
        .expect("a seeded venue");
    let currency: String = sqlx::query_scalar("SELECT code FROM currency ORDER BY code LIMIT 1")
        .fetch_one(pool)
        .await
        .expect("a seeded currency");
    sqlx::query_scalar(
        "INSERT INTO instrument \
             (symbol, venue, name, asset_class, instrument_class, currency, status, \
              price_precision, price_increment) \
         VALUES ($1, $2, 'Watchlist test instrument', 'EQUITY', 'SPOT', $3, 'ACTIVE', 2, 0.01) \
         RETURNING id",
    )
    .bind(format!("WATCH{symbol_suffix}"))
    .bind(&venue)
    .bind(&currency)
    .fetch_one(pool)
    .await
    .expect("seed instrument")
}

/// Mirrors the existing actor-attribution test's `AppState::new(...)` call
/// exactly (src/handlers.rs, the test just above this one) — empty registry,
/// no Kafka, a throwaway quote channel. Nothing in these tests routes an
/// order or reads the registry, so an empty one is correct, not a stub.
fn test_app_state(pool: sqlx::PgPool) -> AppState {
    let (quote_tx, _quote_rx) = tokio::sync::mpsc::channel(1);
    AppState::new(
        pool,
        "test-admin-token".to_string(),
        false,
        BrokerRegistry::new(),
        None,
        Identifier::new(OpenFigiClient::new(None), InMemoryCache::new()),
        StreamHealthRegistry::new(),
        None,
        quote_tx,
        crate::sessions::SessionConfig {
            cookie_policy: crate::sessions::cookie_policy("localhost:3001", None),
            ttl: crate::sessions::SessionTtl::default(),
            public_base_url: None,
        },
    )
}

#[tokio::test]
async fn watchlist_add_list_remove_round_trips() {
    let pool = test_pool().await;
    let (principal_id, principal_code) = seed_principal(&pool, "watch-roundtrip").await;
    let instrument_id = seed_instrument(&pool, "RT").await.to_string();
    let state = test_app_state(pool);
    let auth = AuthContext { principal_id, principal_code };

    add_watchlist_item(
        State(state.clone()),
        Extension(auth.clone()),
        Json(AddWatchlistItem { instrument_id: instrument_id.clone() }),
    )
    .await
    .expect("add should succeed");

    let listed = list_watchlist(State(state.clone()), Extension(auth.clone()))
        .await
        .expect("list should succeed")
        .0;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].instrument_id, instrument_id);

    remove_watchlist_item(State(state.clone()), Extension(auth.clone()), Path(instrument_id.clone()))
        .await
        .expect("remove should succeed");

    let listed = list_watchlist(State(state), Extension(auth)).await.expect("list should succeed").0;
    assert!(listed.is_empty());
}

#[tokio::test]
async fn adding_a_nonexistent_instrument_is_404() {
    let pool = test_pool().await;
    let (principal_id, principal_code) = seed_principal(&pool, "watch-404").await;
    let state = test_app_state(pool);
    let auth = AuthContext { principal_id, principal_code };

    let result = add_watchlist_item(
        State(state),
        Extension(auth),
        Json(AddWatchlistItem { instrument_id: "999999999".to_string() }),
    )
    .await;

    assert!(matches!(result, Err(ApiError { status: StatusCode::NOT_FOUND, .. })));
}

#[tokio::test]
async fn adding_the_same_instrument_twice_is_idempotent_not_an_error() {
    let pool = test_pool().await;
    let (principal_id, principal_code) = seed_principal(&pool, "watch-dup").await;
    let instrument_id = seed_instrument(&pool, "DUP").await.to_string();
    let state = test_app_state(pool);
    let auth = AuthContext { principal_id, principal_code };

    add_watchlist_item(State(state.clone()), Extension(auth.clone()), Json(AddWatchlistItem { instrument_id: instrument_id.clone() }))
        .await
        .expect("first add succeeds");
    add_watchlist_item(State(state), Extension(auth), Json(AddWatchlistItem { instrument_id }))
        .await
        .expect("second add is idempotent, not an error");
}

#[tokio::test]
async fn one_principal_cannot_see_or_remove_anothers_watchlist_item() {
    let pool = test_pool().await;
    let (principal_a, code_a) = seed_principal(&pool, "watch-a").await;
    let (principal_b, code_b) = seed_principal(&pool, "watch-b").await;
    let instrument_id = seed_instrument(&pool, "XOWN").await.to_string();
    let state = test_app_state(pool);

    add_watchlist_item(
        State(state.clone()),
        Extension(AuthContext { principal_id: principal_a, principal_code: code_a.clone() }),
        Json(AddWatchlistItem { instrument_id: instrument_id.clone() }),
    )
    .await
    .expect("a adds");

    let b_list = list_watchlist(
        State(state.clone()),
        Extension(AuthContext { principal_id: principal_b, principal_code: code_b.clone() }),
    )
    .await
    .expect("list succeeds")
    .0;
    assert!(b_list.is_empty(), "b must not see a's watchlist item");

    // b's delete of an item that exists (for a, not b) must not error — it's
    // scoped to b's own rows, so it's a no-op, and a's row survives.
    remove_watchlist_item(
        State(state.clone()),
        Extension(AuthContext { principal_id: principal_b, principal_code: code_b }),
        Path(instrument_id.clone()),
    )
    .await
    .expect("no-op delete still succeeds");
    let a_list = list_watchlist(
        State(state),
        Extension(AuthContext { principal_id: principal_a, principal_code: code_a }),
    )
    .await
    .expect("list succeeds")
    .0;
    assert_eq!(a_list.len(), 1, "a's item must survive b's no-op delete");
}

#[tokio::test]
async fn marks_returns_null_fields_for_unpriced_instruments_not_zero() {
    let pool = test_pool().await;
    let instrument_id = seed_instrument(&pool, "UNPRICED").await;
    let state = test_app_state(pool);
    // Deliberately: no MarkStore.set, no DailyStatsStore.set for this id.

    let result = get_marks(State(state), Query(MarksQuery { instrument_ids: instrument_id.to_string() }))
        .await
        .expect("should succeed even with nothing priced")
        .0;

    assert_eq!(result.len(), 1);
    assert!(result[0].bid.is_none());
    assert!(result[0].prev_close.is_none());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib handlers::tests::watchlist -- --nocapture` and `cargo test --lib handlers::tests::marks -- --nocapture`
Expected: FAIL — none of these functions/types exist yet.

- [ ] **Step 3: Write the implementation**

Add to `src/handlers.rs`:

```rust
#[derive(serde::Serialize)]
pub struct WatchlistRow {
    pub instrument_id: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(serde::Deserialize)]
pub struct AddWatchlistItem {
    pub instrument_id: String,
}

pub async fn list_watchlist(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
) -> Result<Json<Vec<WatchlistRow>>, ApiError> {
    let rows = sqlx::query_as::<_, (String, chrono::DateTime<chrono::Utc>)>(
        "SELECT instrument_id, created_at FROM watchlist_item \
         WHERE principal_id = $1 ORDER BY created_at",
    )
    .bind(auth.principal_id)
    .fetch_all(state.pool())
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to list watchlist: {err:?}"),
    })?;
    Ok(Json(
        rows.into_iter()
            .map(|(instrument_id, created_at)| WatchlistRow { instrument_id, created_at })
            .collect(),
    ))
}

pub async fn add_watchlist_item(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<AddWatchlistItem>,
) -> Result<StatusCode, ApiError> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM instrument WHERE id::text = $1 AND status = 'ACTIVE')",
    )
    .bind(&body.instrument_id)
    .fetch_one(state.pool())
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to check instrument: {err:?}"),
    })?;
    if !exists {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: format!("no active instrument {}", body.instrument_id),
        });
    }

    sqlx::query(
        "INSERT INTO watchlist_item (principal_id, instrument_id) VALUES ($1, $2) \
         ON CONFLICT (principal_id, instrument_id) DO NOTHING",
    )
    .bind(auth.principal_id)
    .bind(&body.instrument_id)
    .execute(state.pool())
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to add watchlist item: {err:?}"),
    })?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn remove_watchlist_item(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(instrument_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    sqlx::query("DELETE FROM watchlist_item WHERE principal_id = $1 AND instrument_id = $2")
        .bind(auth.principal_id)
        .bind(&instrument_id)
        .execute(state.pool())
        .await
        .map_err(|err| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("failed to remove watchlist item: {err:?}"),
        })?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(serde::Deserialize)]
pub struct MarksQuery {
    pub instrument_ids: String, // comma-separated, e.g. "1,2,3"
}

#[derive(serde::Serialize)]
pub struct MarkRow {
    pub instrument_id: i64,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub mid: Option<f64>,
    pub prev_close: Option<f64>,
    pub pct_change: Option<f64>,
}

pub async fn get_marks(
    State(state): State<AppState>,
    Query(query): Query<MarksQuery>,
) -> Result<Json<Vec<MarkRow>>, ApiError> {
    let ids: Vec<i64> = query
        .instrument_ids
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    let rows = ids
        .into_iter()
        .map(|instrument_id| {
            let mark = state.marks().get(instrument_id);
            let daily = state.daily_stats().get(instrument_id);
            let mid = mark.map(|m| m.mid());
            let pct_change = match (mid, daily) {
                (Some(mid), Some(d)) if d.prev_close != 0.0 => {
                    Some((mid - d.prev_close) / d.prev_close * 100.0)
                }
                _ => None,
            };
            MarkRow {
                instrument_id,
                bid: mark.map(|m| m.bid),
                ask: mark.map(|m| m.ask),
                mid,
                prev_close: daily.map(|d| d.prev_close),
                pct_change,
            }
        })
        .collect();
    Ok(Json(rows))
}
```

- [ ] **Step 4: Register the routes**

In `src/main.rs`, in the `orders_router` chain, next to the existing `.route("/portfolios", ...)` line:

```rust
        .route(
            "/watchlist",
            get(handlers::list_watchlist).post(handlers::add_watchlist_item),
        )
        .route("/watchlist/:instrument_id", delete(handlers::remove_watchlist_item))
        .route("/marks", get(handlers::get_marks))
```

`main.rs`'s `use axum::{ ... }` block (line 38) currently imports only `routing::get` and `routing::post` — add `routing::delete`:

```rust
use axum::{
    response::Html,
    routing::get,
    routing::post,
    routing::delete,
    middleware,
    Extension,
    Router
};
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib handlers::tests::watchlist -- --nocapture` and `cargo test --lib handlers::tests::marks -- --nocapture`
Expected: PASS.

- [ ] **Step 6: Run the full test suite**

Run: `cargo test --lib 2>&1 | tail -20`
Expected: all pass.

- [ ] **Step 7: Commit**

```bash
git add src/handlers.rs src/main.rs
git commit -m "feat(trade-api): add /watchlist CRUD and /marks endpoint"
```

---

## Task 9: `Watchlist.tsx` and wiring into `TradePage`

**Files:**
- Create: `cockpit/src/trade/types.ts` (shared `MarkRow` type, since both this task and Task 10 need it)
- Create: `cockpit/src/trade/components/Watchlist.tsx`
- Modify: `cockpit/src/trade/pages/Trade.tsx`

**Interfaces:**
- Consumes: `tradeApi` (`cockpit/src/trade/api/client.ts`), `InstrumentSelect` (`cockpit/src/components/InstrumentSelect.tsx` — props: `value`, `onChange`, `onSelected`, `basePath`, `apiGet`, matching `OrderTicket.tsx`'s existing usage), `GET /watchlist`, `POST /watchlist`, `DELETE /watchlist/:id`, `GET /marks` (Task 8).
- Produces: `<Watchlist onSelectInstrument={(id: string) => void} />`, mounted in `TradePage` — Task 10 does NOT depend on this file directly (it imports `MarkRow` from `types.ts`, not from this component).

- [ ] **Step 1: Add the shared type**

```typescript
// cockpit/src/trade/types.ts
// Mirrors src/handlers.rs's MarkRow. A field is null when that half of the
// data (live mark, or previous close) hasn't been populated for this
// instrument yet — never a fabricated 0, so callers must not `?? 0` these.
export interface MarkRow {
  instrument_id: number;
  bid: number | null;
  ask: number | null;
  mid: number | null;
  prev_close: number | null;
  pct_change: number | null;
}
```

- [ ] **Step 2: Write `Watchlist.tsx`**

```typescript
// cockpit/src/trade/components/Watchlist.tsx
import { useState } from "react";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { ActionIcon, Group, Paper, Stack, Text, Title } from "@mantine/core";
import { IconX, IconPlus } from "@tabler/icons-react";
import { tradeApi } from "../api/client";
import { InstrumentSelect, type Instrument } from "../../components/InstrumentSelect";
import type { MarkRow } from "../types";

interface WatchlistRow {
  instrument_id: string;
  created_at: string;
}

// Live watchlist: a trader's own list of symbols, ticking price + day
// %-change. Polls /marks on the same ~3s cadence TradeBlotter already uses
// for orders — no new transport, just another poll.
export function Watchlist({ onSelectInstrument }: { onSelectInstrument: (instrumentId: string) => void }) {
  const queryClient = useQueryClient();
  const [adding, setAdding] = useState(false);
  const [pendingInstrument, setPendingInstrument] = useState<string | null>(null);

  const watchlist = useQuery<WatchlistRow[]>({
    queryKey: ["/watchlist"],
    queryFn: () => tradeApi.get<WatchlistRow[]>("/watchlist"),
  });

  const instrumentIds = (watchlist.data ?? []).map((w) => w.instrument_id);
  const marks = useQuery<MarkRow[]>({
    queryKey: ["/marks", instrumentIds],
    queryFn: () => tradeApi.get<MarkRow[]>(`/marks?instrument_ids=${instrumentIds.join(",")}`),
    enabled: instrumentIds.length > 0,
    refetchInterval: 3000,
  });
  const markByInstrument = new Map((marks.data ?? []).map((m) => [String(m.instrument_id), m]));

  const addMutation = useMutation({
    mutationFn: (instrumentId: string) => tradeApi.post("/watchlist", { instrument_id: instrumentId }),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["/watchlist"] });
      setAdding(false);
      setPendingInstrument(null);
    },
  });

  const removeMutation = useMutation({
    mutationFn: (instrumentId: string) => tradeApi.del(`/watchlist/${instrumentId}`),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["/watchlist"] }),
  });

  return (
    <Paper withBorder p="md">
      <Group justify="space-between" mb="xs">
        <Title order={4}>Watchlist</Title>
        <ActionIcon variant="subtle" onClick={() => setAdding((v) => !v)} aria-label="Add symbol">
          <IconPlus size={16} />
        </ActionIcon>
      </Group>

      {adding && (
        <InstrumentSelect
          value={pendingInstrument}
          onChange={setPendingInstrument}
          onSelected={(instrument: Instrument | null) => {
            if (instrument) addMutation.mutate(String(instrument.id));
          }}
          placeholder="Search symbol to add"
          basePath="/instruments"
          apiGet={tradeApi.get}
        />
      )}

      <Stack gap={4} mt="xs">
        {(watchlist.data ?? []).map((row) => {
          const mark = markByInstrument.get(row.instrument_id);
          const pct = mark?.pct_change ?? null;
          return (
            <Group
              key={row.instrument_id}
              justify="space-between"
              wrap="nowrap"
              style={{ cursor: "pointer" }}
              onClick={() => onSelectInstrument(row.instrument_id)}
            >
              <Text size="sm">{row.instrument_id}</Text>
              <Group gap="xs" wrap="nowrap">
                <Text size="sm" ff="monospace">
                  {mark?.mid !== null && mark?.mid !== undefined ? mark.mid.toFixed(2) : "—"}
                </Text>
                <Text size="xs" c={pct === null ? "dimmed" : pct >= 0 ? "depth" : "offer"}>
                  {pct === null ? "" : `${pct >= 0 ? "+" : ""}${pct.toFixed(2)}%`}
                </Text>
                <ActionIcon
                  size="xs"
                  variant="subtle"
                  aria-label="Remove from watchlist"
                  onClick={(e) => {
                    e.stopPropagation();
                    removeMutation.mutate(row.instrument_id);
                  }}
                >
                  <IconX size={12} />
                </ActionIcon>
              </Group>
            </Group>
          );
        })}
        {watchlist.data?.length === 0 && (
          <Text size="sm" c="dimmed">
            No symbols yet — add one above.
          </Text>
        )}
      </Stack>
    </Paper>
  );
}
```

Check `@tabler/icons-react` is already a dependency (`grep -n "tabler" cockpit/package.json`) — if not, use plain `"×"`/`"+"` text in a `Button variant="subtle"` instead of `ActionIcon`+icons, to avoid adding a new dependency for this task.

- [ ] **Step 3: Wire it into `TradePage`**

In `cockpit/src/trade/pages/Trade.tsx`, add a new column and pass a callback that sets `OrderTicket`'s instrument. `OrderTicket` currently manages its own `instrumentId` state internally with no way to set it from outside — add an optional controlled-ish prop:

In `cockpit/src/trade/components/OrderTicket.tsx`, change the `instrumentId` line from:

```typescript
  const [instrumentId, setInstrumentId] = useState<string | null>(null);
```

to accept an optional external setter that `TradePage` can call, by lifting just enough: add a prop `selectedInstrumentId?: string | null` and a `useEffect` that adopts it:

```typescript
export function OrderTicket({
  portfolios,
  onSubmitted,
  selectedInstrumentId,
}: {
  portfolios: GrantedPortfolio[];
  onSubmitted: (orderId: string) => void;
  selectedInstrumentId?: string | null;
}) {
  // ...existing state...
  const [instrumentId, setInstrumentId] = useState<string | null>(null);

  useEffect(() => {
    if (selectedInstrumentId) setInstrumentId(selectedInstrumentId);
  }, [selectedInstrumentId]);
```

Add `import { useEffect } from "react";` to `OrderTicket.tsx`'s existing `import { useState } from "react";` line.

Then in `Trade.tsx`:

```typescript
import { Watchlist } from "../components/Watchlist";

export function TradePage({ me }: { me: Me }) {
  const queryClient = useQueryClient();
  const [followOrderId, setFollowOrderId] = useState<string | null>(null);
  const [activeTab, setActiveTab] = useState<string | null>("orders");
  const [selectedInstrumentId, setSelectedInstrumentId] = useState<string | null>(null);

  return (
    <Grid>
      <Grid.Col span={{ base: 12, md: 3 }}>
        <Watchlist onSelectInstrument={setSelectedInstrumentId} />
      </Grid.Col>
      <Grid.Col span={{ base: 12, md: 3 }}>
        <OrderTicket
          portfolios={me.portfolios}
          selectedInstrumentId={selectedInstrumentId}
          onSubmitted={(orderId) => {
            setFollowOrderId(orderId);
            setActiveTab("orders");
            queryClient.invalidateQueries({ queryKey: ["/orders"] });
          }}
        />
      </Grid.Col>
      <Grid.Col span={{ base: 12, md: 6 }}>
        {/* ...existing Tabs block, unchanged... */}
      </Grid.Col>
    </Grid>
  );
}
```

- [ ] **Step 4: Type-check**

Run: `cd cockpit && npx tsc --noEmit`
Expected: no errors.

- [ ] **Step 5: Manual verification in the browser**

Start the dev stack (`./scripts/dev-trade.sh` from the repo root), sign in, and confirm: add a symbol via the watchlist's search, see it appear in the list, click it and confirm the order ticket's instrument field updates, remove it via the × button and confirm it disappears. This app has no frontend test suite (checked in Task 8's constraints) — this manual pass is the actual verification for this task.

- [ ] **Step 6: Commit**

```bash
git add cockpit/src/trade/types.ts cockpit/src/trade/components/Watchlist.tsx cockpit/src/trade/pages/Trade.tsx cockpit/src/trade/components/OrderTicket.tsx
git commit -m "feat(trade): add live watchlist, wired to select the order ticket's instrument"
```

---

## Task 10: Day P&L and Net Exposure on `PositionsPage`

**Files:**
- Modify: `cockpit/src/trade/pages/Positions.tsx`

**Interfaces:**
- Consumes: `MarkRow` type (Task 9's `cockpit/src/trade/types.ts`), `GET /marks` (Task 8), existing `PositionRow[]` data already fetched by this page.

- [ ] **Step 1: Add the `/marks` query and both computed cards**

In `cockpit/src/trade/pages/Positions.tsx`, add the import:

```typescript
import type { MarkRow } from "../types";
```

After the existing `positions` query, add:

```typescript
  const heldInstrumentIds = rows.map((p) => p.instrument_id);
  const marks = useQuery<MarkRow[]>({
    queryKey: ["/marks", heldInstrumentIds],
    queryFn: () => tradeApi.get<MarkRow[]>(`/marks?instrument_ids=${heldInstrumentIds.join(",")}`),
    enabled: heldInstrumentIds.length > 0,
    refetchInterval: 3000,
  });
  const markByInstrument = new Map((marks.data ?? []).map((m) => [String(m.instrument_id), m]));

  // Day P&L: qty * (mark - prev_close), summed only over positions where
  // BOTH a live mark and a previous close exist — same "excluded, not
  // zeroed" rule this file already applies to unrealized P&L above.
  const dayPnlRows = rows.filter((p) => {
    const mark = markByInstrument.get(p.instrument_id);
    return mark?.mid != null && mark?.prev_close != null;
  });
  const dayPnlTotal = dayPnlRows.reduce((sum, p) => {
    const mark = markByInstrument.get(p.instrument_id)!;
    return sum + Number(p.net_qty) * (mark.mid! - mark.prev_close!);
  }, 0);
  const dayPnlUnpriced = rows.length - dayPnlRows.length;

  // Net exposure: sum of |market value| — pure computation over data this
  // page already fetched, no new store involved. Only defined over
  // positions that already have a market_value (same exclusion as the
  // existing marketValueTotal above).
  const netExposure = rows
    .filter((p) => p.market_value !== null)
    .reduce((sum, p) => sum + Math.abs(p.market_value as number), 0);
```

Then add the two cards to the existing `SimpleGrid`:

```typescript
          <SimpleGrid cols={{ base: 1, sm: 5 }}>
            <SummaryCard label="Unrealized P&L" value={unrealizedTotal.toFixed(2)} pnlColor />
            <SummaryCard label="Realized P&L" value={realizedTotal.toFixed(2)} pnlColor />
            <SummaryCard label="Market value" value={marketValueTotal.toFixed(2)} />
            <SummaryCard label="Day P&L" value={dayPnlTotal.toFixed(2)} pnlColor />
            <SummaryCard label="Net exposure" value={netExposure.toFixed(2)} />
          </SimpleGrid>
```

Update the existing "N positions have no live mark" caveat text to also account for `dayPnlUnpriced`, or add a second line — check the existing caveat block (`{unpriced > 0 && (...)}`) and add, immediately after it:

```typescript
          {dayPnlUnpriced > 0 && (
            <Text size="xs" c="dimmed">
              {dayPnlUnpriced} of {rows.length} position{rows.length === 1 ? "" : "s"} {dayPnlUnpriced === 1 ? "is" : "are"} missing
              a live mark or a previous close and {dayPnlUnpriced === 1 ? "is" : "are"} excluded from Day P&L.
            </Text>
          )}
```

- [ ] **Step 2: Type-check**

Run: `cd cockpit && npx tsc --noEmit`
Expected: no errors.

- [ ] **Step 3: Manual verification in the browser**

With at least one held position whose instrument has both a live mark (e.g. a Binance-paper crypto position, since that's the only feed with real live marks in this deployment per the Review Focus note) and a `prev_close` from the poller, confirm Day P&L and Net Exposure show real, non-zero, correctly-signed numbers. Confirm a position with neither is excluded (not shown as `$0.00`) and the caveat text appears.

- [ ] **Step 4: Commit**

```bash
git add cockpit/src/trade/pages/Positions.tsx
git commit -m "feat(trade): add Day P&L and Net Exposure to Positions & P&L"
```

---

## Self-Review

**Spec coverage:**
- Live watchlist pricing → Tasks 2, 3, 4, 5, 6, 7, 8, 9.
- Day P&L / Net Exposure → Task 10.
- "Standardised setup, extend the adapters" → Task 3's default-method idiom + Tasks 4/5's per-adapter implementations, exactly mirroring the existing `open_orders`/`order_status` pattern.
- Non-goals (chart, order book, stop orders, buying power, etc.) → none built; not referenced by any task.
- Empty-watchlist default → Task 9 has no seeding logic; confirmed by its own manual-verification step.

**Placeholder scan:** none — every step has real code, no TODOs.

**Type consistency:** `MarkRow` (Rust, `src/handlers.rs`) and `MarkRow` (TypeScript, `cockpit/src/trade/types.ts`) field names match exactly (`instrument_id`, `bid`, `ask`, `mid`, `prev_close`, `pct_change`). `DailyStatsStore`/`DailyStat` used identically across Tasks 2, 6, 7, 8. `BrokerAdapter::daily_stats`'s signature (`&[String] -> Result<Vec<(String, f64)>, BrokerError>`) is identical in Tasks 3, 4, 5, 6.

**Review Focus:** all five items each have an owning task's test named above (unpriced equity → Task 8's `marks_returns_null_fields...`; independent stores → Task 8's same test plus Task 10's exclusion logic; duplicate add → Task 8's `adding_the_same_instrument_twice...`; cross-principal access → Task 8's `one_principal_cannot_see_or_remove...`; one-adapter-failing → Task 6's `poll_once`, which continues its loop on any single adapter's `Err`).
