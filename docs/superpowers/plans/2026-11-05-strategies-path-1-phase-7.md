# Strategies / Algos Path ① (Phase 7) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A strategy logs on to the OMS over FIX (or uses REST), trades through the same checks and order engine as the cockpit, and can be killed, re-enabled or flattened from the cockpit.

**Architecture:**
- **Registry:** a new `strategy` table plus `strategy_connection` (one account per broker code) drive everything.
- **One order path:** the order path is extracted from `handlers::orders_submit` / `orders_cancel` into `src/order_entry.rs`. REST, FIX and Flatten all call it. A strategy guard inside it adds the strategy rules:
  - exclusive portfolio;
  - state;
  - per-strategy limits with per-instrument overrides;
  - the USD-like currency gate;
  - the price band;
  - waiting for a mark.
- **FIX:** the wire protocol lives in `crates/gateway/src/fix_acceptor/`, a QuickFIX acceptor that speaks plain request/report types and no DB. The OMS side, `src/strategies/session.rs`:
  - answers Logon checks from a cached registry snapshot;
  - turns requests into order-entry calls;
  - sends execution reports from the order lane, replaying what a strategy missed from the event store.

**Tech Stack:** Rust 2021, axum, sqlx/Postgres, the `quickfix` crate 0.2 (`build-with-ssl`, already in `crates/gateway`), bcrypt (already used for API keys), React/Mantine cockpit (`CrudResource`).

**Spec:** `docs/superpowers/specs/2026-11-05-strategies-path-1-design.md` (this branch). Its parent is `docs/superpowers/specs/2026-11-04-target-architecture-design.md`, §4.4 path ① and §7 phase 7, on branch `docs/target-architecture`.

## Global Constraints

- **Done when** (parent §7): "A test client places an order over FIX".
- **Anti-goals** (spec §2):
  - a strategy order skips any check a cockpit order goes through, or is booked anywhere other than the order engine;
  - Kill depends on the strategy behaving;
  - a strategy's working orders survive after its session is gone;
  - a rejected order reaches the strategy without a reason it can read;
  - a FIX password is stored so that it can be read back.
- **Gateway:** `crates/gateway` keeps no `sqlx` / `rdkafka` / `axum`. `boundary_tests::gateway_depends_on_no_database_or_kafka` stays green. FIX tags live only in `crates/gateway`; `boundary_tests::central_files_name_no_connector` stays green.
- **Values:**
  - hold timeout **2 s**;
  - USD-like quote currencies are exactly **USD, USDT, USDC**;
  - message types: in **35=D, F, H, x**; out **35=8, 9, y, j**;
  - order types: market, limit;
  - TIF: day, GTC, IOC;
  - Flatten TIF is **GTC**;
  - FIX version **FIX 4.4** (`FIX.4.4`).
- **Single user, pre-release:** no compatibility shims. No help text in the UI; explanations go in `docs-site`.
- **Tests and DB:** DB tests are `#[ignore]` against the shared dev DB. Never create databases on the shared dev Postgres. Never run the credential rotate DB test, and never run `--ignored`. Never touch `oms-postgres` / `oms-pgdata`.
- **Git:**
  - Never commit `.gitignore`, `.ignore` or `docs/superpowers/plans/2026-09-28-options-search-chain-strategy.md`.
  - Stage files explicitly. A heredoc and `git commit` never go in one Bash call.
  - Branch `feat/strategy-fix` (spec commit `c43e05b`, on top of `feat/md-gateway` `473eaee`).
- **Baseline:** `cargo test --workspace` summed over every result line gives 706 passed, 80 ignored. The cockpit runs `npm test` in `cockpit/`. Ledger both counts after each task.

## Plan decisions that refine the spec

The executor treats these as settled. The reasons are here so a reviewer can weigh them.

1. **Sequence numbers.** Spec §4.4 says "Sequence numbers persist in Postgres". QuickFIX's Postgres store needs libpq, which would break the static macOS release link (`otool` check). The plan instead:
   - runs the acceptor with `ResetOnLogon=Y` and an in-memory store, as the existing initiators do;
   - keeps a per-strategy **report cursor** (`strategy.fix_report_position`, the last `global_position` reported) in Postgres;
   - on Logon, replays every event on the strategy's FIX orders past the cursor.

   The spec's purpose, "missed execution reports are resent", is met from Postgres.
