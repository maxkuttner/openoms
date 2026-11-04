# Event Bus (Phase 1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the in-process two-lane event bus, publish committed order events and position changes on the order lane and accepted marks on the price lane, and replace the per-feed "position changed" doorbells with bus subscriptions.

**Architecture:**
- `src/event_bus.rs` holds a cloneable `EventBus` with two `tokio::sync::broadcast` lanes.
- **Order lane:** every place that commits order events publishes `OrderLaneEvent::Appended` after `tx.commit()`; the fill path also publishes `PositionChanged`.
- **Price lane:** the mark router publishes `MarkUpdate` after `marks.set`.
- **Quote feeds:** they subscribe to the order lane instead of their own doorbell channel. `DoorbellRegistry`, the per-feed `mpsc<()>` pairs and the fan-out task are deleted.
- **Positions:** they stay inside the fill transaction (spec §6); only the notification moves to the bus.

**Tech Stack:** Rust, tokio (`broadcast`), sqlx/Postgres, existing `stream_supervisor`.

**Spec:** `docs/superpowers/specs/2026-11-04-target-architecture-design.md`, on branch `docs/target-architecture` (§4.1 "Event bus lanes", §6 "My calls", §7 phase 1).

## Global Constraints

- **Branch:** work on `feat/event-bus`, created from `feat/okx-spot` (not `main`), because every stream this plan touches (Coinbase, OKX) exists only there. That makes the merge order `feat/coinbase-spot` → `feat/okx-spot` → `feat/event-bus`.
- **Order lane:** an event is published only after its transaction commits. Never publish inside a transaction or before `commit()`.
- **Never block the fill path:** publishing uses `broadcast::Sender::send` and ignores the "no receivers" error.
- **Price lane:** latest wins. A subscriber that lags skips ahead (`RecvError::Lagged` → continue). The price lane must never be awaited by a producer.
- **Positions** stay synchronous in the fill transaction (`positions::persist_fill`, `src/execution.rs:213`). Do not move them to a subscriber.
- **DB tests:** mark them `#[ignore = "needs a live Postgres; run with --ignored"]` and run them against the shared dev DB at 100.80.233.3. Never create databases on it.
- **Don't commit the user's files:** `.gitignore`, `.ignore` and `docs/superpowers/plans/2026-09-28-options-search-chain-strategy.md` stay untouched.
- **No compatibility shims:** the OMS is single-user and pre-release, so delete the doorbell types outright.
- **Lane capacities (exact values):** order lane 1024, price lane 4096.
- **Out of scope:** store-backed catch-up for order-lane laggards (re-reading `order_event` by `global_position`). That is phase 2, where the Kafka bridge needs it. In phase 1 the only order-lane subscribers are quote feeds, and for them a lag only means "re-check the held set".
- **Spec reconciliation:** spec §7 phase 1 says "positions subscribe". Spec §6 overrides it: positions stay in the fill transaction, and the bus carries `PositionChanged`. This plan follows §6.

## Review Focus

1. **A fill booked while no feed is running yet (boot order) must not error or block.** Publishing with zero subscribers is a no-op. Covered by Task 1 `publish_without_subscribers_is_a_no_op`.
2. **A feed that falls behind on the order lane (`Lagged`) must still pick up newly held instruments.** It treats `Lagged` as a re-check, not as an error or a session end. Covered by Task 4 `lagged_order_lane_means_recheck`.
3. **A rejected or concurrency-retried append must publish nothing.** Only a committed append publishes. Covered by Task 3 `failed_append_publishes_nothing` (DB, ignored).
4. **A feed restarted by a credential reload must subscribe to the same bus.** Today it gets a fresh doorbell sender; after the change it is a fresh `subscribe_order()` on the one `AppState` bus. Covered by Task 4's `restart_databento_feed` signature change, plus the existing `reload_tests.rs` passing.
5. **A burst of quotes must not delay `Appended`/`PositionChanged` delivery.** The two lanes are separate channels. Covered by Task 1 `price_burst_does_not_touch_order_lane`.

---

### Task 1: `EventBus` module

**Files:**
- Create: `src/event_bus.rs`
- Modify: `src/main.rs` (add `mod event_bus;`)

