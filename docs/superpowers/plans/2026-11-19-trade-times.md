# Trade Times & External Order Lifecycle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Store and show venue trade times separately from booking times, give external orders an honest observed lifecycle, and keep positions/realized P&L in trade-time order.

**Architecture:** Additive: `occurred_at` on order events, a new `order_observed` entry event for external orders, order-level `last_trade_at`/`venue_created_at`, and a per-position rebuild in canonical trade-time order when a fill arrives late. UI shows Placed / Trade time / Booked / Updated.

**Tech Stack:** Rust (tokio, sqlx/Postgres), React cockpit, Astro docs.

**Spec:** `docs/superpowers/specs/2026-11-19-trade-times-design.md`

## Global Constraints

- Migration is `db/migrations/ods/oms/0067_TRADE_TIMES.sql`; bump the embedded migration count test in `src/setup/database/assets.rs`.
- Booking time is never overwritten; event append order / `global_position` unchanged.
- Canonical fill order: `(COALESCE(occurred_at, created_at), execution_id, global_position)`.
- Estimated trade time: `occurred_at = booking time` + payload `trade_time_estimated: true`.
- `order_observed` books no fills; booked `cum_qty` starts at 0; `venue_cum_qty` informational.
- `venue_created_at` set once, never overwritten.
- No settlement/value/effective dates; no as-of reports.
- DB tests: `cargo test -- --ignored --skip real_docker` against a throwaway postgres:16 (`database init`, `OMS_RELOAD_TEST_DATABASE_URL`, `COLO_TEST_DATABASE_URL`, `BINANCE_PAPER_TRANSPORT=rest`). Never `--ignored` without `--skip real_docker`.
- Build: `OPENSSL_ROOT_DIR=/opt/homebrew/opt/openssl@3`.

## Review Focus

1. Same fills arriving in different orders (stream first vs pull first vs late adopt) → identical position qty, avg cost, realized P&L (Task 5 `any_arrival_order_gives_the_same_position`).
2. A late fill older than the last applied fill on a position that also has fees → fees re-applied at their fill's trade time, not double-counted (Task 5 `rebuild_reapplies_fees_once`).
3. External order observed as Filled, its fills arriving later by pull → booked once, cum reaches venue cum, history shows "Observed" then fills — never Submitted/Routed (Task 4 `observed_filled_order_then_fills_books_once`).
4. Fill with no venue time → `trade_time_estimated` and booking-time trade time; not treated as late (Task 4 `missing_execution_time_is_estimated_and_not_late`).
5. Break adopt replay creates `order_observed`, not submit+route (Task 4 `adopted_break_order_is_observed`).

---

### Task 1: Schema

**Files:** Create `db/migrations/ods/oms/0067_TRADE_TIMES.sql`; modify `src/setup/database/assets.rs` (count); test `src/ingest/schema_tests.rs`.

**Interfaces — Produces:** `order_event.occurred_at TIMESTAMPTZ NULL`; `order_state.last_trade_at TIMESTAMPTZ NULL`, `order_state.venue_created_at TIMESTAMPTZ NULL`, `order_state.venue_cum_qty NUMERIC NULL`; `position.last_trade_at TIMESTAMPTZ NULL`; index `order_state(last_trade_at DESC NULLS LAST)`.

- [ ] **Step 1:** Failing DB test `trade_time_columns_exist_and_default_null` (insert minimal rows, assert new columns NULL).
- [ ] **Step 2:** Run `cargo test -p rustoms schema_tests -- --ignored --skip real_docker` → FAIL.
- [ ] **Step 3:** Write migration (no backfill: legacy rows stay NULL).
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5:** Commit `feat(db): trade time columns`.

---

### Task 2: Venue order times from connectors

**Files:** `crates/gateway/src/venue_event.rs` (`VenueOrderUpdate.created_at: Option<DateTime<Utc>>`), connectors `binance`, `alpaca`, `bybit`, `okx`, `coinbase`, `ibkr_tws`, `fix/dialect.rs` (stream + snapshot mappers).