2. **IP allowlist is exact addresses, not CIDRs.** QuickFIX's `AllowedRemoteAddresses` matches addresses exactly. The cockpit field takes a comma-separated list of IPs.
3. **Logon refusals carry no tag 58.** The crate's `RejectLogon` carries no text. The reason is logged and shown in the cockpit as the strategy's `last_logon_error`.
4. **Sessions come from the registry when the acceptor starts.** QuickFIX has no dynamic sessions. A new strategy, or changed CompIDs or allowlist, takes effect on **Restart FIX acceptor**, a cockpit button that says how many sessions it will log out, or on OMS restart. A password, state or limit change applies at once.
5. **"Connections" are accounts.** Orders route through an account (`account.broker_connection_code`). So `strategy_connection(strategy_code, broker_code, account_id)` maps tag 100 to the account, and the account must belong to the strategy's portfolio.
6. **Flatten's account** for a position is the account of the latest order with fills on that (portfolio, instrument). Positions are not keyed by account.
7. **Reference price** for the band and for market-order notional is the mark's **mid**. The band rejects `|limit − mid| / mid > band%`. Flatten prices sells at `mid × (1 − band)` and buys at `mid × (1 + band)`.

## Review Focus

1. **Arbitrage legs on two brokers in quick succession.** Both resolve to their own accounts and neither waits on the other. Test: `two_legs_resolve_to_their_own_accounts` (Task 2).
2. **A strategy's first order on an instrument nobody holds.** It waits for the first quote, then passes or fails the band. A later sale of a held position must not unsubscribe it. Tests: `an_order_waits_for_the_first_quote`, `a_strategy_interest_survives_the_held_set_dropping_it` (Task 2).
3. **ETHBTC with only strategy-wide defaults:** rejected with `no notional limit for quote currency BTC`. With a per-instrument override it passes. Test: `defaults_do_not_price_a_non_usd_quote` (Task 2).
4. **Session drops mid-burst.** Open non-Flatten orders are cancelled, and on the next Logon the strategy gets the cancels it missed, once each. Tests: `a_dropped_session_cancels_everything_but_flatten_orders` (Task 5), `logon_replays_reports_past_the_cursor_once` (Task 4).
5. **Kill while the strategy is still sending.** An order that arrives after Kill is rejected by the guard (Halted), even if it raced the logout. REST is blocked too. Tests: `a_halted_strategy_admits_nothing`, `a_stopped_strategys_token_is_refused` (Tasks 2 and 5).

---

## File Structure (end state)

```
db/migrations/ods/oms/0035_CREATE_STRATEGY_TABLES.sql   strategy, strategy_connection
db/migrations/ods/oms/0036_ALTER_ORDER_STATE_ADD_ORIGIN.sql  origin, fix_cl_ord_id
crates/gateway/src/md/mod.rs           MdHandle gains owners (interest per owner)
crates/gateway/src/fix_acceptor/mod.rs  Acceptor start/stop, settings, LogonCheck, ClientRequest/ClientReport
crates/gateway/src/fix_acceptor/codec.rs  FIX 4.4 message ⇄ ClientRequest/ClientReport (pure)
crates/gateway/src/fix_acceptor/testdata/  self-signed test cert + key
src/order_entry.rs                     submit/cancel core extracted from handlers.rs
src/strategies/mod.rs                  Strategy, registry loads, RegistrySnapshot (ArcSwap)
src/strategies/admin.rs                /admin/strategies* handlers
src/strategies/guard.rs                strategy rules inside order_entry (pure fns + DB glue)
src/strategies/session.rs              FIX request handling, reports from the order lane, replay
src/strategies/control.rs              kill, enable, flatten, cancel_open, boot recovery
cockpit/src/pages/Strategies.tsx       registry CRUD + Kill/Enable/Flatten/Reset password/Restart acceptor
docs-site/src/content/docs/strategies/{fix.md,rest.md}
```

---

### Task 1: Strategy registry

**Files:**
- Create: `db/migrations/ods/oms/0035_CREATE_STRATEGY_TABLES.sql`, `src/strategies/mod.rs`, `src/strategies/admin.rs`.
- Modify: `src/main.rs` (`mod strategies;`, routes under the admin router beside `/admin/risk-limits`).

**Interfaces:**
- **Produces, the tables:**
  - `strategy`, keyed by natural `code TEXT PRIMARY KEY` (pk convention for config tables). Columns:
    - `name TEXT NOT NULL`;
    - `portfolio_id UUID NOT NULL UNIQUE REFERENCES portfolio(id)`;
    - `owner_principal_id UUID NOT NULL REFERENCES principal(id)`;
    - `principal_id UUID NOT NULL UNIQUE REFERENCES principal(id)`, the service principal used for REST and as the actor of FIX orders;
    - `sender_comp_id TEXT NOT NULL`, `target_comp_id TEXT NOT NULL`, with `UNIQUE (sender_comp_id, target_comp_id)`;
    - `fix_password_hash TEXT` (NULL means FIX logon impossible);
    - `allowed_addresses TEXT[] NOT NULL DEFAULT '{}'`;
    - `state TEXT NOT NULL DEFAULT 'ACTIVE' CHECK (state IN ('ACTIVE','REDUCING','HALTED'))`;
    - `rest_enabled BOOLEAN NOT NULL DEFAULT true`;
    - `max_order_quantity`, `max_order_notional`, `max_position_quantity`, `max_position_notional`, each `NUMERIC`, `> 0` when set;
    - `price_band_pct NUMERIC` (`> 0` when set; NULL means no band);
    - `fix_report_position BIGINT NOT NULL DEFAULT 0`;
    - `last_logon_error TEXT`;
    - `created_at`, `updated_at`.
  - `strategy_connection(strategy_code TEXT REFERENCES strategy(code) ON DELETE CASCADE, broker_code TEXT NOT NULL, account_id UUID NOT NULL REFERENCES account(id), PRIMARY KEY (strategy_code, broker_code))`.
