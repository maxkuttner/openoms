# Co-located Strategies (Phase 8) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A co-located strategy trades Binance directly. The OMS follows its account and books its fills seconds after they happen, controls it over a TLS WebSocket, and can kill it without its cooperation.

**Architecture:** Four pieces, with new crates under `crates/`.
- **Two client-side crates**, used by the co-located program:
  - `colo-risk`: pure pre-trade checks;
  - `colo-link`: the control-link protocol types plus a client with the dead man's switch.
- **The OMS's side** (`src/colo/`):
  - serves the control link on its own TLS port;
  - follows each co-located strategy's Binance account through a gateway "follow" stream;
  - books every fill as its own small external order, with an id derived from the trade, so a repeated fill is never booked twice;
  - runs Kill and the alarms.
- **A reference market maker** (`crates/colo-example`) ties the pieces together on Binance testnet.

**Tech Stack:**
- Rust 2021, tokio;
- `tokio-tungstenite` + `tokio-native-tls` (TLS WebSocket server and client; `native-tls` uses the macOS Security framework, so no new OpenSSL link);
- serde JSON;
- the `gateway` crate's Binance connector;
- sqlx/Postgres, `uuid` v5.

**Spec:** `docs/superpowers/specs/2026-11-06-colocated-strategies-path-2-design.md` (this branch). Its parent is `docs/superpowers/specs/2026-11-04-target-architecture-design.md`, §4.4 path ② (branch `docs/target-architecture`).

## Global Constraints

- **Done when:** "a sample co-located program trades on a paper venue (Binance testnet), its fills appear in the OMS positions, and Kill stops it."
- **Values:**
  - dead man's switch **5 s** (per strategy);
  - heartbeat **1 s**;
  - limits must be acknowledged within **10 s**;
  - kill acknowledgement expected within **5 s**;
  - unmonitored is an **alarm by default**; `kill_when_unmonitored` is off by default;
  - tag = the client order id up to its **second `-`** (`mm1-q-0001` → `mm1-q`).
- **Fills only:** the OMS books only fills. Open orders are held in memory; per-minute counts of placed, cancelled and rejected orders are kept.
- **The OMS never places orders on a co-located account.** Its key there only follows and cancels.
- **Co-located strategies:**
  - they are only Active or Killed: no Reducing, no Flatten;
  - phase 7's cancel-on-disconnect and startup cancel do not apply;
  - Kill keeps the control link open.
- **Kill fights:** while killed, every new order seen on the account is cancelled with the OMS's key, within Binance's rate limits.
- **Unknown pairs:** a fill on an unknown pair is looked up in Binance's exchange information, added to the catalog, and booked. It is never left unbooked.
- **Gateway boundary:** `crates/gateway`, `colo-risk` and `colo-link` hold no `sqlx` / `rdkafka` / `axum`. `boundary_tests` stay green; add the new crates to `gateway_depends_on_no_database_or_kafka`'s check.
- **Rules:**
  - DB tests are `#[ignore]`; never run `--ignored`; never create databases on the shared dev Postgres.
  - No help text in the UI.
  - No compatibility shims.
  - Stage files explicitly; a heredoc and `git commit` never go in one Bash call.
  - Ask before editing the user's `oms.toml`.
- **Branch:** `feat/colo-strategies` (spec `b685e84`, on top of `feat/engine-gaps` `329697a`).
- **Baseline:** 771 passed, 0 failed, 88 ignored; cockpit 30 tests.

## Review Focus