**Interfaces:**
- Produces:
  ```rust
  pub const ORDER_LANE_CAPACITY: usize = 1024;
  pub const PRICE_LANE_CAPACITY: usize = 4096;

  #[derive(Debug, Clone, PartialEq)]
  pub enum OrderLaneEvent {
      /// One committed order event (published after commit, in append order).
      Appended { order_id: Uuid, global_position: i64, event_type: String },
      /// A committed fill changed a position.
      PositionChanged { portfolio_id: Uuid, instrument_id: String },
  }

  #[derive(Debug, Clone, Copy, PartialEq)]
  pub struct MarkUpdate { pub instrument_id: i64, pub bid: f64, pub ask: f64 }
  impl From<&dataprovider::Quote> for MarkUpdate { /* copies instrument_id, bid, ask */ }

  #[derive(Clone)]
  pub struct EventBus { /* two broadcast::Sender */ }
  impl EventBus {
      pub fn new() -> Self;
      pub fn publish_order(&self, ev: OrderLaneEvent);   // ignores "no receivers"
      pub fn publish_mark(&self, m: MarkUpdate);          // ignores "no receivers"
      pub fn subscribe_order(&self) -> broadcast::Receiver<OrderLaneEvent>;
      pub fn subscribe_price(&self) -> broadcast::Receiver<MarkUpdate>;
  }
  impl Default for EventBus
  ```

- [ ] **Step 1: Write the failing tests** in `src/event_bus.rs` `mod tests`:
  - `publish_without_subscribers_is_a_no_op`: `EventBus::new().publish_order(..)` and `.publish_mark(..)` return without panicking.
  - `order_lane_delivers_in_publish_order`: subscribe, then publish `Appended` with `global_position` 1, 2, 3. `try_recv` yields 1, 2, 3 in order.
  - `price_lane_lagging_subscriber_skips_to_newest`: subscribe, then publish 4097 marks with `bid = i as f64`. The first `try_recv` is `Err(TryRecvError::Lagged(_))`. Draining after it ends with `bid == 4096.0`.
  - `price_burst_does_not_touch_order_lane`: subscribe to both lanes and publish 5000 marks. `order_rx.try_recv()` is `Err(TryRecvError::Empty)`.
  - `mark_update_copies_quote_prices`: `MarkUpdate::from(&quote)` has the quote's `instrument_id`, `bid` and `ask`.
- [ ] **Step 2: Run, expect a compile failure.** `cargo test --bin oms event_bus 2>&1 | tail -5` gives FAIL: `EventBus` not found.
- [ ] **Step 3: Implement** the Interfaces block. `new()` builds each lane with `broadcast::channel(CAPACITY)` and drops the initial receiver.
- [ ] **Step 4: Run.** `cargo test --bin oms event_bus` passes 5/5.
- [ ] **Step 5: Commit.** `git add src/event_bus.rs src/main.rs && git commit -m "feat(bus): in-process event bus with order and price lanes"`

### Task 2: The event store reports global positions

**Files:**
- Modify: `src/event_store.rs`: `AppendResult` (line 38) and the INSERT in `append_events_in_tx`
- Test: `src/event_store.rs` `mod tests` (ignored DB test)

**Interfaces:**
- Produces: `AppendResult { last_version: i64, appended: usize, global_positions: Vec<i64> }`. Positions follow the order of `events`; the vector is empty when `events` is empty.

- [ ] **Step 1: Write the failing test** `append_returns_global_positions_in_order` (ignored, DB). Append 2 events to a fresh `Uuid::new_v4()` stream. Then:
  - `r.global_positions.len() == 2`;
  - `r.global_positions[0] < r.global_positions[1]`;
  - both equal the `global_position`s that `load_stream` returns, in version order.
- [ ] **Step 2: Run, expect a compile failure.** `cargo test --bin oms event_store -- --ignored append_returns_global_positions 2>&1 | tail -5` gives FAIL: no field `global_positions`.
- [ ] **Step 3: Implement.** Add `RETURNING global_position` to the INSERT, switch to `fetch_one` and collect the values. The early-return branches set `global_positions: Vec::new()`.
- [ ] **Step 4: Run** the test with `--ignored`; expect a PASS. `cargo test --bin oms` stays green.
- [ ] **Step 5: Commit.** `git commit -am "feat(event-store): return global positions from append"`

### Task 3: Publish on the order lane after commit; the fill path carries the bus

**Files:**
- Modify: `src/app_state.rs`:
  - add an `event_bus: EventBus` field and `pub fn event_bus(&self) -> &EventBus`;
  - remove `position_changed_tx` and its getter (line 248).