- **Produces, `src/strategies/mod.rs`:**

```rust
pub struct Strategy { pub code: String, pub portfolio_id: Uuid, pub principal_id: Uuid, pub sender_comp_id: String,
    pub target_comp_id: String, pub fix_password_hash: Option<String>, pub allowed_addresses: Vec<String>,
    pub state: TradingState, pub rest_enabled: bool, pub limits: RiskLimits, pub price_band_pct: Option<f64>,
    pub connections: BTreeMap<String /*broker_code*/, Uuid /*account_id*/> }
pub async fn load_all(pool: &PgPool) -> Result<Vec<Strategy>, sqlx::Error>;
pub async fn by_portfolio(conn: &mut PgConnection, portfolio_id: Uuid) -> Result<Option<Strategy>, sqlx::Error>; // FOR UPDATE on the strategy row
pub fn hash_fix_password(plain: &str) -> String;               // bcrypt cost 12, as api keys
pub fn verify_fix_password(plain: &str, hash: &str) -> bool;
pub fn new_fix_password() -> String;                            // 24 url-safe chars
#[derive(Clone, Default)] pub struct RegistrySnapshot(Arc<ArcSwap<Vec<Strategy>>>);
impl RegistrySnapshot { pub async fn reload(&self, pool: &PgPool) -> Result<(), sqlx::Error>; pub fn by_comp_ids(&self, sender: &str, target: &str) -> Option<Strategy>; pub fn by_code(&self, code: &str) -> Option<Strategy>; }
```

- **Produces, the admin routes** (admin auth, like `/admin/risk-limits`):
  - `GET/POST /admin/strategies`;
  - `PUT/DELETE /admin/strategies/:code`;
  - `PUT /admin/strategies/:code/connections` (body `[{broker_code, account_id}]`), which rejects an account whose `portfolio_id` is not the strategy's and a `broker_code` that does not match the account's connection;
  - `POST /admin/strategies/:code/password`, which returns `{ "password": "<plain>" }` once and stores only the hash.

  Every write calls `RegistrySnapshot::reload`. `AppState` holds the snapshot (`strategies()` accessor).

- [ ] **Step 1: Write the failing tests** in `src/strategies/mod.rs`:

```rust
#[test]
fn a_fix_password_verifies_against_its_hash_only() {
    let h = hash_fix_password("s3cret");
    assert!(verify_fix_password("s3cret", &h));
    assert!(!verify_fix_password("s3cre", &h));
    assert!(!h.contains("s3cret"));
}
#[test]
fn snapshot_finds_a_strategy_by_its_comp_ids() {
    // RegistrySnapshot with one strategy XARB1/OMS → by_comp_ids("XARB1","OMS") is Some, ("XARB1","X") is None
}
```

  Add `#[ignore]` DB tests in `src/strategies/admin.rs`:
  - `connections_refuse_an_account_of_another_portfolio`;
  - `password_reset_returns_plaintext_once_and_stores_a_hash`.

- [ ] **Step 2: Run** `cargo test --bin oms strategies::` and expect compile errors.
- [ ] **Step 3: Implement** the migration, the module and the routes.
- [ ] **Step 4: Run** `cargo test --bin oms strategies::` and expect the non-ignored tests to pass. Then run the workspace sum (as in phase 6) and expect `708 0 82`.
- [ ] **Step 5: Commit** with `feat(strategies): strategy registry with FIX logon identity and connections`.

---

### Task 2: One order path with the strategy guard

**Files:**
- Create: `src/order_entry.rs`, `src/strategies/guard.rs`, `db/migrations/ods/oms/0036_ALTER_ORDER_STATE_ADD_ORIGIN.sql`.
- Modify:
  - `src/handlers.rs` (`orders_submit` and `orders_cancel` become thin wrappers);
  - `src/risk_engine.rs` (`check_submit` takes the effective config and a reference price);
  - `crates/gateway/src/md/mod.rs` (owners);
  - `src/market_data.rs` (owner `"held"`).

**Interfaces:**
- **Migration 0036:**
  - `order_state.origin TEXT NOT NULL DEFAULT 'API' CHECK (origin IN ('API','FIX','FLATTEN'))`;
  - `order_state.fix_cl_ord_id TEXT`;
  - `UNIQUE (portfolio_id, fix_cl_ord_id)`.