1. **The same fill arrives twice,** live then again from gap recon, or twice after a reconnect. It is booked once. Test: `a_repeated_fill_books_once` (Task 4), via the derived order id.
2. **A strategy that ignores Kill and keeps placing orders** has each new order cancelled. The cockpit says "killed, still fighting". Test: `a_killed_strategys_new_order_is_cancelled_on_sight` (Task 5).
3. **The OMS restarts while a strategy is quoting.** The strategy's dead man's switch stops it within 5 s, and it resumes on reconnect when the state is active. The OMS cancels nothing. Tests: `the_switch_trips_after_the_timeout_and_rearms_on_active` (Task 2), and `colo_strategies_get_no_phase7_cancels` (Task 5).
4. **The strategy acknowledges an old limits version and keeps trading.** After 10 s the "unmonitored" alarm fires, and it kills only when `kill_when_unmonitored` is set. Test: `stale_limits_raise_the_alarm_after_10s` (Task 5).
5. **A fill on a pair not in the catalog** gets the instrument created, then is booked. Test: `an_unknown_pair_is_added_before_booking` (Task 4, ignored DB test, plus the pure `instrument_from_exchange_info` parser test).

---

## File Structure (end state)

```
crates/colo-risk/src/lib.rs            Limits, OrderIntent, RiskBook (pure)
crates/colo-link/src/proto.rs          ToStrategy / FromStrategy messages (serde)
crates/colo-link/src/client.rs         Link client + DeadMan switch
crates/colo-example/src/main.rs        reference market maker (Binance testnet)
crates/gateway/src/contract.rs         + FollowEvent, FollowHandle
crates/gateway/src/connectors/binance/stream.rs   + follow mode
crates/gateway/src/connectors/binance/adapter.rs  + my_trades, symbol_info
db/migrations/ods/oms/0038_ALTER_STRATEGY_ADD_COLOCATED.sql
src/colo/mod.rs                        ColoHub: per-strategy runtime state (link, book, counters, alarms)
src/colo/link_server.rs                TLS WebSocket control endpoint
src/colo/follow.rs                     follow streams per strategy; gap recon
src/colo/booking.rs                    external fill → order, instrument on demand
src/colo/control.rs                    kill / enable / fight for co-located strategies
cockpit/src/pages/Strategies.tsx       co-located fields and status
docs-site/src/content/docs/strategies/colocated.md
```

---

### Task 1: The risk library (`crates/colo-risk`)

**Files:** create `crates/colo-risk/{Cargo.toml,src/lib.rs}`, and add the member to the root `Cargo.toml`.

**Interfaces (produces):**

```rust
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Limits { pub max_order_qty: Option<f64>, pub max_order_notional: Option<f64>,
    pub max_position_qty: Option<f64>, pub max_position_notional: Option<f64>,
    pub price_band_pct: Option<f64>, pub max_orders_per_sec: Option<u32> }
pub struct OrderIntent<'a> { pub symbol: &'a str, pub quote_currency: &'a str, pub buy: bool, pub qty: f64, pub price: f64 }
#[derive(Debug, PartialEq)] pub struct Reject { pub code: &'static str, pub message: String }
pub struct RiskBook { /* limits, version, positions: HashMap<String, f64>, halted: bool, window: VecDeque<Instant> */ }
impl RiskBook {
    pub fn new(limits: Limits, version: u64) -> Self;
    pub fn set_limits(&mut self, limits: Limits, version: u64);
    pub fn version(&self) -> u64;
    pub fn set_positions(&mut self, positions: impl IntoIterator<Item = (String, f64)>);
    pub fn on_fill(&mut self, symbol: &str, buy: bool, qty: f64);
    pub fn halt(&mut self); pub fn resume(&mut self); pub fn halted(&self) -> bool;
    /// `reference` is the strategy's own mid. Records the order in the rate window when it passes.
    pub fn check(&mut self, o: &OrderIntent, reference: f64, now: std::time::Instant) -> Result<(), Reject>;
}
pub const USD_LIKE: [&str; 3] = ["USD", "USDT", "USDC"];
```

**The checks**, mirroring phase 7's guard:
- `halted` → `"halted"`;
- order qty and notional;
- projected position qty and notional (position plus the signed qty);
- notional limits only for a USD-like quote, else `"no_notional_limit"`;
- band, `|price − reference| / reference > band% × (1 + 1e-9)` → `"price_band"`;
- more than `max_orders_per_sec` in the trailing 1 s → `"order_rate"`.