- Modify: `src/execution.rs`:
  - change `process_execution_report(.., position_changed_tx: Option<&mpsc::Sender<()>>)` to `bus: Option<&EventBus>`;
  - `apply_once` gains `bus: Option<&EventBus>`.
- Modify: `src/handlers.rs`, at the three commit sites (around lines 684, 902 and 1198): publish after commit.
- Modify (mechanical rename `position_changed_tx: Option<&mpsc::Sender<()>>` → `bus: Option<&EventBus>`, and owned `Option<mpsc::Sender<()>>` → `Option<EventBus>`):
  - `src/recon_orders.rs` (`run_order_reconcile`, `apply_broker_terminal`, `apply_broker_progress`, `apply_terminal`);
  - `src/alpaca_stream.rs`, `src/binance_stream.rs`, `src/bybit_stream.rs`, `src/coinbase_stream.rs`, `src/okx_stream.rs`;
  - `src/fix/mod.rs`;
  - `src/reload.rs` (`RegistrationDeps.position_changed_tx` → `event_bus: EventBus`);
  - `src/admin.rs`, `src/main.rs`.
- Test: `src/execution.rs` `mod tests` (ignored DB tests)

**Interfaces:**
- Consumes: Task 1 `EventBus`, `OrderLaneEvent`; Task 2 `AppendResult.global_positions`.
- Produces:
  - `pub fn publish_appended(bus: &EventBus, order_id: Uuid, events: &[NewOrderEvent], result: &AppendResult)` in `src/event_bus.rs`. It publishes one `Appended` per event, zipping `events[i].event_type` with `result.global_positions[i]`.
  - `AppState::event_bus()`.
  - `RegistrationDeps { .., event_bus: EventBus }`.

- [ ] **Step 1: Write the failing tests:**
  - (unit, `event_bus.rs`) `publish_appended_pairs_types_with_positions`: 2 `NewOrderEvent`s plus an `AppendResult{global_positions: vec![10, 11], ..}` yield two `Appended` with positions 10 and 11 and the matching `event_type`s.
  - (DB, ignored, `execution.rs`) `fill_publishes_appended_then_position_changed`:
    1. seed a routed order (reuse the existing execution test fixtures if present; otherwise insert via `handlers` helpers);
    2. subscribe;
    3. call `process_execution_report` with a `Fill`.

    The received events are ≥1 `Appended`, then exactly one `PositionChanged{portfolio_id, instrument_id}` matching the order. Nothing is received before the call returns `Ok`.
  - (DB, ignored) `failed_append_publishes_nothing`: call `apply_once` with a stale `expected_version` (forces `ConcurrencyRetry`). `try_recv` is `Empty`.
- [ ] **Step 2: Run, expect FAIL.** `cargo test --bin oms publish_appended` fails: `publish_appended` not found.
- [ ] **Step 3: Implement:**
  - In `apply_once`, after `tx.commit()` (`src/execution.rs:255`): call `publish_appended`. If any event was a fill, publish one `PositionChanged { portfolio_id: portfolio_uuid, instrument_id: new_state.instrument_id.clone() }`.
  - In `process_execution_report`, delete the doorbell `try_send` block (lines 53–60).
  - At each `handlers.rs` site, call `publish_appended(state.event_bus(), …)` after that site's `tx.commit()`.
  - Then do the mechanical rename through the listed files. `main.rs` creates one `EventBus::new()`, passes it into `RegistrationDeps` and `AppState::new` (replacing the `position_changed_tx` argument), and deletes the `position_changed_tx/rx` channel creation (line 1059).
- [ ] **Step 4: Run.**
  - `cargo build` compiles with no new warnings from `cargo clippy --bin oms 2>&1 | grep -E 'event_bus|position_changed'`.
  - `cargo test --bin oms` is green.
  - With `--ignored`, the two DB tests pass.
  - `grep -rn position_changed src` returns nothing outside `quote_feed.rs` (Task 4 removes the rest).
- [ ] **Step 5: Commit.** `git commit -am "feat(bus): publish committed order events and position changes on the order lane"`

### Task 4: Quote feeds subscribe to the order lane; delete the doorbells

**Files:**
- Modify: `src/quote_feed.rs`:
  - `QuoteFeedSession { position_changed_rx: mpsc::Receiver<()> }` becomes `order_rx: broadcast::Receiver<OrderLaneEvent>`;
  - `new(.., order_rx, health)`;
  - `wait_for_subscribable` and `watch_held` take the receiver.