- **`src/order_entry.rs`:**

```rust
pub enum Origin { Api, Fix { cl_ord_id: String }, Flatten }
pub async fn submit(state: &AppState, auth: &AuthContext, req: SubmitOrderRequest, origin: Origin) -> Result<Uuid, ApiError>;
pub async fn cancel(state: &AppState, actor: &AuthContext, order_id: Uuid) -> Result<(), ApiError>;
```

  The bodies are today's handler bodies, unchanged except:
  - the guard runs in TX1, in place of the plain `RiskEngine::check_submit` call;
  - `origin` / `fix_cl_ord_id` are inserted with the order;
  - a broker reject after TX1 is returned as today. The FIX layer turns it into 35=8 39=8.
- **`MdHandle` gains owners** (gateway). These replace phase 6's `subscribe` / `unsubscribe`:

```rust
pub fn subscribe(&self, owner: &str, instruments: Vec<InstrumentRef>);
pub fn unsubscribe(&self, owner: &str, instrument_ids: &[i64]);
pub fn unsubscribe_owner(&self, owner: &str);
```

  An instrument stays in the watch while any owner holds it. `market_data::run` uses owner `"held"`, and strategies use `"strategy:<code>"`.
- **`src/strategies/guard.rs`** (pure functions, plus the async glue `check`):

```rust
pub const USD_LIKE: [&str; 3] = ["USD", "USDT", "USDC"];
pub const HOLD_FOR_MARK: Duration = Duration::from_secs(2);
pub fn may_trade(strategy: &Strategy, principal_id: Uuid, origin: &Origin) -> Result<(), RiskRejection>; // not the strategy's principal → code "strategy_portfolio": "portfolio is traded by strategy {code}"
pub fn effective_limits(strategy: &Strategy, override_row: Option<RiskLimits>, quote_currency: &str) -> Result<RiskLimits, RiskRejection>;
pub fn check_price_band(limit_price: Option<f64>, mid: f64, band_pct: f64) -> Result<(), RiskRejection>; // code "price_band"
pub enum MarkLookup { Live(Mark), NotCovered, TimedOut }
pub async fn reference_mark(marks: &MarkStore, md: &MdHandle, owner: &str, inst: &InstrumentRef, covered: bool, timeout: Duration) -> MarkLookup;
pub async fn check(state: &AppState, tx: &mut PgConnection, strategy: &Strategy, cmd: &SubmitOrder, inst: &InstrumentRef, contract_size: f64, origin: &Origin) -> Result<(), RiskRejection>;
```