**No I/O, no allocation on the pass path** beyond the window push.

- [ ] **Step 1: Write the failing tests:**
  - `a_halted_book_rejects_everything`;
  - `order_and_position_limits_reject_with_their_code`;
  - `notional_limits_need_a_usd_like_quote`;
  - `the_band_rejects_a_price_too_far_from_reference`;
  - `the_order_rate_cap_counts_a_trailing_second`: with cap 3, the 4th order inside 1 s is rejected; at `now + 1001 ms` it passes;
  - `fills_move_the_position_the_limits_see`.
- [ ] **Step 2: Run** `cargo test -p colo-risk` and expect compile errors.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p colo-risk` and expect PASS. Then run the workspace sum and expect 0 failed.
- [ ] **Step 5: Commit** with `feat(colo-risk): in-memory pre-trade checks for co-located strategies`.

---

### Task 2: The control link: protocol, client and dead man's switch (`crates/colo-link`)

**Files:** create `crates/colo-link/{Cargo.toml,src/lib.rs,src/proto.rs,src/client.rs}`.

**Interfaces (produces):**

```rust
// proto.rs — JSON, tagged by "type"
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)] #[serde(tag = "type", rename_all = "snake_case")]
pub enum ToStrategy {
    State { active: bool, limits: colo_risk::Limits, limits_version: u64, positions: Vec<Position> },
    Limits { limits: colo_risk::Limits, limits_version: u64 },
    Positions { positions: Vec<Position> },
    Kill, Resume, Heartbeat,
}
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)] #[serde(tag = "type", rename_all = "snake_case")]
pub enum FromStrategy { AckLimits { limits_version: u64 }, AckKill, Heartbeat }
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)] pub struct Position { pub symbol: String, pub qty: f64 }
pub const HEARTBEAT: Duration = Duration::from_secs(1);

// client.rs
pub struct DeadMan { /* timeout, last_heard: Instant, killed: bool, active: bool */ }
impl DeadMan {
    pub fn new(timeout: Duration) -> Self;
    pub fn heard(&mut self, now: Instant);                         // any message from the OMS
    pub fn apply(&mut self, msg: &ToStrategy);                     // State/Kill/Resume set active/killed
    pub fn may_trade(&self, now: Instant) -> bool;                 // active && !killed && now - last_heard <= timeout
}
pub struct LinkConfig { pub url: String, pub token: String, pub ca_pem: Option<Vec<u8>>, pub dead_man: Duration }
pub struct Link { /* sender for FromStrategy */ }
impl Link {
    /// Connects (and reconnects every second when down), heartbeats every second, and
    /// delivers each `ToStrategy` on the returned channel. Sends the token as
    /// `Authorization: Bearer` on the upgrade.
    pub async fn spawn(cfg: LinkConfig) -> (Link, tokio::sync::mpsc::Receiver<ToStrategy>);
    pub async fn send(&self, msg: FromStrategy);
}
```

**Resume rule (spec 4.4):**
- after the switch tripped, a reconnect's `State { active: true }` re-arms it;
- `Kill` sets `killed` until `Resume`;
- a `State { active: false }` keeps it stopped.

- [ ] **Step 1: Write the failing tests:**
  - `messages_round_trip_as_tagged_json`: `{"type":"ack_limits","limits_version":7}` ⇄ `AckLimits{7}`;
  - `the_switch_trips_after_the_timeout_and_rearms_on_active`: heard at t0; `may_trade(t0 + 4.9 s)` is true; `may_trade(t0 + 5.1 s)` is false; then `heard` plus `apply(State{active: true…})` makes it true again;
  - `kill_holds_until_resume_even_when_heard`.
- [ ] **Step 2: Run** `cargo test -p colo-link` and expect compile errors.
- [ ] **Step 3: Implement.** The client uses `tokio-tungstenite` with `native-tls`, and `ca_pem` is added as a trusted root when set (self-signed test certificate).
- [ ] **Step 4: Run** and expect PASS. Then run the workspace sum and expect 0 failed.
- [ ] **Step 5: Commit** with `feat(colo-link): control-link protocol and client with a dead man's switch`.

