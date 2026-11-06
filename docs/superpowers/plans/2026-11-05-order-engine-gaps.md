# Order Engine Gaps Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** No order sits in `submitted` forever, and every order the OMS refused before it left is on record.

**Architecture:**
- **What's wrong today:**
  - A venue's definite reject after the order is stored leaves it `submitted` with no event.
  - A failure after routing (TX2) leaves it `submitted` while the venue works it.
  - A pre-trade reject stores nothing.
- **Fix 1:** a definite broker reject becomes an `ExecEvent` `Rejected` through the order engine, the one place that books.
- **Fix 2:** pre-trade rejects are stored as an order that was submitted and then rejected.
- **Fix 3:** a recon sweep finds orders stuck in `submitted` past a grace period and settles them from what the venue says. It looks them up by our client order id, the OMS order id each connector already sends.

**Tech Stack:** Rust 2021, sqlx/Postgres, the existing order engine (`src/order_engine.rs`), recon (`src/recon_orders.rs`), and the gateway contract (`crates/gateway/src/contract.rs`).

**Spec:** `docs/superpowers/specs/2026-11-04-target-architecture-design.md` (branch `docs/target-architecture`):
- §4.1 order engine, "the **only** place that applies execution events";
- §4.3 `ExecEvent` kinds incl. `Ack`;
- §5 flow 4 (recon).

The gap list this plan closes was agreed in conversation on 2026-11-05 (items 1–3; item 4, external orders, is phase 8).

## Global Constraints

- The order engine is the only place that books execution events (spec §4.1). Order entry may book its own `SubmitOrder` / `RouteOrder` as today, but a venue's answer goes through `EngineHandle::exec`.
- **Don't guess outcomes.** A transport error or timeout is not a reject: the order may have reached the venue. Only `BrokerError::BrokerRejected` is a definite reject. Everything else waits for the sweep.
- **Grace period** before an order counts as stuck: **60 s** after `order_state.created_at`.
- **Not found at the venue** gets the reject reason `never reached the broker`.
- **Rules:**
  - DB tests are `#[ignore]` against the shared dev DB; never run `--ignored`; never create databases on it.
  - No compatibility shims.
  - Stage files explicitly; a heredoc and `git commit` never go in one Bash call.
- **Branch:** `feat/engine-gaps`, cut from `feat/strategy-fix` (`8ddd72d`).
- **Baseline:** workspace 744 passed, 0 failed, 88 ignored.

## Review Focus

1. **Transport error versus reject.** A network timeout on submit must leave the order `submitted` for the sweep, never `rejected`, because the venue may be working it. Test: `a_transport_error_is_not_a_reject` (Task 1).
2. **FIX strategies must not get two rejects.** A stored reject reaches a FIX strategy once, through the report queue, not also as a synchronous 35=8. Test: `a_stored_reject_is_reported_from_the_queue_only` (Task 1).
3. **A stuck order the venue still works** gets `routed` with the venue's order id, plus any fills it missed, not rejected. Test: `a_stuck_order_open_at_the_broker_is_routed` (Task 3).
4. **An order younger than the grace period is never touched**, even when the venue does not list it yet. Test: `a_young_submitted_order_is_left_alone` (Task 3).
5. **A venue with no client-id lookup** leaves the order `submitted` and logs `error!`. It never rejects blindly. Test: `an_unknown_outcome_leaves_the_order` (Task 3).

---

### Task 1: A broker's reject is booked; a transport error is not

**Files:**
- Modify: `src/order_entry.rs:540-560` (the submit-time `adapter.submit_order` error arms) and the `no adapter configured` arm.
- Modify: `src/strategies/session.rs` (the `NewOrder` error path).

**Interfaces:**
- **In `order_entry`:**

```rust
/// What a failed broker call means for the stored order.
#[derive(Debug, PartialEq)]
pub enum RouteFailure { Rejected(String), Unknown }
pub fn route_failure(err: &BrokerError) -> RouteFailure; // BrokerRejected(m) → Rejected(m); everything else → Unknown
```

- **On `Rejected(reason)`, and when no adapter is registered** (with `"no adapter configured for {broker}/{env}"`): submit calls

  ```rust
  state.engine().exec(ExecEvent { order_id, venue: broker_code, actor: "oms", source: ExecSource::Session, kind: ExecKind::Rejected { reason } })
  ```

  and returns the same `ApiError` as today (502 / 503).