**Interfaces — Produces:** every `VenueOrderUpdate` carries `ts` (venue update time, existing) and `created_at` where the API provides it: Binance `O` (stream) / `time` (REST), Alpaca `order.created_at`, Bybit `createdTime`, OKX `cTime`, Coinbase `created_time`, FIX `TransactTime(60)` as update ts only; IBKR TWS `created_at = None`. Every `VenueFill.executed_at` populated wherever the venue sends it (audit each stream mapper; leave `None` only where absent).

- [ ] **Step 1:** Failing tests per connector `<venue>_order_update_carries_created_and_update_time` and `<venue>_stream_fill_carries_executed_at` (use testdata/inline JSON).
- [ ] **Step 2:** Run `cargo test -p gateway --features fix` → FAIL.
- [ ] **Step 3:** Implement extraction.
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5:** Commit `feat(gateway): venue order create/update and fill times`.

---

### Task 3: `order_observed` domain event

**Files:** `src/domain/orders/{events.rs,commands.rs,aggregate.rs,state.rs}`; `src/handlers.rs` `domain_event_to_new_event` if it maps types.

**Interfaces — Produces:** `OrderCommand::ObserveOrder(ObserveOrder { order_id, client_order_id, portfolio_id, account_id, instrument_id, side, order_type, time_in_force, limit_price: Option<f64>, quantity, venue, external_order_id, observed_status: OrderStatus /* Working|PartiallyFilled|Filled|Canceled|Expired|Rejected */, venue_cum_qty: f64, venue_created_at: Option<DateTime<Utc>> })` → `OrderEventType::OrderObserved` (`"order_observed"`), payload carrying the same fields; aggregate state: status = observed_status, cum_qty 0, leaves = quantity, routed with external id. `EventMetadata` gains `occurred_at: Option<DateTime<Utc>>` copied onto every event it produces.

- [ ] **Step 1:** Failing unit tests: `observe_creates_order_in_observed_status_with_zero_booked_cum`, `observe_on_existing_order_is_rejected`, `fill_after_observed_filled_is_accepted` (booked cum grows to venue cum; status stays Filled), `metadata_occurred_at_is_copied_to_events`.
- [ ] **Step 2:** Run `cargo test -p rustoms domain::orders` → FAIL.
- [ ] **Step 3:** Implement. Fill on an observed-Filled order must not be rejected by the terminal guard while booked cum < quantity.
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5:** Commit `feat(orders): order_observed entry event and occurred_at`.

---

### Task 4: Ingest writes venue times and observes external orders

**Files:** `src/ingest/apply.rs`, `src/ingest/adopt.rs`, `src/recon_admin_api.rs` (break replay), `src/event_store.rs` (persist `occurred_at`).