- **What the rules mean:**
  - `effective_limits`: a per-instrument `risk_limits` row wins whole. Otherwise the strategy defaults apply, but a default notional limit on a non-USD-like quote is `Err("no_notional_limit", "no notional limit for quote currency {ccy}")`.
  - **Halted** admits nothing (`trading_halted`).
  - **Reducing** admits only reducing orders (today's `check_trading_state`).
  - **Reference price** is the mark mid. If `NotCovered` (no feed covers the instrument, via `crate::feeds::all()` + `dataprovider::covers`), the band is skipped and a market order's notional checks are skipped, each with a `warn!`.
  - `TimedOut` → `Err("no_reference_price", "no reference price within 2s")`.
- **Routing for FIX/REST with `venue` = broker code:** `order_entry::submit` takes the account from `strategy.connections[venue]` when the portfolio is a strategy's. An unknown broker code gives `Err` 422 `"strategy has no connection for {venue}"`. The symbol is resolved through `broker_instrument (broker_symbol, broker_code)`.

- [ ] **Step 1: Write the failing tests** (pure; in `guard.rs` unless noted):

```rust
#[test] fn a_halted_strategy_admits_nothing() { /* state Halted, a reducing sell → Err code "trading_halted" */ }
#[test] fn the_cockpit_cannot_trade_a_strategy_portfolio() { /* may_trade with another principal → Err "strategy_portfolio" */ }
#[test] fn defaults_do_not_price_a_non_usd_quote() {
    // defaults max_order_notional=10000; quote "BTC", no override → Err message "no notional limit for quote currency BTC"
    // same with an override row → Ok(override)
    // quote "USDT", no override → Ok(defaults)
}
#[test] fn the_band_rejects_a_price_too_far_from_mid() {
    assert!(check_price_band(Some(104.0), 100.0, 5.0).is_ok());
    let e = check_price_band(Some(106.0), 100.0, 5.0).unwrap_err();
    assert_eq!(e.code, "price_band");
    assert!(check_price_band(None, 100.0, 5.0).is_ok()); // market orders have no limit to band
}
#[tokio::test] async fn an_order_waits_for_the_first_quote() {
    // MarkStore empty; spawn: sleep 100ms then marks.set(7, 99.0, 101.0)
    // reference_mark(.., covered=true, 2s) → Live(mid 100)
}
#[tokio::test] async fn no_quote_within_the_timeout_is_timed_out() { /* covered=true, timeout 50ms, no set → TimedOut */ }
#[tokio::test] async fn an_uncovered_instrument_is_not_waited_for() { /* covered=false → NotCovered immediately */ }
```

  In `crates/gateway/src/md/session.rs` tests:

```rust
#[tokio::test] async fn a_strategy_interest_survives_the_held_set_dropping_it() {
    // subscribe("held",[2 ABC X]); subscribe("strategy:s",[2 ABC X]); unsubscribe("held",[2]); end session → reconnect sees {"abc":2}
}
```

  `#[ignore]` DB tests in `src/order_entry.rs`:
  - `two_legs_resolve_to_their_own_accounts`;
  - `a_rest_order_on_a_strategy_portfolio_gets_the_strategy_checks`.

- [ ] **Step 2: Run** `cargo test --bin oms guard; cargo test -p gateway md::session` and expect compile errors.
- [ ] **Step 3: Implement.**
  1. Move the handler bodies into `order_entry` first, with no behaviour change, and run the full suite.
  2. Then add the guard and the owners.
  3. Update phase 6's three session tests to pass owner `"held"`.
- [ ] **Step 4: Run** the same commands and expect PASS. Then run the workspace sum and expect 0 failed, with passed = Task 1 + 8 and ignored + 2.
- [ ] **Step 5: Commit** with `feat(strategies): strategy orders take the one order path with limits, price band and a wait for the first quote`.

---

### Task 3: The FIX acceptor

**Files:**
- Create:
  - `crates/gateway/src/fix_acceptor/mod.rs`;
  - `crates/gateway/src/fix_acceptor/codec.rs`;
  - `crates/gateway/src/fix_acceptor/testdata/{cert.pem,key.pem}`, a self-signed test certificate made with `openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -subj /CN=localhost`;
  - `src/strategies/session.rs`.
- Modify:
  - `crates/gateway/src/lib.rs`;
  - `src/config.rs`: a `[fix_acceptor]` section with `port`, `certificate`, `private_key`, read like `oidc()`; absent means no acceptor;
  - `src/main.rs`: start after `AppState`;
  - `src/strategies/admin.rs`: `POST /admin/fix-acceptor/restart`, which returns `{ "logged_out": n }`.

**Interfaces:**
- **Gateway** (no DB, no FIX tags outside the gateway):

```rust
pub struct SessionKey { pub sender_comp_id: String, pub target_comp_id: String }
pub enum ClientRequest {
    NewOrder { cl_ord_id: String, symbol: String, ex_destination: String, side: Side, qty: f64, ord_type: OrdType, price: Option<f64>, tif: Tif },
    Cancel { cl_ord_id: String, orig_cl_ord_id: String },
    Status { cl_ord_id: String },
    SecurityList { req_id: String },
}
pub enum ClientReport {
    Exec { cl_ord_id: String, order_id: String, exec_id: String, exec_type: ExecType, ord_status: OrdStatus, symbol: String, side: Side,
           order_qty: f64, cum_qty: f64, leaves_qty: f64, avg_px: f64, last_qty: Option<f64>, last_px: Option<f64>, text: Option<String> },
    CancelReject { cl_ord_id: String, orig_cl_ord_id: String, text: String },
    SecurityList { req_id: String, securities: Vec<(String /*ex_destination*/, String /*symbol*/, i64 /*oms id*/)> },
    BusinessReject { ref_msg_type: String, text: String },
}
pub trait LogonCheck: Send + Sync { fn check(&self, key: &SessionKey, password: Option<&str>) -> Result<(), String>; }
pub struct AcceptorConfig { pub port: u16, pub certificate: PathBuf, pub private_key: PathBuf, pub sessions: Vec<(SessionKey, Vec<String> /*allowed addresses*/)> }
pub struct AcceptorHandle { /* … */ }
impl AcceptorHandle {
    pub fn start(cfg: AcceptorConfig, logon: Arc<dyn LogonCheck>) -> Result<(Self, mpsc::Receiver<(SessionKey, Inbound)>), String>;
    pub fn send(&self, key: &SessionKey, report: ClientReport) -> Result<(), String>;
    pub fn logout(&self, key: &SessionKey, reason: &str);
    pub fn stop(self) -> usize; // sessions logged out
}
pub enum Inbound { LoggedOn, LoggedOut, Request(ClientRequest) }
```

- **Settings:**
  - `ConnectionType=acceptor`, `SocketAcceptPort`, `ServerCertificateFile`, `ServerCertificateKeyFile`;
  - `SslSingleThreaded`;
  - `BeginString=FIX.4.4`, `ResetOnLogon=Y`, `UseDataDictionary=N`, `MemoryMessageStoreFactory`;
  - per session `AllowedRemoteAddresses` when the list is non-empty.
- **Logon** goes through `on_msg_from_admin`: 35=A → `LogonCheck::check(key, tag 554)`, and `Err` → `RejectLogon`. QuickFIX runs on its own OS thread, as in `fix/mod.rs::start_session`.
- **The codec** is pure. It maps tags as follows:
  - 11, 41, 55, 100, 54 (1 = buy, 2 = sell), 38, 40 (1 = market, 2 = limit), 44, 59 (0 = day, 1 = GTC, 3 = IOC), 320;
  - outbound 37, 17, 150, 39, 14, 151, 6, 32, 31, 58, 146 group (55, 100, 48 with 22 = 8).

  Any other type → `BusinessReject` (35=j, 372, 380 = 3).
- **OMS `src/strategies/session.rs`:**
  - `StrategyLogon(RegistrySnapshot)` implements `LogonCheck`. It refuses on unknown CompIDs, a missing or wrong password, a Halted state, or a session already live (a `HashSet` of logged-on codes), and writes `last_logon_error` asynchronously.
  - `pub async fn run(state: AppState, acceptor: Arc<AcceptorHandle>, inbound: mpsc::Receiver<(SessionKey, Inbound)>)` handles requests:
    - **NewOrder:** `order_entry::submit` with an `AuthContext` for `strategy.principal_id`, `order_id = Uuid::new_v4()`, `client_order_id = cl_ord_id`, `venue = ex_destination`, `Origin::Fix`. On `Err`, it sends `Exec` with `exec_type=Rejected, ord_status=Rejected, text=Some(message)`.
    - **Cancel:** looks up `(portfolio_id, fix_cl_ord_id = orig_cl_ord_id)`, then `order_entry::cancel`. An unknown order or `Err` sends `CancelReject` with the reason.
    - **Status:** sends the order's current `Exec` with `exec_type=OrderStatus` (150=I).
    - **SecurityList:** every `broker_instrument` row (`is_tradeable`) whose `broker_code` is in `strategy.connections`, on an ACTIVE instrument. This is computed by `pub async fn tradeable(pool, &Strategy) -> Result<Vec<(String, String, i64)>, sqlx::Error>`.
  - **REST twin:** `GET /strategies/securities` (bearer auth) returns the same list as JSON `[{ "venue", "symbol", "instrument_id" }]` for the caller's strategy, and 404 when the caller is not a strategy principal. Add the `#[ignore]` DB test `rest_and_fix_security_lists_agree`.

- [ ] **Step 1: Write the failing tests:**
  - In `crates/gateway/src/fix_acceptor/codec.rs`:
    - `a_new_order_single_decodes_every_field`;
    - `an_unknown_message_type_is_a_business_reject`;
    - `a_rejected_exec_carries_its_reason_in_58` (builds 35=8 with 150=8, 39=8, 58=`max_order_notional: 12000 > 10000`).
  - In `crates/gateway/src/fix_acceptor/mod.rs`:

```rust
/// The phase's "done when", at the protocol level: a QuickFIX initiator logs on over
/// TLS with the right password, sends 35=D and receives the 35=8 the handler sends back.
#[test]
fn a_test_client_logs_on_and_places_an_order() { /* acceptor on 127.0.0.1:free port with testdata cert; LogonCheck accepting "pw";
    initiator (SslSingleThreaded, same cert as CA) sends 35=D 11=c1 55=BTCUSDT 100=BINANCE 54=1 38=1 40=2 44=100 59=1;
    test drains inbound: LoggedOn, Request(NewOrder{cl_ord_id:"c1",..}); acceptor.send(Exec{.. ord_status: New ..});
    initiator's on_msg_from_app receives 35=8 with 11=c1 and 39=0 within 5 s */ }
#[test]
fn a_wrong_password_is_refused() { /* same, LogonCheck returns Err → initiator never sees on_logon within 3 s */ }
```

  - In `src/strategies/session.rs`, unit tests over `StrategyLogon`:
    - `logon_refuses_unknown_comp_ids_a_wrong_password_a_halted_strategy_and_a_second_session`.

- [ ] **Step 2: Run** `cargo test -p gateway fix_acceptor; cargo test --bin oms strategies::session` and expect compile errors.
- [ ] **Step 3: Implement** the interfaces above. If the acceptor's config section is absent, `main.rs` logs `FIX acceptor: not configured` and starts nothing.
- [ ] **Step 4: Run** the same commands and expect PASS. Then run the workspace sum and expect 0 failed.
- [ ] **Step 5: Commit** with `feat(strategies): FIX acceptor for strategies — logon, new order, cancel, status, security list`.

---

### Task 4: Execution reports from the order lane, and replay on Logon

**Files:**
- Modify: `src/strategies/session.rs`.

**Interfaces:**
- **Consumes:** `OrderLaneEvent::Appended { order_id, global_position, event_type }` (`src/event_bus.rs:27`), `order_state`, and `order_event` (for `last_qty` / `last_px` from the fill payload, as `order_events.rs` reads it).
- **Produces:**
  - `fn exec_for(event_type: &str) -> Option<(ExecType, OrdStatus)>`, defined as:
    - `order_routed` → (New, New);
    - `order_partially_filled` → (Trade, PartiallyFilled);
    - `order_filled` → (Trade, Filled);
    - `order_canceled` → (Canceled, Canceled);
    - `order_rejected` → (Rejected, Rejected);
    - `order_expired` → (Expired, Expired);
    - anything else → `None`, with no report.
  - **Live reports:** a task subscribed to the order lane looks up `order_state` (origin `FIX`, `fix_cl_ord_id`, portfolio → strategy). If that strategy is logged on, it sends the report and sets `strategy.fix_report_position = global_position` when that is larger.
  - **Replay:** on `Inbound::LoggedOn`, it sends every event with `global_position > fix_report_position` on that strategy's FIX orders, in order, and advances the cursor.
  - `exec_id = global_position` as a string.

- [ ] **Step 1: Write the failing tests:**

```rust
#[test] fn routed_fill_cancel_reject_map_to_fix_exec_and_status() { /* exec_for table above, plus "order_submitted" → None */ }
#[test] fn logon_replays_reports_past_the_cursor_once() {
    // pure: fn reports_to_replay(cursor: i64, events: &[(i64, &str)]) -> Vec<i64>
    // cursor 10, events [(9,"order_routed"),(11,"order_canceled"),(12,"order_submitted")] → [11]; cursor then 12 → []
}
```

  Add an `#[ignore]` DB test, `a_fill_on_a_fix_order_reaches_the_session_and_advances_the_cursor`.

- [ ] **Step 2: Run** `cargo test --bin oms strategies::session` and expect compile errors.
- [ ] **Step 3: Implement.** Laggard rule (parent §4.1): on `RecvError::Lagged`, replay from the cursor for every logged-on strategy instead of dropping reports.
- [ ] **Step 4: Run** and expect PASS. Then run the workspace sum and expect 0 failed.
- [ ] **Step 5: Commit** with `feat(strategies): execution reports to strategies from the order lane, replayed on logon`.

---

### Task 5: Kill, Enable, Flatten, disconnect and restart cancels, REST gate

**Files:**
- Create: `src/strategies/control.rs`.
- Modify:
  - `src/strategies/session.rs` (`LoggedOut` → cancel; `unsubscribe_owner`);
  - `src/strategies/admin.rs` (`POST /admin/strategies/:code/{kill,enable,flatten}`);
  - `src/auth.rs` (the REST gate);
  - `src/main.rs` (boot recovery after the registry and adapters are up).

**Interfaces:**

```rust
pub struct ControlReport { pub cancelled: usize, pub cancel_failures: Vec<String>, pub flatten_orders: usize, pub skipped: Vec<String> }
pub async fn cancel_open(state: &AppState, strategy: &Strategy, keep_flatten: bool) -> ControlReport;
pub async fn kill(state: &AppState, code: &str) -> Result<ControlReport, ApiError>;   // state=HALTED, rest_enabled=false, cancel_open(keep_flatten=false), acceptor.logout(key, "killed")
pub async fn enable(state: &AppState, code: &str) -> Result<(), ApiError>;             // state=ACTIVE, rest_enabled=true
pub async fn flatten(state: &AppState, code: &str) -> Result<ControlReport, ApiError>; // as kill but state=REDUCING; then one Origin::Flatten GTC limit per non-zero position
pub async fn on_session_end(state: &AppState, code: &str) -> ControlReport;            // cancel_open(keep_flatten=true); md.unsubscribe_owner("strategy:<code>")
pub async fn boot_recovery(state: &AppState) -> Vec<(String, ControlReport)>;           // every strategy: cancel_open(keep_flatten=true)
pub fn flatten_order(net_qty: f64, mid: f64, band_pct: f64) -> (OrderSide, f64 /*qty*/, f64 /*limit*/);
```

- **Who cancels:** cancels go through `order_entry::cancel` with an `AuthContext` for the strategy's principal, so the order engine books them like any other cancel. They never depend on the strategy.
- **Which positions Flatten closes:** positions come from `position WHERE portfolio_id = $1 AND net_qty <> 0`. The account is the latest order with `cum_qty > 0` on that instrument in the portfolio. A missing mark after `reference_mark` is skipped with `"{symbol}: no reference price"`. With no `price_band_pct`, the position is skipped with `"{symbol}: no price band set"`.
- **REST gate:** when an API key's principal is a strategy principal with `rest_enabled = false`, the request is refused with `403 "strategy {code} is stopped"`.

- [ ] **Step 1: Write the failing tests:**

```rust
#[test] fn flatten_sells_a_long_at_the_lower_band_edge() { assert_eq!(flatten_order(2.0, 100.0, 5.0), (OrderSide::Sell, 2.0, 95.0)); }
#[test] fn flatten_buys_a_short_at_the_upper_band_edge() { assert_eq!(flatten_order(-3.0, 100.0, 5.0), (OrderSide::Buy, 3.0, 105.0)); }
```

  Add `#[ignore]` DB tests:
  - `a_dropped_session_cancels_everything_but_flatten_orders`;
  - `kill_halts_cancels_and_logs_out`;
  - `a_stopped_strategys_token_is_refused`;
  - `boot_recovery_cancels_open_strategy_orders`.

- [ ] **Step 2: Run** `cargo test --bin oms strategies::control` and expect compile errors.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** and expect PASS. Then run the workspace sum and expect 0 failed.
- [ ] **Step 5: Commit** with `feat(strategies): kill, enable and flatten; cancel on disconnect and at boot; stopped strategies lose REST`.

---

### Task 6: Cockpit page, docs, and the end-to-end check

**Files:**
- Create: `cockpit/src/pages/Strategies.tsx`, `cockpit/src/strategies.ts` plus `cockpit/src/strategies.test.ts` (pure helpers), `docs-site/src/content/docs/strategies/fix.md`, `docs-site/src/content/docs/strategies/rest.md`.
- Modify: `cockpit/src/App.tsx` (route `/strategies` and nav entry), and the docs-site sidebar config where integrations are listed.

**Interfaces:**
- **Page:** `CrudResource` on `/admin/strategies`, with these columns:
  - code, name, state, REST, session (logged on / down, from `GET /admin/strategies` field `session_live: bool`, which Task 3's live set provides);
  - max order notional, price band;
  - last logon error.

  `rowAction` has these buttons:
  - **Kill** (red) and **Enable**;
  - **Flatten**, which opens a confirm modal naming the positions it will close and then shows the `ControlReport`;
  - **Reset password**, which shows the new password once, in a modal with a copy button;
  - **Connections**, an editor over `PUT …/connections`.

  A page-level **Restart FIX acceptor** button opens a confirm modal: "logs out N sessions". Per-instrument overrides stay on the existing Risk limits page (portfolio = the strategy's).
- **`cockpit/src/strategies.ts`:**
  - `export function killLabel(state: string): "Kill" | "Enable"`;
  - `export function reportSummary(r: ControlReport): string`, for example `"cancelled 3, flatten orders 2, skipped: ETHBTC: no reference price"`.
- **Docs pages:**
  - **FIX:** session setup (TLS, CompIDs, password in 554, IP allowlist), every supported message with its tags, reject reasons, ClOrdID uniqueness, Kill / disconnect behaviour, replay on Logon. A worked 35=D example.
  - **REST:** trading token, `POST /orders/submit` with `venue` = broker code and the broker symbol, the same checks, Kill disabling the token.

- [ ] **Step 1: Write the failing tests** in `cockpit/src/strategies.test.ts`, covering `killLabel` (HALTED → "Enable", ACTIVE and REDUCING → "Kill") and `reportSummary`.
- [ ] **Step 2: Run** `npm test` in `cockpit/` and expect it to fail (module missing).
- [ ] **Step 3: Implement** the page, the helpers and the docs.
- [ ] **Step 4: Run** `npm test` and `npm run build` in `cockpit/`, and expect PASS. Then run the workspace sum and expect 0 failed.
- [ ] **Step 5: End-to-end check against the dev OMS** (~15 min).
  1. Add `[fix_acceptor]` to the dev config with port 9878 and a local self-signed certificate. Ask the user before editing their config file.
  2. In the cockpit, create strategy `e2e`:
     - a new portfolio, a service principal, CompIDs `E2E1/OMS`, a reset password;
     - a connection to the Binance paper account;
     - `max_order_notional 1000`, band 5%.
  3. Restart the FIX acceptor.
  4. Run the test client: `cargo run --example fix_client -- 127.0.0.1:9878 E2E1 OMS <password> BTCUSDT BINANCE` (a new example in `crates/gateway/examples/fix_client.rs`). It sends 35=D for a 0.0001 BTC limit buy at mid − 1%.

  Expected:
  - the client prints 35=8 with 39=0;
  - the order is in the blotter under the `e2e` portfolio.

  Then:
  1. Send one order above the notional limit, and expect 35=8 39=8 with `58=max_order_notional…`.
  2. Kill in the cockpit. Expect the client to see the cancel and a logout, and its re-logon to be refused.
- [ ] **Step 6: Release link check.** Run `OMS_OPENSSL_STATIC=1 cargo build --release && ! otool -L target/release/oms | grep -qi 'homebrew\|openssl' && echo STATIC_OK` and expect `STATIC_OK`.
- [ ] **Step 7: Commit** with `feat(cockpit): strategies page with kill, enable, flatten; FIX and REST docs for strategy developers`.