---

### Task 3: Registry extension, the follow credential, and the cockpit fields

**Files:**
- Create: `db/migrations/ods/oms/0038_ALTER_STRATEGY_ADD_COLOCATED.sql`.
- Modify:
  - `src/strategies/{mod.rs,admin.rs}`;
  - `src/setup/database/assets.rs` (count 38 → 39);
  - `cockpit/src/pages/Strategies.tsx`;
  - `cockpit/src/strategies.ts` (+ test).

**Interfaces:**
- **Migration 0038** adds to `strategy`:
  - `mode TEXT NOT NULL DEFAULT 'VIA_OMS' CHECK (mode IN ('VIA_OMS','COLOCATED'))`;
  - `follow_environment TEXT CHECK (follow_environment IN ('PAPER','LIVE'))`;
  - `follow_credentials BYTEA` (sealed with the master key, like stored broker credentials: `crate::secrets` seal/open);
  - `dead_man_secs INTEGER NOT NULL DEFAULT 5`;
  - `max_orders_per_sec INTEGER`;
  - `kill_when_unmonitored BOOLEAN NOT NULL DEFAULT false`;
  - `limits_version BIGINT NOT NULL DEFAULT 1`, bumped by any limit change in `update_strategy`.
- **`Strategy`** gains `mode: Mode { ViaOms, Colocated }`, `follow_environment: Option<String>`, `dead_man_secs: u32`, `max_orders_per_sec: Option<u32>`, `kill_when_unmonitored: bool`, `limits_version: i64`, plus `fn risk_limits(&self) -> colo_risk::Limits`.
- **Admin:** `PUT /admin/strategies/{code}/follow-credentials`, body `{ "api_key", "private_key" }`, the Binance Ed25519 pair. It stores the sealed credential and returns 204; the credential is never returned. Create/update accept the new fields.
- **Cockpit:**
  - fields: mode select, follow environment, dead-man seconds, max orders/sec, kill when unmonitored;
  - a **Follow key** row action opening a two-field form (no help text);
  - helper `export function isColocated(row): boolean` with a test.

- [ ] **Step 1: Write the failing tests:**
  - `a_limit_change_bumps_the_limits_version`: a pure fn `next_limits_version(old: &UpdateStrategy, current: i64) -> i64` changes only when a limit field is present;
  - `risk_limits_carry_the_strategy_values`;
  - cockpit `isColocated`.
- [ ] **Step 2: Run** and expect compile/test failures.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the workspace sum and `npm test`, and expect 0 failed. Regenerate `docs/openapi.json`.
- [ ] **Step 5: Commit** with `feat(strategies): co-located mode, follow credential and control settings in the registry`.

---

### Task 4: Following the account and booking its fills

**Files:**
- Modify:
  - `crates/gateway/src/contract.rs`;
  - `crates/gateway/src/connectors/binance/{stream.rs,adapter.rs}`;
  - `src/order_engine.rs`;
  - `src/main.rs`.
- Create: `src/colo/{mod.rs,follow.rs,booking.rs}`.