**Interfaces — Consumes:** Tasks 1–3. **Produces:** fill events with `occurred_at = executed_at` (or booking time + `trade_time_estimated: true`); status events with `occurred_at = VenueOrderUpdate.ts`; `order_state.last_trade_at = max(fill trade time)`; `venue_created_at` set once; `venue_cum_qty` updated from order updates; adoption uses `ObserveOrder` (status from the triggering event: an OrderUpdate's status, or `PartiallyFilled` for a first-seen Fill); positions call (Task 5) receives the fill's trade time.

- [ ] **Step 1:** Failing DB tests: `fill_event_stores_venue_execution_time`, `missing_execution_time_is_estimated_and_not_late`, `observed_filled_order_then_fills_books_once`, `adopted_break_order_is_observed`, `venue_created_at_is_set_once`, `last_trade_at_tracks_latest_fill`.
- [ ] **Step 2:** Run `cargo test -p rustoms ingest -- --ignored --skip real_docker` → FAIL.
- [ ] **Step 3:** Implement; remove SubmitOrder+RouteOrder from the adopt path.
- [ ] **Step 4:** Run → PASS (+ existing ingest/reconciler/recon_admin tests).
- [ ] **Step 5:** Commit `feat(ingest): venue times and observed external orders`.

---

### Task 5: Positions in trade-time order

**Files:** `src/positions.rs` (`persist_fill`, `persist_fee`, `persist_transfer`, `rebuild_positions_scoped`), callers in `src/execution.rs` / `src/ingest/apply.rs` / colo transfer path.

**Interfaces — Produces:** `persist_fill(tx, portfolio, instrument, side, qty, price, trade_time: DateTime<Utc>)`; if `trade_time < position.last_trade_at` → `rebuild_position(tx, portfolio, instrument)` replaying fills, fees (at their fill's trade time) and transfers (at their own time) in canonical order `(COALESCE(occurred_at, created_at), execution_id, global_position)`; else incremental and `last_trade_at = max`. Rebuild logs duration + fill count; publishes one PositionChanged.

- [ ] **Step 1:** Failing tests: `any_arrival_order_gives_the_same_position` (property: 3–6 fills buy/sell mix + fees, all permutations → equal qty/avg/realized within EPS, DB), `late_fill_triggers_rebuild_in_trade_time_order`, `rebuild_reapplies_fees_once`, `rebuild_includes_colo_transfers`, `equal_trade_times_order_by_execution_id`.
- [ ] **Step 2:** Run `cargo test -p rustoms positions -- --ignored --skip real_docker` → FAIL.
- [ ] **Step 3:** Implement; extend `rebuild_positions_scoped` ordering to the canonical order too (full rebuild CLI stays consistent).
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5:** Commit `feat(positions): trade-time order with late-fill rebuild`.

---

### Task 6: API fields

**Files:** `src/handlers.rs` (`list_orders` ~1118, admin orders ~1094, order events/detail, `get_portfolio_positions`), `src/models.rs`/response types, OpenAPI annotations.

**Interfaces — Produces:** order rows: `placed_at` (= `venue_created_at` if set else `created_at`, plus `placed_is_first_seen: bool` for external orders without venue time), `trade_time` (`last_trade_at`), `booked_at` (`created_at`), `updated_at`, `trade_time_estimated: bool`; event rows: `occurred_at`, `booked_at`; positions: `instrument_name`/`symbol`. Orders list default sort `trade_time DESC NULLS LAST, booked_at DESC`.

- [ ] **Step 1:** Failing DB tests: `orders_list_returns_trade_times_sorted`, `external_order_without_venue_time_marks_placed_first_seen`, `order_events_return_occurred_and_booked`, `positions_include_instrument_name`.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run → PASS; regenerate any OpenAPI snapshot the repo checks.
- [ ] **Step 5:** Commit `feat(api): trade, booking and placed times; position instrument names`.

---

### Task 7: Cockpit

**Files:** `cockpit/src/components/trading/TradeBlotter.tsx` (+ `table.ts`, `types.ts`), `cockpit/src/components/OrderTimeline.tsx`, `cockpit/src/pages/trading/Positions.tsx`, `cockpit/src/api/types.ts`.

**Interfaces — Consumes:** Task 6 fields. Blotter columns Placed / Trade time / Booked / Updated (estimated trade time marked "~", first-seen placed marked); default sort Trade time desc. Timeline: `order_observed` rendered "Observed at venue"; each row venue time + booked time. Positions: instrument symbol/name, id in tooltip.

- [ ] **Step 1:** Failing logic tests (node --test, existing setup): `blotter_sorts_by_trade_time_then_booked`, `timeline_labels_observed_not_submitted`, `positions_label_uses_instrument_name`.
- [ ] **Step 2:** Run `cd cockpit && npm test` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `npm test && npm run build` → PASS.
- [ ] **Step 5:** Commit `feat(cockpit): trade times, observed history, position names`.

---

### Task 8: Docs

**Files:** `docs-site/src/content/docs/concepts/reconciliation.md` (+ orders/positions concept page if present).

- [ ] **Step 1:** Document the four times, estimated trade time, observed lifecycle, trade-time positions/P&L rebuild. No hand-written changelog.
- [ ] **Step 2:** Run `cd docs-site && npm run check` → PASS.
- [ ] **Step 3:** Commit `docs: trade times and observed external orders`.