- **On `Unknown`:** nothing is booked. The `warn!` says the order is left for recon.
- **FIX session:** after `order_entry::submit` returns `Err`, the session sends its synchronous `rejected(...)` only when no order row exists for `(portfolio_id, fix_cl_ord_id)`. A stored order reports through `fix_report_queue`.

- [ ] **Step 1: Write the failing tests**
  - In `src/order_entry.rs`:

    ```rust
    #[test] fn a_venue_reject_is_a_reject() { assert_eq!(route_failure(&BrokerError::BrokerRejected("insufficient balance".into())), RouteFailure::Rejected("insufficient balance".into())); }
    #[test] fn a_transport_error_is_not_a_reject() { /* BrokerError variants other than BrokerRejected (network / timeout / NotConfigured) → Unknown */ }
    ```

  - In `src/strategies/session.rs`:

    ```rust
    #[test] fn a_stored_reject_is_reported_from_the_queue_only() { assert!(!needs_sync_reject(true)); assert!(needs_sync_reject(false)); }
    ```

    with `fn needs_sync_reject(order_stored: bool) -> bool`.
  - `#[ignore]` DB test in `src/order_entry.rs`: `a_broker_reject_after_storing_books_order_rejected`.
- [ ] **Step 2: Run** `cargo test --bin oms route_failure a_venue_reject a_transport_error a_stored_reject` and expect compile errors.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same and expect PASS. Then run the workspace sum and expect 0 failed.
- [ ] **Step 5: Commit** with `fix(orders): a broker's reject is booked through the order engine; a transport error waits for recon`.

---

### Task 2: Pre-trade rejects are on record

**Files:**
- Modify: `src/order_entry.rs`:
  - the TX1 risk-check `map_err`;
  - the `guard::reference` call;
  - the `// TODO: also persist an OrderDenied audit event` comment (remove it).

**Interfaces:**

```rust
/// Store `cmd` as an order that was submitted and rejected at entry: an
/// `order_state` row (status `rejected`, leaves 0) and the events
/// `order_submitted` then `order_rejected { reason: "{code}: {message}" }`, in
/// one transaction, published on the order lane after commit.
async fn record_rejection(state: &AppState, auth: &AuthContext, order_id: Uuid, cmd: SubmitOrder,
    broker_connection_code: &str, origin: &Origin, rejection: &RiskRejection) -> Result<(), ApiError>;
```

- **When:** a `RiskCheckError::Rejected`, or a `guard::reference` rejection. TX1 is rolled back first, then the rejection is recorded in its own transaction.
- **Events:** built with the aggregate, `SubmitOrder` then `ReceiveExecutionReport(ExecutionReport::Reject { reason, .. })`, so they go through the state machine.
- **Not recorded:** request-shape errors (bad UUIDs, unknown instrument, no account) stay unrecorded; there is nothing to attach them to.
- **REST:** still answers 422 with the same message. A retry with the same `order_id` now gets 409, the existing duplicate rule.
- **FIX:** the `order_rejected` event queues a 35=8 39=8 with the reason in 58, through Task 1's rule.

- [ ] **Step 1: Write the failing tests:**
  - in `src/domain/orders/aggregate.rs` tests: `a_submitted_order_rejected_at_entry_ends_rejected`. Decide `SubmitOrder`, apply, decide `ReceiveExecutionReport(Reject)`, apply. The final status is `Rejected`, and the two events are `OrderSubmitted`, `OrderRejected`. If this already passes, it pins the behaviour Task 2 relies on: record that in the ledger;
  - `#[ignore]` DB test: `a_pre_trade_reject_is_stored_and_appears_in_the_blotter`.
- [ ] **Step 2: Run** and confirm the aggregate test's result (see Step 1).
- [ ] **Step 3: Implement** `record_rejection` and call it at both sites.
- [ ] **Step 4: Run** the workspace sum and expect 0 failed.
- [ ] **Step 5: Commit** with `feat(orders): orders refused before they leave are stored as rejected, with the reason`.

---

### Task 3: Orders stuck in `submitted` are settled by recon

