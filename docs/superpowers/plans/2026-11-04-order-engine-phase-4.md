# Inbound Contract + Order Engine (Phase 4) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Connectors (Alpaca, Binance, Bybit, Coinbase, OKX streams and the FIX sessions) stop booking into the database. They send execution events to one order engine, which is the only place that books. Recon goes through the same engine.

**Architecture:**
- **The contract:** `src/order_engine.rs` defines it: `ExecEvent` (one order's fill, cumulative progress, terminal state, cancel or reject) and `GatewayMsg::{Exec, Reconcile}`.
- **Connectors** hold an `EngineHandle` (a bounded `mpsc::Sender<GatewayMsg>`) instead of a `PgPool` and an `EventBus`.
- **One engine task** consumes the channel in order:
  - for each `ExecEvent` it reads the OMS's booked state when needed and turns the event into `ExecutionReport`s with one pure function (`reports_for`), then applies them through the existing `process_execution_report`;
  - a `Reconcile` spawns the existing snapshot diff (`run_order_reconcile`) on its own task, so a slow broker REST call never holds up fills.
- **Consolidation:** today's three booking helpers (`apply_broker_progress`, `apply_broker_terminal`, `apply_terminal`) collapse into that one path.

**Tech Stack:** Rust, tokio `mpsc`, sqlx/Postgres, existing `execution.rs` / `recon_orders.rs`.

**Spec:** `docs/superpowers/specs/2026-11-04-target-architecture-design.md` on branch `docs/target-architecture`: §4.1 (order engine row), §4.3 (contracts), §7 phase 4. Success check: "No connector file references `PgPool` or Kafka".

## Global Constraints

- **Branch:** `feat/order-engine`, created from `feat/ui-updates`.
- **Connector files** (the success check runs on these): `src/alpaca_stream.rs`, `src/binance_stream.rs`, `src/bybit_stream.rs`, `src/coinbase_stream.rs`, `src/okx_stream.rs`, and `src/fix/*.rs`. After this phase none of them mentions `PgPool`, `sqlx`, `kafka`, `process_execution_report`, `apply_broker_*` or `EventBus`.
- **Booking semantics must not change.** Same events, same dedupe guards (the terminal-state skip, the version lock, and "a cumulative update books only `cum − booked`"), same prices (`increment_price`). Existing recon and execution tests keep passing unchanged.
- **One consumer, in order:** the engine applies `Exec` messages in arrival order, which preserves each stream's per-order order. Only `Reconcile` runs concurrently: on a spawned task, and at most one in flight per `broker_code`. A `Reconcile` for a code already running is dropped with a `debug!`; the next 30s heartbeat or reconnect brings another.
- **Channel:** bounded at `ENGINE_CHANNEL_CAPACITY = 4096`. Connectors `send().await`, so a stalled engine slows streams instead of dropping fills. Book or block, never lose.
- **Naming, my call:** the spec names `ExecEvent` and `OrderSnapshot`. The snapshot stays inside the engine's reconcile, because the engine asks the adapter for it. A connector sends `GatewayMsg::Reconcile { broker_code, actor, adapter }` instead of a pre-fetched snapshot: the diff needs further `order_status` calls per resolved order, so a static snapshot would not be enough.
- **Spec field `source: Session|DropCopy|Snapshot`:** add `ExecSource { Session, Snapshot }` and log it. `DropCopy` waits for the first connector that has one (exchanges, phase 6+). YAGNI.
- **DB tests:** `#[ignore = "needs a live Postgres; run with --ignored"]`, dev DB, never create databases. Reuse `execution::tests::{seeded_order, remove_seeded}`.
- **Don't commit the user's files.** Stage explicitly.

## Review Focus

1. **The OMS boots while a broker is mid-fill.** Streams start before or after the engine task, and no event is lost: the channel buffers until the engine runs. Covered by Task 2 `messages_sent_before_the_engine_starts_are_applied`.
2. **A 30s FIX heartbeat recon that takes longer than 30s** (a slow broker REST call) must not pile up concurrent recons for the same broker. Covered by Task 2 `second_reconcile_for_a_busy_broker_is_dropped`.
3. **Coinbase/OKX cumulative updates repeated after a reconnect** must book nothing twice. Covered by Task 1 `repeated_cumulative_update_books_nothing` and Task 2 DB `cumulative_update_twice_books_once`.
4. **A partial fill followed by a venue cancel** books the missing quantity first, then the cancel. Covered by Task 1 `terminal_cancel_books_missing_fill_first`.
5. **A regression: someone adds a DB call to a connector later.** A test fails. Covered by Task 5 `connectors_do_not_touch_the_database`.

---

### Task 1: The contract and the pure booking plan

**Files:**
- Create: `src/order_engine.rs`; add `mod order_engine;` in `src/main.rs`.
- Modify: `src/recon_orders.rs` (make `missing_qty`, `increment_price`, `BrokerOrderState`, `BrokerOrderOutcome` usable from `order_engine`; they already are `pub(crate)`/`pub`).

**Interfaces:**
- Produces:
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq)] pub enum ExecSource { Session, Snapshot }
  #[derive(Debug, Clone, PartialEq)]
  pub enum ExecKind {
      /// One fill with the venue's execution id (Alpaca, Binance, Bybit, FIX).
      Fill { execution_id: String, qty: f64, price: f64 },
      /// Cumulative order state; books whatever exceeds what the OMS has (Coinbase, OKX).
      Cumulative { cum_qty: f64, avg_px: f64 },
      /// The order is closed at the venue, with its final cumulative state.
      Terminal(BrokerOrderState),
      Canceled { reason: Option<String> },
      Rejected { reason: String },
  }
  #[derive(Debug, Clone, PartialEq)]
  pub struct ExecEvent { pub order_id: Uuid, pub venue: String, pub actor: &'static str, pub source: ExecSource, pub kind: ExecKind }

  pub enum GatewayMsg {
      Exec(ExecEvent),
      Reconcile { broker_code: &'static str, actor: &'static str, adapter: Arc<dyn BrokerAdapter> },
  }

  /// What the engine applies for one event, given what the OMS has booked.
  pub fn reports_for(ev: &ExecEvent, booked_cum: f64, booked_avg: Option<f64>) -> Vec<ExecutionReport>;
  /// Whether `reports_for` needs the booked state (Cumulative, Terminal) or not.
  pub fn needs_booked_state(kind: &ExecKind) -> bool;
  ```
- Rules, the same as today's helpers:
  - `Fill` → one `ExecutionReport::Fill { execution_id, fill_qty: qty, fill_price: price, venue }`.
  - `Cumulative` → a fill of `missing_qty(booked, cum)`, priced with `increment_price`, `execution_id = "{venue}-{order_id}-{cum_qty}"`; nothing when nothing is missing.
  - `Terminal` → first the missing fill (`execution_id = "recon-{order_id}"`), then `Canceled`/`Reject{"rejected"}` per outcome. Nothing extra for `Filled`; `Working` → `[]`.
  - `Canceled`/`Rejected` → that one report.

- [ ] **Step 1: Write the failing unit tests** in `order_engine.rs` `mod tests`:
  - `fill_becomes_one_fill_report`.
  - `cumulative_books_only_the_missing_quantity`: booked 0.5 @ 100, cum 1.0 @ 105 → one fill of 0.5 @ 110.
  - `repeated_cumulative_update_books_nothing`: booked 1.0, cum 1.0 → `[]`.
  - `terminal_cancel_books_missing_fill_first`: booked 0.2, terminal Canceled executed 0.5 @ 10 → `[Fill 0.3, Canceled]`, in that order.
  - `terminal_filled_with_nothing_missing_is_empty`, `terminal_working_is_empty`.
  - `needs_booked_state_only_for_cumulative_and_terminal`.
- [ ] **Step 2: Run, expect FAIL.** `cargo test --bin oms order_engine` fails: `ExecEvent` not found.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run.** `cargo test --bin oms order_engine` is green.
- [ ] **Step 5: Commit** `src/order_engine.rs`, `src/main.rs` and `src/recon_orders.rs`: `feat(engine): inbound execution contract and pure booking plan`.

### Task 2: The engine task, and recon through it

**Files:**
- Modify: `src/order_engine.rs` (`EngineHandle`, `run`, `apply_exec`)
- Modify: `src/recon_orders.rs`:
  - `apply_terminal` builds `ExecEvent{kind: Terminal, source: Snapshot}` and goes through `apply_exec`;
  - delete `apply_broker_terminal` and `apply_broker_progress` once Task 3 no longer calls them (Task 3 deletes them; here they become thin wrappers over `apply_exec` so nothing breaks in between).
- Modify: `src/main.rs`: create the channel and spawn `order_engine::run` before any stream starts. Store the `EngineHandle` in `RegistrationDeps.engine` and on `AppState` (`engine()`), so reload can hand it out.

**Interfaces:**
- Consumes: Task 1.
- Produces:
  ```rust
  pub const ENGINE_CHANNEL_CAPACITY: usize = 4096;
  #[derive(Clone)] pub struct EngineHandle(mpsc::Sender<GatewayMsg>);
  impl EngineHandle {
      pub fn new() -> (Self, mpsc::Receiver<GatewayMsg>);
      pub async fn exec(&self, ev: ExecEvent);   // logs at error! if the engine is gone
      pub async fn reconcile(&self, broker_code: &'static str, actor: &'static str, adapter: Arc<dyn BrokerAdapter>);
  }
  /// The only place that books: applies Exec in order; Reconcile on its own task, one per broker_code.
  pub async fn run(rx: mpsc::Receiver<GatewayMsg>, pool: PgPool, bus: EventBus);
  /// Read booked state if needed, apply each of `reports_for(..)` via `process_execution_report`, log failures.
  pub async fn apply_exec(pool: &PgPool, bus: &EventBus, ev: &ExecEvent);
  ```
  - `run` keeps `Arc<Mutex<HashSet<&'static str>>>` of busy broker codes. The spawned recon removes its code when done, guaranteed by a drop guard.

- [ ] **Step 1: Write the failing tests:**
  - (unit) `second_reconcile_for_a_busy_broker_is_dropped`:
    1. Use a fake `BrokerAdapter` whose `open_orders` waits on a `Notify` and counts calls.
    2. Send `Reconcile("FAKE")` twice, then release.
    3. `open_orders` was called once.
    4. A third `Reconcile` after completion calls it again (2 total).
    5. Use a `connect_lazy` pool. `run_order_reconcile` with an empty OMS working set must not touch the DB. If the working-set query is reached, it logs and returns empty, which is acceptable; assert only the call counts.
  - (DB, ignored) `messages_sent_before_the_engine_starts_are_applied`:
    1. `EngineHandle::new()`.
    2. Send a `Fill` for a seeded order (`seeded_order(&pool, 0)`).
    3. Then spawn `run`.
    4. Within 2s, `order_state.status = 'filled'` (qty 1).
    5. Clean up with `remove_seeded`.
  - (DB, ignored) `cumulative_update_twice_books_once`: on a seeded order with `original_qty` 1, send `Cumulative{0.5 @ 100}` twice. Booked `cum_qty` is 0.5 and there is exactly one `order_partially_filled` event.
- [ ] **Step 2: Run, expect FAIL.** `cargo test --bin oms order_engine` fails: `EngineHandle` not found.
- [ ] **Step 3: Implement**, then wire `main.rs`, `AppState`, `RegistrationDeps` and the recon change.
- [ ] **Step 4: Run.** `cargo test --bin oms` is green, plus `cargo test --bin oms -- --ignored order_engine execution::tests`. The existing `recon_orders` tests are unchanged and green.
- [ ] **Step 5: Commit**: `feat(engine): one order-engine task books every execution event; recon goes through it`.

### Task 3: WebSocket and REST streams emit events

**Files:**
- Modify:
  - `src/alpaca_stream.rs` (`handle_trade_update` → `engine.exec(Fill|Canceled|Rejected)`; reconnect recon → `engine.reconcile`);
  - `src/binance_stream.rs` (the same; `handle_execution_report`);
  - `src/bybit_stream.rs` (execution → `Fill`, order close → `Terminal`);
  - `src/coinbase_stream.rs` and `src/okx_stream.rs` (progress → `Cumulative`, close → `Terminal`).
- Modify: `src/recon_orders.rs` (delete `apply_broker_progress` and `apply_broker_terminal`; `oms_fill_state` moves to `order_engine`).
- Modify: `src/reload.rs` (stream restarts pass `deps.engine.clone()`), `src/main.rs` (Binance boot spawn), and the stream tests.

**Interfaces:**
- Consumes: Task 2 `EngineHandle`.
- Each stream's session struct and `run(..)` replace `pool: PgPool` and `bus: Option<EventBus>` with `engine: EngineHandle`. Recon calls become `engine.reconcile(VENUE, actor, adapter.clone())`, where the adapter is coerced to `Arc<dyn BrokerAdapter>`.

- [ ] **Step 1: Add one test per stream file** that drives that stream's pure frame handler with a recorded sample frame and asserts the `GatewayMsg` it emits, using `let (engine, mut rx) = EngineHandle::new()`:
  - `alpaca_fill_frame_becomes_fill_event`;
  - `binance_trade_frame_becomes_fill_event`;
  - `bybit_closed_order_becomes_terminal_event`;
  - `coinbase_progress_becomes_cumulative_event`;
  - `okx_filled_order_becomes_terminal_event`.

  Where a file already has a parse test with a sample frame, reuse that frame. Where the handler is not pure today, the test targets the existing `handle_*` function with the new `engine` argument.
- [ ] **Step 2: Run, expect FAIL** (compile errors: `engine` argument missing).
- [ ] **Step 3: Migrate the five streams.**
- [ ] **Step 4: Run.**
  - `cargo test --bin oms` is green.
  - `grep -n "PgPool\|sqlx\|process_execution_report\|apply_broker\|EventBus" src/{alpaca,binance,bybit,coinbase,okx}_stream.rs` returns nothing.
- [ ] **Step 5: Commit**: `refactor(streams): emit execution events to the order engine instead of booking`.

### Task 4: FIX sessions emit events

**Files:**
- Modify: `src/fix/mod.rs`:
  - the report drain forwards `(order_id, report)` as `ExecEvent{ kind: from_report(report), source: Session }`;
  - the recon driver calls `engine.reconcile(broker_code, actor, recon_adapter.clone())`;
  - `start_ibkr` and `start_binance` take `engine: EngineHandle` instead of `pool, bus`.
- Modify: `src/reload.rs` (the FIX starts) and the `fix/mod.rs` tests (`lazy_pool()` → `EngineHandle::new().0`).
- Modify: `src/order_engine.rs`: `pub fn kind_from_report(r: ExecutionReport) -> ExecKind` for transports that already speak `ExecutionReport`.

- [ ] **Step 1: Write the failing test** `report_maps_to_kind` in `order_engine.rs`. `Fill`, `Canceled` and `Reject` map to `Fill`, `Canceled` and `Rejected`, with the fields carried over.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement and migrate.** If `RegistrationDeps.pool` has no reader left, delete it; otherwise keep it.
- [ ] **Step 4: Run.** `cargo test --bin oms` is green, and `grep -rn "PgPool\|sqlx\|process_execution_report\|EventBus" src/fix` returns nothing.
- [ ] **Step 5: Commit**: `refactor(fix): FIX sessions emit execution events to the order engine`.

### Task 5: Guard the boundary; manual check

**Files:**
- Modify: `src/order_engine.rs` tests.

- [ ] **Step 1: Write the test** `connectors_do_not_touch_the_database`.
  - `include_str!` each connector file listed in Global Constraints. For `src/fix`, include each `.rs` in the directory by name.
  - Assert none contains `PgPool`, `sqlx::`, `KafkaClient`, `process_execution_report`, `apply_broker_` or `EventBus`.
  - It must pass now. Mutation check: temporarily add `use sqlx::PgPool;` to `okx_stream.rs`; the test fails. Then revert.
- [ ] **Step 2: Run.** `cargo test --bin oms` is green.
- [ ] **Step 3: Manual check.**
  1. Restart the dev OMS.
  2. The log shows `order engine: started` before the first `connecting stream=` line.
  3. All execution streams connect.
  4. `grep -c "recon" /tmp/oms-server.log` keeps growing every 30s for FIX with no overlap warnings.
  5. A paper order placed from the cockpit fills, and the blotter updates live (phase 3).
- [ ] **Step 4: Commit**: `test(engine): connectors may not reach the database`.
