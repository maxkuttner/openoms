# Market Data Gateway (Phase 6) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prices travel only through a market data gateway. The order side of the gateway (adapters, the broker registry, execution streams) carries no prices. The mark router becomes the pricing service, and the market data gateway is its only feeder.

**Architecture:**
- **New module `gateway::md`.** It holds three things:
  - the price feeds: each connector's public feed, plus the Databento OPRA vendor feed;
  - a subscription interface the core drives (`MdHandle::subscribe` / `unsubscribe`, with plain-data `InstrumentRef`s);
  - the daily-stats (previous close) sources, split off the order adapter.
- **What stays in the OMS:**
  - working out *what* is held, from the database;
  - the mark router, which reads the only quote receiver, owned by the gateway;
  - the daily-stats poller, which now asks a `DailyStatsRegistry` instead of the broker registry.
- **What the gateway owns:**
  - symbology: which instruments a feed covers, and the venue symbol for each;
  - session handling: wait, subscribe, add mid-session, reconnect.

**Tech Stack:** Rust 2021, tokio (`watch`, `mpsc`), `dataprovider` traits, the `databento` crate.

**Spec:** `docs/superpowers/specs/2026-11-04-target-architecture-design.md`, on branch `docs/target-architecture`. This plan covers §4.2 (market data gateway), §4.3 (core → market data gateway commands, gateway → core `Quote`) and §7 phase 6.

## Global Constraints