- Modify: `src/app_state.rs`: delete `DoorbellRegistry` (lines ~75–115), the `doorbells` field and `doorbells()`.
- Modify: `src/reload.rs`:
  - `restart_databento_feed` (around line 437) takes `bus: &EventBus` instead of `doorbells: &DoorbellRegistry`;
  - remove `doorbells().remove/set` calls.
- Modify: `src/main.rs`:
  - the four public feed blocks (BINANCE, BYBIT, COINBASE, OKX, around lines 1300–1368) pass `state.event_bus().subscribe_order()`;
  - delete the fan-out task (lines 1370–1379).
- Modify: `src/reload_tests.rs`, if it references doorbells.

**Interfaces:**
- Consumes: Task 1 `EventBus::subscribe_order`, `OrderLaneEvent::PositionChanged`.
- Produces: `fn needs_recheck(r: &Result<OrderLaneEvent, broadcast::error::RecvError>) -> Recheck` in `quote_feed.rs`, with `enum Recheck { Yes, No, Closed }`:
  - `PositionChanged` → `Yes`;
  - `Appended` → `No`;
  - `Err(Lagged(_))` → `Yes`;
  - `Err(Closed)` → `Closed`.

  `watch_held` re-reads the held set on `Yes`, skips on `No`, and parks forever on `Closed` (same as today's `None`). `wait_for_subscribable` selects on `order_rx.recv()` with the same mapping, alongside the existing `IDLE_RECHECK_SECS` timer.

- [ ] **Step 1: Write the failing tests** in `quote_feed.rs` `mod tests`:
  - `position_changed_means_recheck`: `needs_recheck(&Ok(PositionChanged{..})) == Recheck::Yes`.
  - `appended_alone_is_not_a_recheck`: `Ok(Appended{..})` → `Recheck::No`.
  - `lagged_order_lane_means_recheck`: `Err(RecvError::Lagged(3))` → `Recheck::Yes`.
  - `closed_order_lane_parks`: `Err(RecvError::Closed)` → `Recheck::Closed`.
- [ ] **Step 2: Run, expect FAIL.** `cargo test --bin oms quote_feed` fails: `needs_recheck` not found.
- [ ] **Step 3: Implement** the Interfaces block, the signature changes and the deletions above.
- [ ] **Step 4: Run.**
  - `cargo test --bin oms` is green.
  - `grep -rn 'DoorbellRegistry\|doorbell\|position_changed' src` returns no code hits. Comments referencing history must be reworded to the bus.
  - `cargo test --bin oms -- --ignored reload` passes on the dev DB.
- [ ] **Step 5: Commit.** `git commit -am "refactor(feeds): subscribe to the order lane instead of per-feed doorbells"`

### Task 5: Mark router feeds the price lane; doc and manual check

**Files:**
- Modify: `src/mark_router.rs`:
  - `run(quotes, marks, pool)` becomes `run(quotes, marks, pool, bus: EventBus)`;
  - after `marks.set(..)` (line ~96): `bus.publish_mark(MarkUpdate::from(&q))`.
- Modify: `src/main.rs`, at the `mark_router::run` spawn: pass `state.event_bus().clone()`.
- Modify: `src/marks.rs` or module docs: one line saying the price lane mirrors every accepted mark.

**Interfaces:**
- Consumes: Task 1 `MarkUpdate`, `EventBus::publish_mark`.

- [ ] **Step 1: Write the failing test** `accepted_quote_is_published_on_price_lane` in `mark_router.rs` `mod tests`. Extract the post-`accept` lines into `fn apply_accepted(marks: &MarkStore, bus: &EventBus, q: &Quote)`. The test subscribes, calls it, and asserts that `price_rx.try_recv()` equals `MarkUpdate::from(&q)` and that `marks.get(id)` is `Some`.
- [ ] **Step 2: Run, expect FAIL.** `cargo test --bin oms mark_router` fails: `apply_accepted` not found.
- [ ] **Step 3: Implement** `apply_accepted` and call it from `run`.
- [ ] **Step 4: Run.** `cargo test --bin oms` is green. Then do the manual check:
  1. `cargo build && ./target/debug/oms`;
  2. the log shows `mark router: started` and each `connecting stream="…/SPOT"`;
  3. there is no `doorbell` in the log.
- [ ] **Step 5: Commit.** `git commit -am "feat(bus): mark router publishes accepted marks on the price lane"`