**Interfaces:**
- **Gateway:**

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum FollowEvent {
    Connected,
    Opened { venue_order_id: String, client_order_id: String, symbol: String, buy: bool, qty: f64, price: f64 },
    Closed { venue_order_id: String, outcome: &'static str }, // "canceled" | "rejected" | "expired" | "filled"
    Fill(ExternalFill),
}
#[derive(Debug, Clone, PartialEq)]
pub struct ExternalFill { pub trade_id: String, pub venue_order_id: String, pub client_order_id: String,
    pub symbol: String, pub buy: bool, pub qty: f64, pub price: f64 }
#[derive(Clone)] pub struct FollowHandle(pub mpsc::Sender<(String /*strategy code*/, FollowEvent)>);
// binance/stream.rs
pub async fn follow(environment: &'static str, strategy_code: String, adapter: Arc<BinanceAdapter>, health: StreamHandle, sink: FollowHandle);
// binance/adapter.rs
pub async fn my_trades(&self, symbol: &str, from_trade_id: Option<i64>) -> Result<Vec<ExternalFill>, BrokerError>; // GET /api/v3/myTrades
pub async fn symbol_info(&self, symbol: &str) -> Result<Option<InstrumentDef>, BrokerError>;         // exchangeInfo?symbol=
```

  `follow` reuses the user-data-stream connection. Its event handler maps `executionReport` as follows:
  - `NEW` → `Opened`;
  - `TRADE` → `Fill`, with `trade_id = t`;
  - `CANCELED` / `REJECTED` / `EXPIRED` → `Closed`;
  - fully filled → `Closed{"filled"}` after the fill.

  It reports **every** order, whatever the client id.
- **OMS `src/colo/booking.rs`:**

```rust
pub fn external_order_id(strategy_code: &str, trade_id: &str) -> Uuid;   // Uuid::new_v5(&COLO_NAMESPACE, "{code}:{trade_id}")
pub fn tag_of(client_order_id: &str) -> Option<&str>;                      // up to the second '-'
pub async fn book_fill(state: &AppState, strategy: &Strategy, fill: &ExternalFill) -> Result<Booked, String>;
pub enum Booked { New, AlreadyBooked }
```

  **`book_fill`:**
  1. If an order with `external_order_id` exists → `AlreadyBooked`.
  2. Otherwise resolve the instrument by `(broker_symbol, 'BINANCE')`. If it's missing: `symbol_info` → upsert `instrument` + `broker_instrument` (the universe loader's upsert), using the strategy's follow adapter.
  3. In one transaction: insert an `order_state` row with:
     - `origin = 'EXTERNAL'` (extend the CHECK in 0038);
     - `client_order_id` = the strategy's client id;
     - `external_order_id` = the venue id;
     - a limit order at the fill price for the fill qty, on the strategy's portfolio and its Binance account (`strategy.connections["BINANCE"]`).

     Append `order_submitted` + `order_routed` through the aggregate.
  4. Then `engine.exec(ExecEvent{kind: Fill{execution_id: trade_id, qty, price}})`, so the engine books the fill and positions as always.
- **OMS `src/colo/follow.rs`:**
  - `pub fn start_follows(state: &AppState)`: for every Active or Killed co-located strategy with a follow credential, build a `BinanceAdapter` from it and spawn `binance::stream::follow`, registered in `StreamRegistry` as `"colo-{code}"` (restarted on registry change).
  - **Gap recon on `Connected`:** for each symbol the strategy has external orders in, call `my_trades(symbol, from = last booked trade id + 1)` → `book_fill` each.
- **`src/colo/mod.rs`:** `ColoHub` (on `AppState`) consumes `FollowHandle` events per strategy:
  - fills → `book_fill`;
  - `Opened` / `Closed` → the in-memory open-order map;
  - every event → per-minute counters `{placed, cancelled, rejected}`.

  Task 5 adds kill and alarms here.

- [ ] **Step 1: Write the failing tests:**
  - **Gateway:**
    - `a_trade_event_on_any_client_id_is_a_follow_fill`: a non-UUID `c` → `FollowEvent::Fill` with `trade_id = t`;
    - `new_and_cancel_events_open_and_close`;
    - `my_trades_parse_into_external_fills`;
    - `instrument_from_exchange_info` (one symbol).
  - **OMS (pure):**
    - `a_repeated_fill_books_once`: `external_order_id` is deterministic and differs per trade;
    - `the_tag_is_the_client_id_up_to_its_second_dash`: `mm1-q-0001` → `Some("mm1-q")`; `x` → `None`.
  - **OMS (`#[ignore]` DB):**
    - `an_unknown_pair_is_added_before_booking`;
    - `book_fill_moves_the_strategy_position`.
- [ ] **Step 2: Run** and expect compile errors.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the workspace sum and expect 0 failed.
- [ ] **Step 5: Commit** with `feat(colo): follow co-located accounts and book their fills as external orders`.

---

### Task 5: The control-link server, Kill and fight, alarms, positions down the link

**Files:**
- Create: `src/colo/{link_server.rs,control.rs}`.
- Modify:
  - `src/colo/mod.rs`;
  - `src/strategies/control.rs` (route by mode; co-located strategies skip phase 7's cancels);
  - `src/config.rs` (`[control_link] port, certificate, private_key`, read like `fix_acceptor()`);
  - `src/main.rs`;
  - the cockpit page.

**Interfaces:**
- **`link_server.rs`:** `pub async fn serve(state: AppState, settings: ControlLinkSettings)`. A `tokio-native-tls` acceptor; then `accept_hdr_async` reads `Authorization: Bearer`, verifies it as a trading token (`auth::verify_key`) *without* the stopped-strategy gate, and maps the principal to a co-located strategy. On accept:
  - it sends `State{…}`;
  - it relays `ColoHub`'s pushes;
  - it records `AckLimits` / `AckKill` and heartbeats on the hub.

  The strategy's `allowed_addresses` are enforced on the peer address.
- **`ColoHub` gains:**
  - `pub fn link_status(&self, code) -> LinkStatus { connected, last_heard, acked_version, killed_ack }`;
  - `pub fn push(&self, code, ToStrategy)`;
  - `fn alarm(&self, code) -> Option<&'static str>`, which returns `"unmonitored: no link"`, `"unmonitored: limits version N not acknowledged"` or `"order rate"`.
- **Pure rule:**

```rust
pub fn unmonitored(connected: bool, acked: i64, current: i64, changed_at: Instant, now: Instant) -> Option<&'static str>; // 10 s grace after a limits change
```

- **`colo/control.rs`:**
  - `pub async fn kill(state, &Strategy) -> ControlReport`: set HALTED; push `Kill`; cancel every open order on the account (follow adapter `open_orders` → `cancel_order`); mark fighting.
  - `pub async fn enable(...)`: ACTIVE; push `Resume`; clear fighting.
  - **While killed:** each `Opened` event → `cancel_order` with the OMS key (rate-limited to 10 cancels per second per strategy, my value). The cockpit status says **killed, still fighting** while `Opened` keeps arriving.
  - **Positions:** after each booked fill, push `Positions` for that strategy.
  - **Limit changes** in `update_strategy` push `Limits{…, version}`.
- **Cockpit status:** `GET /admin/strategies` rows of co-located strategies gain:
  - `link_connected`, `last_heard_secs`;
  - `limits_version` and `acked_limits_version`;
  - `open_orders` (count);
  - `placed_per_min`, `cancelled_per_min`;
  - `alarm` (the reason string or null);
  - `fighting`.

  The Strategies page shows them, with `alarm` as a red badge.
- **Switching off the strategy's own key** (spec 4.1, opt-in): Binance spot has no documented master-account call to disable a sub-account's API key. If the executor confirms that, the opt-in is not built and Kill relies on the cancel-all plus fight; ledger it. If such a call exists, add `disable_key` behind a per-strategy flag.
- **`strategies::control`:**
  - `kill` / `enable` dispatch on `mode`;
  - `boot_recovery` and `on_session_end` skip `Colocated`;
  - `flatten` refuses `Colocated` with 400 `"co-located strategies have no Flatten"`.

- [ ] **Step 1: Write the failing tests:**
  - `stale_limits_raise_the_alarm_after_10s`: `unmonitored(true, 6, 7, t0, t0 + 9 s)` is `None`; at `t0 + 11 s` it is `Some(..)`; `connected = false` → `Some` at once;
  - `a_killed_strategys_new_order_is_cancelled_on_sight`: a pure decision fn `fight(killed: bool, ev: &FollowEvent) -> Option<String /*venue id to cancel*/>`;
  - `colo_strategies_get_no_phase7_cancels`: pure fn `phase7_cancels_apply(mode) -> bool`;
  - `flatten_refuses_a_colocated_strategy`;
  - an in-process link test, `a_link_gets_the_state_first_and_acks_are_recorded`: start `serve` on 127.0.0.1 with the gateway's `fix_acceptor/testdata` certificate and a stub token check; connect with `colo_link::Link`; the first message is `State`; send `AckLimits{1}` → `link_status.acked_version == 1`.
- [ ] **Step 2: Run** and expect compile errors.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the workspace sum and `npm test`, and expect 0 failed.
- [ ] **Step 5: Commit** with `feat(colo): control link, kill that keeps fighting, unmonitored and order-rate alarms`.

---

### Task 6: Reference market maker, a lighter gateway build, docs, end to end

**Files:**
- Create:
  - `crates/colo-example/{Cargo.toml,src/main.rs,src/quote.rs}`;
  - `docs-site/src/content/docs/strategies/colocated.md`, with a sidebar entry.
- Modify: `crates/gateway/Cargo.toml` (feature `fix`, default on); the `#[cfg(feature = "fix")]` gates on `fix/`, `fix_acceptor/`, and the FIX parts of the `ibkr` and `binance` connectors.

**Interfaces:**
- **`quote.rs`:**

```rust
pub fn quotes(mid: f64, half_spread_bps: f64, tick: f64) -> (f64 /*bid*/, f64 /*ask*/); // rounded to tick, bid < mid < ask
pub fn client_id(tag: &str, n: u64) -> String;                                           // "mm1-q-000001"
```

- **The program:** `colo-example --link wss://HOST:PORT --token T --ca PEM --symbol BTCUSDT --half-spread-bps 20 --qty 0.0002`. It reads the Binance key from `BINANCE_API_KEY` / `BINANCE_PRIVATE_KEY` (testnet). Its loop:
  1. read the mid from the gateway's Binance public feed;
  2. every 2 s, cancel and replace one bid and one ask;
  3. pass every order through `RiskBook::check`;
  4. place no new order when `DeadMan::may_trade` is false (it cancels its own on the transition);
  5. acknowledge limits and kill;
  6. apply `Positions`.
- **Lighter gateway build:** `colo-example` depends on `gateway` with `default-features = false`, so no FIX and no C++ build.
- **Docs:** setup (admin creates the account and two keys), the control link protocol (every message), the dead man's switch, Kill and fight, alarms, tags, and the risk library API.

- [ ] **Step 1: Write the failing tests:**
  - `quotes_straddle_the_mid_on_the_tick`;
  - `client_ids_carry_the_tag`;
  - `the_gateway_builds_without_fix`, which runs `cargo build -p gateway --no-default-features` as the step-4 check.
- [ ] **Step 2: Run** and expect failures.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace`, `cargo build -p gateway --no-default-features`, `npm test`, and the static release check (`OMS_OPENSSL_STATIC=1 cargo build --release && ! otool -L target/release/oms | grep -qi 'homebrew\|openssl' && echo STATIC_OK`). Expect everything green and `STATIC_OK`.
- [ ] **Step 5: End to end on the dev OMS.** Ask the user first: it needs a `[control_link]` block in `oms.toml`, a second Binance testnet account with two keys, and a cockpit setup.
  1. Register `mm1` co-located; store the OMS's follow key; reset its REST token.
  2. Run `colo-example` against testnet. Expect quotes on testnet, fills in the blotter tagged `mm1-q`, and the `mm1` position moving.
  3. Press Kill. Expect the `kill` acknowledgement, open quotes cancelled by the OMS, and the program halted. Then start a second, non-compliant run with `--ignore-kill` (a flag in the example). Its orders are cancelled on sight, and the cockpit shows "killed, still fighting".
  4. Stop the OMS. Expect the program to stop quoting within 5 s, and resume after the OMS is back and Enable is pressed.
- [ ] **Step 6: Commit** with `feat(colo): reference market maker, FIX-free gateway build, co-located strategy docs`.