- **Done when:** "Order gateway carries no prices". Phase 6 row: "Price sides of the crypto connectors and OPRA run under the market data gateway; the mark router becomes the pricing service fed only from it".
- **Gateway constraints:** no DB, no Kafka. "Symbology (instrument ⇄ venue symbol) lives in the gateways, not the core." `crates/gateway` keeps no `sqlx` / `rdkafka` / `axum`, and `boundary_tests::gateway_depends_on_no_database_or_kafka` stays green.
- **Contract:** commands from the core to the market data gateway are `Subscribe{instrument_ids}` and `Unsubscribe{instrument_ids}`; the event to the core is `Quote{instrument_id, bid, ask, bid_size, ask_size, ts_recv, source}` (today's `dataprovider::Quote`, unchanged). In-process, commands may be direct calls (spec §8).
- **Unchanged:** these names must not change:
  - feed supervisor labels `BINANCE/SPOT`, `BYBIT/SPOT`, `COINBASE/SPOT`, `OKX/SPOT`, `DATABENTO/OPRA`;
  - health handles `(code, "SPOT")` and `("DATABENTO", "OPRA")`;
  - the `StreamRegistry` key `"databento-opra"`;
  - `provider_feed_policy` ranks and the mark router's failover rules;
  - the daily-stats rules: one environment per broker, the manifest's `QuoteScope`, and the 5-minute poll.
- **Single user, pre-release:** no compatibility shims. No help text in the UI. DB tests stay `#[ignore]`. Never create databases on the shared dev Postgres. Never run the credential rotate DB test. Never touch `oms-postgres` / `oms-pgdata`.
- **Git:**
  - Never commit `.gitignore`, `.ignore` or `docs/superpowers/plans/2026-09-28-options-search-chain-strategy.md`.
  - Stage files explicitly; never use `git commit -a`. A heredoc and `git commit` never go in one Bash call.
  - Branch: `feat/md-gateway`, cut from `feat/gateway-crate` (`f40e736`).
- **Baseline:** `cargo test --workspace` summed over all result lines gives 695 passed and 80 ignored. Ledger the count after each task.

## Review Focus

1. **A position opened after boot gets priced without a reconnect.** The new instrument reaches the running session as a `SymbolAdds`, as `watch_held` does today. Test: `an_instrument_held_mid_session_is_added` (Task 2).
2. **Nothing held at boot.** The feed idles quietly: no session, no reconnect loop, no health flapping. It starts the moment something it covers is held. Test: `a_feed_waits_until_something_it_covers_is_held` (Task 2).
3. **A position closed.** After the next reconnect, its instrument is no longer subscribed (today's reconnect re-reads the held set). Test: `an_unsubscribed_instrument_is_not_resubscribed_on_reconnect` (Task 2).
4. **A connection deleted or disabled at runtime.** Its daily-stats source disappears in the same reload that removes its order adapter, so the poller stops using the old key. Test: `swapping_registries_replaces_daily_stats_too` (Task 1).
5. **An OPRA credential changed at runtime.** OPRA restarts under the same label and registry key and keeps feeding the one mark router. Test: `opra_keeps_its_names` (Task 3). The boot check in Task 3 covers the rest.

---

## File Structure (end state)

```
crates/dataprovider/src/quote.rs   + InstrumentRef, covers()          (from src/preflight.rs:245-277)
crates/gateway/src/md/mod.rs        MarketDataGateway, MdHandle
crates/gateway/src/md/session.rs    FeedSession: the gateway-side half of today's src/quote_feed.rs
crates/gateway/src/md/daily_stats.rs DailyStats trait, DailyStatsRegistry
crates/gateway/src/md/opra.rs       DatabentoOpraFeed (from src/opra_stream.rs), LABEL, STREAM_CODE
src/market_data.rs (OMS)            held-set loader + subscription loop (replaces src/quote_feed.rs)
src/opra_stream.rs                  deleted
```

---

### Task 1: Daily stats leave the order adapter

**Files:**
- Create: `crates/gateway/src/md/mod.rs` (`pub mod daily_stats;` for now), `crates/gateway/src/md/daily_stats.rs`.
- Modify:
  - `crates/gateway/src/lib.rs`: `pub mod md;`.
  - `crates/gateway/src/adapter.rs:150-160,240-255`: remove `BrokerAdapter::daily_stats` and its test.
  - The five `crates/gateway/src/connectors/{alpaca,binance,bybit,coinbase,okx}/adapter.rs`: each `daily_stats` body moves from `impl BrokerAdapter` to `impl DailyStats`.
  - `crates/gateway/src/connectors/mod.rs`: `Connected` gains a field.
  - Each connector's `connect`.
  - `src/reload.rs`: `build_registry`, `RegistrationOutput`.
  - `src/app_state.rs`.
  - `src/admin.rs:1365`.
  - `src/main.rs:1137,1195`.
  - `src/daily_stats_poller.rs`.
  - The test call sites of `swap_registry`.
  - `src/boundary_tests.rs`.

**Interfaces:**
- Produces, in `gateway::md::daily_stats`:

```rust
/// A connection's previous-close source: the price side of a connector that
/// shares the order side's credential.
#[async_trait::async_trait]
pub trait DailyStats: Send + Sync {
    /// (symbol, previous close) for the symbols it knows; others are left out.
    async fn daily_stats(&self, symbols: &[String]) -> Result<Vec<(String, f64)>, BrokerError>;
}

#[derive(Default, Clone)]
pub struct DailyStatsRegistry { /* HashMap<(String, String), Arc<dyn DailyStats>> */ }
impl DailyStatsRegistry {
    pub fn new() -> Self;
    pub fn register(&mut self, broker_code: &str, environment: &str, source: Arc<dyn DailyStats>);
    pub fn get(&self, broker_code: &str, environment: &str) -> Option<Arc<dyn DailyStats>>;
    pub fn iter(&self) -> impl Iterator<Item = (&(String, String), &Arc<dyn DailyStats>)>;
}
```

- **Connectors:** `Connected { adapter, exec_stream, daily_stats: Option<Arc<dyn DailyStats>> }`. The REST connectors (Alpaca, Binance REST, Bybit, Coinbase, OKX) return the same `Arc` they register as the adapter, coerced to `dyn DailyStats`. FIX connections (IBKR, Binance FIX) return `None`, as today: the FIX adapter has no `daily_stats`.
- **Registration:** `RegistrationOutput` gains `daily_stats: DailyStatsRegistry`. It holds only connections built fresh this pass; carried-forward connections are FIX, so `None`.
- **AppState:** `swap_registry(next)` becomes `swap_registries(registry: BrokerRegistry, daily_stats: DailyStatsRegistry)`. That is one call, so a reload cannot publish one registry without the other. AppState also gets `daily_stats_handle() -> Arc<ArcSwap<DailyStatsRegistry>>`, and `registry_handle()` stays.
- **The poller:** `daily_stats_poller::run(pool, sources: Arc<ArcSwap<DailyStatsRegistry>>, stats)`. `poll_once` iterates `sources` the way it iterates the broker registry today. `select_one_environment_per_broker`, `broker_serves_venue` and the `NotConfigured` skip stay.

- [ ] **Step 1: Write the failing tests**
  - In `src/reload.rs` tests, modelled on `every_rest_connector_hands_back_an_exec_stream` (same four connections):

```rust
#[tokio::test]
async fn every_rest_connector_hands_back_daily_stats() {
    // same connections as every_rest_connector_hands_back_an_exec_stream
    let out = build_registry(&connections, true, &BrokerRegistry::new(), &stub_deps()).await;
    let mut codes: Vec<&str> = out.daily_stats.iter().map(|((code, _), _)| code.as_str()).collect();
    codes.sort_unstable();
    assert_eq!(codes, ["ALPACA", "BYBIT", "COINBASE", "OKX"]);
}
```

  - In `src/app_state.rs` tests, next to the existing `swap_registry` tests:

```rust
#[test]
fn swapping_registries_replaces_daily_stats_too() {
    // state with ALPACA/PAPER registered in both, then swap_registries(empty, empty):
    assert!(state.daily_stats_handle().load().get("ALPACA", "PAPER").is_none());
}
```

  - In `src/boundary_tests.rs`:

```rust
/// The order side carries no prices: previous closes come from `gateway::md`.
#[test]
fn order_adapter_carries_no_prices() {
    let adapter = include_str!("../crates/gateway/src/adapter.rs");
    assert!(!adapter.contains("daily_stats"), "BrokerAdapter still serves prices");
}
```

- [ ] **Step 2: Run them**

  Run: `cargo test every_rest_connector_hands_back_daily_stats swapping_registries_replaces_daily_stats_too order_adapter_carries_no_prices`
  Expected: compile errors (no field `daily_stats`, no `swap_registries`). Once those compile, `order_adapter_carries_no_prices` fails.

- [ ] **Step 3: Implement** the interfaces above.
  - **Bodies:** each adapter's `daily_stats` body moves unchanged.
  - **Call sites:** update every `swap_registry` caller, tests included.
  - **Tests that leave `adapter.rs`:** `daily_stats_defaults_to_not_configured` is deleted, because no default exists any more. The poller's `NotConfigured` arm stays for an adapter that returns it.

- [ ] **Step 4: Run them**

  Run: same command.
  Expected: PASS.

- [ ] **Step 5: Run the workspace**

  Run: `cargo test --workspace 2>&1 | grep "test result" | awk '{s+=$4;i+=$8;f+=$6} END {print s,f,i}'`
  Expected: `697 0 80` (695 + 3 new − 1 deleted).

- [ ] **Step 6: Commit**

```bash
git add crates/gateway src
git commit -m "refactor(md): previous closes come from a market-data registry, not the order adapter"
```

---

### Task 2: The market data gateway runs the feeds from the core's subscriptions

**Files:**
- Modify:
  - `crates/dataprovider/src/quote.rs`: add `InstrumentRef` and `covers`.
  - `src/preflight.rs:245-277`: `HeldRow` + `covers` become `dataprovider::InstrumentRef` + `dataprovider::covers`.
  - `src/main.rs`: feed startup, ~1280-1297.
  - `crates/gateway/src/md/mod.rs`.
- Create:
  - `crates/gateway/src/md/session.rs`;
  - `src/market_data.rs`, which replaces `src/quote_feed.rs` (`git mv`, then rewrite). Its `needs_recheck` / `next_recheck` and their 5 tests stay.

**Interfaces:**
- Produces, in `dataprovider`:

```rust
/// What the core tells a market data gateway about one held instrument: enough
/// for a feed to decide whether it covers it and what to call it. Plain data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstrumentRef {
    pub id: i64,
    pub symbol: String,
    pub instrument_class: Option<String>,
    pub asset_class: Option<String>,
    pub venue: Option<String>,
}

/// Whether `feed` subscribes this instrument: every `Some` field of
/// `candidates()` matches, and `to_feed_symbol` accepts the symbol.
pub fn covers(feed: &dyn FeedSymbology, i: &InstrumentRef) -> bool;
```

  `covers` uses today's `preflight::covers` logic. `InstrumentRef.symbol` is non-optional; preflight skips rows without a symbol, which matches today's `is_some_and`.

- Produces, in `gateway::md`:

```rust
/// The core's handle on the market data gateway: what is held.
#[derive(Clone)]
pub struct MdHandle(/* watch::Sender<BTreeMap<i64, InstrumentRef>> */);
impl MdHandle {
    pub fn subscribe(&self, instruments: Vec<InstrumentRef>);   // insert / replace by id
    pub fn unsubscribe(&self, instrument_ids: &[i64]);
}

pub struct MarketDataGateway { /* quote_tx, held watch, StreamHealthRegistry */ }
impl MarketDataGateway {
    /// The gateway and the one receiver of every quote it emits (the mark router's input).
    pub fn new(health: StreamHealthRegistry) -> (Self, mpsc::Receiver<Quote>);
    pub fn handle(&self) -> MdHandle;
    /// Spawn every connector's public feed, supervised under its label, for the
    /// life of the process (as main.rs does today).
    pub fn start_public_feeds(&self);
}
```

  The quote channel capacity stays 1024, as in `main.rs:1061` today.

- `gateway::md::session::FeedSession` implements `stream_supervisor::Session`. It is built from `Arc<dyn QuoteSource>`, the held `watch::Receiver`, the quote sender and a `StreamHandle`, and replaces `QuoteFeedSession`:
  - **Waiting:** `run_once` waits on the watch until the held set has an instrument the feed `covers`, then maps the covered set to `HashMap<feed_symbol, id>` (last id wins on a symbol collision, as today). While waiting, `health.set_connecting()` stays as it is today, and the session does not return.
  - **Running:** it runs `run_session`, racing a watcher. On each watch change the watcher sends the covered instruments the session does not know yet as `SymbolAdds`. Removals are not sent mid-session: feeds cannot unsubscribe. The next reconnect builds from the current set, so a removed instrument drops out.
  - **Exit:** the session ends only when the feed ends it, as today.
- `src/market_data.rs` (OMS):
  - `pub async fn held_instruments(pool: &PgPool) -> Result<Vec<InstrumentRef>, sqlx::Error>`: today's `load_subscribable` SQL with no per-feed filter. It selects `i.id, i.symbol, i.instrument_class, i.asset_class, i.venue` for `net_qty <> 0` and `ACTIVE` instruments.
  - `pub fn diff(sent: &BTreeSet<i64>, held: &[InstrumentRef]) -> (Vec<InstrumentRef>, Vec<i64>)`: returns (to subscribe, to unsubscribe).
  - `pub async fn run(pool: PgPool, md: MdHandle, mut order_rx: broadcast::Receiver<OrderLaneEvent>) -> !`: reads `held_instruments` at start, then on each `next_recheck` and at least every 60 s (`IDLE_RECHECK_SECS`). It applies `diff` through `md`. A query error logs `warn!` and keeps the last set.
- `main.rs`:
  - creates `MarketDataGateway::new(state.stream_health())` before `AppState`;
  - hands the `Receiver` to `mark_router::run` in place of today's `quote_rx`;
  - calls `start_public_feeds()`;
  - spawns `market_data::run(...)`.

  Until Task 3, OPRA still needs a sender, and it must reach the same mark router. So for this task only, `MarketDataGateway` has `pub fn quote_sender(&self) -> mpsc::Sender<Quote>`, and `AppState::quote_tx` is that clone. Task 3 deletes both.

- [ ] **Step 1: Write the failing tests**
  - In `crates/gateway/src/md/session.rs` tests, with a fake `QuoteSource`:
    - `candidates()` gives venue `Some("X")`, and `to_feed_symbol` returns `Some(symbol.to_lowercase())`;
    - `run_session` records the symbols it got and every `SymbolAdds` it receives, then waits on `add_rx` until it is closed or a test signal ends it.

```rust
#[tokio::test]
async fn a_feed_waits_until_something_it_covers_is_held() {
    // held = {1: venue "Y"} → run_once has not called run_session after 50 ms
    // then subscribe {2: venue "X", symbol "ABC"} → run_session called with {"abc": 2}
}

#[tokio::test]
async fn an_instrument_held_mid_session_is_added() {
    // session running with {"abc": 2}; subscribe {3: "X", "DEF"} → fake receives SymbolAdds containing ("def", 3)
}

#[tokio::test]
async fn an_unsubscribed_instrument_is_not_resubscribed_on_reconnect() {
    // run_once with {2,3}; unsubscribe [3]; fake ends the session; next run_once → symbols == {"abc": 2}
}
```

  - In `crates/dataprovider/src/quote.rs` tests: `covers_requires_every_filter_field_and_a_feed_symbol`. This is today's preflight behaviour: a `Some` filter field against a `None` column is not a match, and a symbol the feed declines is not covered.
  - In `src/market_data.rs` tests:

```rust
#[test]
fn diff_subscribes_new_and_unsubscribes_gone() {
    // sent {1,2}, held [2,3] → (vec![ref(3)], vec![1])
}
```

- [ ] **Step 2: Run them**

  Run: `cargo test -p gateway md::session; cargo test -p dataprovider covers; cargo test market_data`
  Expected: compile errors (modules and functions missing).

- [ ] **Step 3: Implement** the interfaces above.
  - Delete `QuoteFeedSession`, `wait_for_subscribable`, `watch_held` and `load_subscribable`.
  - Point `preflight` at `dataprovider::covers`, and delete its `HeldRow` / `covers`. Keep `preflight`'s own SQL.
  - Update `src/quote_feed.rs` mentions in comments to `market_data`.

- [ ] **Step 4: Run them**

  Run: same commands.
  Expected: PASS.

- [ ] **Step 5: Run the workspace**

  Run: as in Task 1.
  Expected: 0 failed, 80 ignored. Passed = Task 1's count + 5 new + whatever preflight `covers` tests moved into dataprovider (the count moves with them).

- [ ] **Step 6: Commit**

```bash
git add crates src
git commit -m "feat(md): the market data gateway runs every feed from the core's subscriptions"
```

---

### Task 3: OPRA under the market data gateway; the mark router is fed only from it

**Files:**
- Move: `src/opra_stream.rs` → `crates/gateway/src/md/opra.rs` (`git mv`, then fix paths). The `databento` dependency moves from the root `Cargo.toml` to `crates/gateway/Cargo.toml` if the OMS no longer uses it.
- Modify:
  - `crates/gateway/src/md/mod.rs`;
  - `src/feeds.rs` (static OPRA from `gateway::md::opra`);
  - `src/reload.rs:240-305` (`restart_databento_feed` and `ReloadStreams::databento`);
  - `src/app_state.rs`: drop `quote_tx` / `quote_tx()`, add `market_data() -> &MdHandle`-style access to what a restart needs;
  - `src/main.rs`: the OPRA boot block and the `quote_tx` creation;
  - `src/handlers.rs` test `AppState` constructors (2416, 3169);
  - `src/reload_tests.rs:32-36`;
  - `src/boundary_tests.rs`;
  - `docs/contributing/adding-a-connector.md`.

**Interfaces:**
- `gateway::md::opra`:
  - `pub const LABEL: &str = "DATABENTO/OPRA";`
  - `pub const STREAM_CODE: &str = "databento-opra";`
  - `DatabentoOpraFeed` unchanged.
- `MarketDataGateway::start_vendor_feed(&self, source: Arc<dyn QuoteSource>, label: &'static str, health: StreamHandle) -> tokio::task::JoinHandle<()>`: runs a `FeedSession` (Task 2) under `supervise(label, …)` and returns the handle for the caller's `StreamRegistry`.
- `reload::restart_databento_feed(api_key, md: &MarketDataGateway, stream_health, streams)`: the same abort-and-insert under `opra::STREAM_CODE`, and the same `("DATABENTO", "OPRA")` health handle. It no longer takes `pool`, `bus` or `quote_tx`.
- `AppState` holds `Arc<MarketDataGateway>` in place of `quote_tx`.

- [ ] **Step 1: Write the failing tests**
  - In `crates/gateway/src/md/opra.rs` tests:

```rust
#[test]
fn opra_keeps_its_names() {
    assert_eq!(LABEL, "DATABENTO/OPRA");
    assert_eq!(STREAM_CODE, "databento-opra");
}
```

  - In `src/boundary_tests.rs`:

```rust
/// The pricing service has one source: no OMS file and no order-side gateway
/// file holds a quote sender; only gateway::md does.
#[test]
fn only_the_market_data_gateway_sends_quotes() {
    let root = env!("CARGO_MANIFEST_DIR");
    let mut files = vec![format!("{root}/crates/gateway/src/adapter.rs")];
    for dir in [format!("{root}/src"), format!("{root}/crates/gateway/src/connectors")] {
        // every .rs file under dir, recursively (std::fs::read_dir walk)
    }
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap();
        for word in ["Sender<Quote>", "Sender<dataprovider::Quote>", "quote_tx"] {
            assert!(!text.contains(word), "{f} holds a quote sender ({word})");
        }
    }
}
```

  The walk skips `boundary_tests.rs` itself, because it contains the words.

- [ ] **Step 2: Run them**

  Run: `cargo test -p gateway opra_keeps_its_names; cargo test only_the_market_data_gateway_sends_quotes`
  Expected: the first fails to compile (no `md::opra`). The second fails, naming `src/app_state.rs` (`quote_tx`).

- [ ] **Step 3: Implement** the interfaces above.
  - Remove `quote_tx` everywhere in the OMS.
  - The test `AppState`s build a throwaway `MarketDataGateway::new(StreamHealthRegistry::new()).0`.
  - Make the recipe's step 1 mention that `feed.rs` runs under `gateway::md` (the connector only supplies the feed). Add one sentence: price-only vendors (OPRA) live in `crates/gateway/src/md/`.

- [ ] **Step 4: Run them**

  Run: same commands.
  Expected: PASS.

- [ ] **Step 5: Run the workspace**

  Run: as in Task 1, plus `cargo test -p gateway`.
  Expected: 0 failed, 80 ignored; Task 2's count + 2.

- [ ] **Step 6: Check the release link and the boot** (~5 min)
  1. Run `OMS_OPENSSL_STATIC=1 cargo build --release && ! otool -L target/release/oms | grep -qi 'homebrew\|openssl' && echo STATIC_OK`.
     Expected: `STATIC_OK`.
  2. Restart the dev OMS: stop its background task; run `cargo build && ./target/debug/oms > /tmp/oms-server.log 2>&1` in the background; wait for `OMS listening`.
  3. Run `grep -iE "registered|supervisor|quote feed|listening|daily stats" /tmp/oms-server.log`.

     Expected:
     - Alpaca and Binance FIX are registered;
     - `connecting` for the four `*/SPOT` feeds and `DATABENTO/OPRA`;
     - `subscribing held set` for BINANCE and BYBIT (2 each, as before);
     - `OMS listening`;
     - no new kind of error beyond the known environment ones (local Kafka down, the preflight data warnings, Alpaca stats 400).

- [ ] **Step 7: Commit**

```bash
git add crates src Cargo.toml Cargo.lock docs/contributing/adding-a-connector.md
git commit -m "feat(md): OPRA runs under the market data gateway; the mark router's only feeder is the gateway"
```