**Files:**
- Modify:
  - `crates/gateway/src/contract.rs`: `BrokerOpenOrder` gains `venue_order_id: String`; `ExecKind` gains `Ack { venue_order_id: String }`;
  - every connector's `open_orders` parser (fill `venue_order_id`) and `order_status` (client-id lookup when `external_order_id` is empty), in `crates/gateway/src/connectors/{alpaca,binance,bybit,coinbase,okx}/adapter.rs`. FIX already asks 35=H by ClOrdID;
  - `src/order_engine.rs`: apply `Ack` as `RouteOrder { venue: ev.venue, external_order_id }` when the order is `submitted`, ignore it otherwise;
  - `src/recon_orders.rs`: load stuck orders, new actions, run them.

**Interfaces:**

```rust
// recon_orders.rs
pub struct StuckOrder { pub order_id: Uuid, pub symbol: String }
pub async fn load_stuck_orders(pool: &PgPool, broker_code: &str) -> Vec<StuckOrder>;
//   status 'submitted' AND created_at < now() - interval '60 seconds' on that broker's connections
pub enum StuckAction { Route { order_id: Uuid, venue_order_id: String, executed_qty: f64 }, AskStatus { order_id: Uuid, symbol: String } }
pub fn plan_stuck(stuck: &[StuckOrder], broker_open: &[BrokerOpenOrder]) -> Vec<StuckAction>;
```

- **Running an action:**
  - **`Route`:** sends `Ack`, then the existing missed-partial logic when `executed_qty > 0`.
  - **`AskStatus`:** calls `order_status(client_order_id = order_id, external_order_id = "", symbol)`:
    - `Ok(state)` → `ExecKind::Terminal(state)`;
    - an `Err` that means the venue has no such order (each connector maps its "unknown order" answer to `BrokerError::BrokerRejected` whose message starts `no order`) → `ExecKind::Rejected { reason: "never reached the broker" }`;
    - `NotConfigured` or anything else → `error!` and leave the order.
- **Client-id lookups per venue:** these are each venue's documented query by our client order id:
  - Alpaca `GET /v2/orders:by_client_order_id?client_order_id=`;
  - Binance `origClientOrderId`;
  - Bybit `orderLinkId`;
  - OKX `clOrdId`;
  - Coinbase: the historical-orders endpoint's client-order-id filter if its API has one, else `NotConfigured`. Record which in the ledger.
- **Orphans:** OMS orders not working at the venue are already reported as `OrphanAtBroker` by recon. A stuck order is now matched first, so it no longer shows up there.

- [ ] **Step 1: Write the failing tests:**
  - In `src/recon_orders.rs`:

```rust
#[test] fn a_stuck_order_open_at_the_broker_is_routed() {
    // stuck [o1], broker_open [{client_order_id: o1, venue_order_id: "v9", executed_qty: 0.5}] → [Route{o1, "v9", 0.5}]
}
#[test] fn a_stuck_order_absent_at_the_broker_is_asked_about() { /* stuck [o1], broker_open [] → [AskStatus{o1, ..}] */ }
#[test] fn a_young_submitted_order_is_left_alone() { /* SQL guard: assert the load query text contains "interval '60 seconds'" (const STUCK_AFTER_SECS = 60 used to build it) */ }
#[test] fn an_unknown_outcome_leaves_the_order() { /* fn stuck_outcome(r: &Result<BrokerOrderState, BrokerError>) -> Option<ExecKind>: NotConfigured → None; BrokerRejected("no order …") → Some(Rejected{"never reached the broker"}); Ok(s) → Some(Terminal(s)) */ }
```

  - In `src/order_engine.rs` tests: `an_ack_routes_a_submitted_order_once`. Applying `Ack` twice yields one `order_routed`.
  - **Per connector, where the adapter already has parser tests:**
    - `open_orders_carry_the_venue_order_id` (Alpaca, Binance, Bybit, Coinbase, OKX test JSON);
    - `order_status_looks_up_by_client_id_without_a_venue_id` (the request URL or query built for an empty external id).
- [ ] **Step 2: Run** `cargo test --workspace` and expect compile errors.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the workspace sum and expect 0 failed.
- [ ] **Step 5: Add an `#[ignore]` DB test, `recon_rejects_a_stuck_order_the_venue_never_saw`.** It seeds a `submitted` order 2 minutes old with no venue id, then runs `run_order_reconcile` with a stub adapter whose `open_orders` is empty and whose `order_status` answers `BrokerRejected("no order …")`. It asserts `rejected` with reason `never reached the broker`. A crash between TX1 and TX2 cannot be reproduced by hand on the dev OMS, so this test stands in for a dev check.
- [ ] **Step 6: Commit** with `feat(recon): orders stuck in submitted are settled from what the venue says`.
