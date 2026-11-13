# Fee Booking Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Book the fee each venue reports on every new fill. Realized P&L is
net of quote-currency fees, base-asset fees shrink the holding, and any other
fee currency shows as a per-currency total.

**Architecture:**
- A fee is its own append-only order event (`order_fee_charged`).
- The event is booked by a new `fees` module through the order engine.
- A new `order_fee` table does two jobs: dedupe (unique per order + `fee_id`)
  and the read model for fee columns and third-currency totals.
- Connectors pass fees to the engine as two new gateway `ExecKind` variants:
  - `Fee`, per execution;
  - `FeeTotal`, a running total per order.
- Position math gains `apply_fee`. `rebuild_positions` replays fee events.

**Tech Stack:**
- Rust: axum, sqlx/Postgres, tokio; the gateway crate.
- React/TypeScript cockpit (Mantine, react-query; `node --test`).
- Astro docs-site.

**Spec:** `docs/superpowers/specs/2026-11-12-fee-booking-design.md`

## Global Constraints

- Fees per currency; never FX-convert.
- Only new fills; no backfill of existing fills or positions.
- Only venue-reported fees; no estimates or fee schedules.
- `order_event` is append-only. Never update or delete it, including in tests.
- DB tests are `#[ignore]` and seed their own data. Never run `--ignored`
  locally against the shared dev DB at 100.80.233.3; use a throwaway Postgres
  as in HANDOFF.md.
- Migrations are additive only. The next number is `0046` in
  `db/migrations/ods/oms/`.
- Sign convention: a charged fee is positive and a rebate is negative,
  whatever the venue's own sign.
- No credentials in code, tests or commits. Conventional Commits. Stage files
  explicitly. Write commit messages to a file first; no heredoc with
  `git commit`.
- Cockpit: no help text in the UI; explanations go in the docs.

## Review Focus

1. **A fee arriving after its order is terminal** (IBKR commission after the
   last fill). It must still book; `apply_once` skips terminal orders, so the
   fee path must not. Test in Task 2.
2. **The same fee delivered twice** (session plus snapshot recon, a reconnect,
   or restart catch-up) books once. Tests in Tasks 2, 4, 5 and 6.
3. **A cumulative total that repeats or goes down** books nothing, or a
   negative correction. Test in Task 3.
4. **A base-asset fee on a pair whose symbol has a separator** (`BTC-USD`,
   `EUR.USD`) resolves to `Base`, not `Other`. Test in Task 1.
5. **Rebuild equals incremental**, including fees booked before their fill.
   Test in Task 2.

---

### Task 1: Fee domain — effect, event, position math

**Files:**
- Create: `src/fees.rs` (pure part only in this task: `FeeEffect`,
  `fee_effect`). Register `mod fees;` in `src/main.rs`/`src/lib.rs`, wherever
  the other modules are declared.
- Modify: `src/domain/orders/events.rs` (new event type and payload).
- Modify: `src/domain/orders/aggregate.rs:321` `apply`: the new event changes
  nothing but `version`.
- Modify: `src/positions.rs` (`PositionMath::apply_fee`).

**Interfaces:**
- Produces:
  - `pub enum FeeEffect { Quote, Base, Other }`, with serde names `"quote"`,
    `"base"`, `"other"`.
  - `pub fn fee_effect(fee_currency: &str, symbol: &str, asset_class: &str, instrument_currency: &str) -> FeeEffect`
  - `OrderEventType::OrderFeeCharged` (`as_str` = `"order_fee_charged"`)
  - `OrderEventPayload::OrderFeeCharged { fee_id: String, execution_id: Option<String>, amount: f64, currency: String, effect: FeeEffect }`
  - `impl PositionMath { pub fn apply_fee(self, amount: f64, effect: FeeEffect) -> PositionMath }`

`fee_effect` rules (compare case-insensitively):
- The fee currency equals the instrument currency → `Quote`.
- `asset_class` is `CRYPTO` or `FX`, and the symbol (with `-`, `.`, `/`, `_`
  removed) ends with the instrument currency and the rest equals the fee
  currency → `Base`.
- Otherwise → `Other`.

`apply_fee` rules:
- `Quote`: `realized_pnl -= amount`.
- `Base`: shrink `|net_qty|` by `amount` at unchanged `avg_cost`, keeping the
  sign. Flat stays flat; the quantity is never pushed past zero. Leftover fee
  when flat changes nothing.
- `Other`: unchanged.

- [ ] **Step 1: Write the failing tests** in `src/fees.rs` and `src/positions.rs` tests:

```rust
#[test] fn usdt_fee_on_btcusdt_is_quote() { assert_eq!(fee_effect("USDT", "BTCUSDT", "CRYPTO", "USDT"), FeeEffect::Quote); }
#[test] fn btc_fee_on_btcusdt_is_base() { assert_eq!(fee_effect("btc", "BTCUSDT", "CRYPTO", "USDT"), FeeEffect::Base); }
#[test] fn base_fee_with_separator_symbol() {
    assert_eq!(fee_effect("BTC", "BTC-USD", "CRYPTO", "USD"), FeeEffect::Base);
    assert_eq!(fee_effect("EUR", "EUR.USD", "FX", "USD"), FeeEffect::Base);
}
#[test] fn bnb_fee_on_btcusdt_is_other() { assert_eq!(fee_effect("BNB", "BTCUSDT", "CRYPTO", "USDT"), FeeEffect::Other); }
#[test] fn usd_commission_on_cad_stock_is_other() { assert_eq!(fee_effect("USD", "SHOP", "EQUITY", "CAD"), FeeEffect::Other); }
#[test] fn equity_never_has_a_base() { assert_eq!(fee_effect("AAPL", "AAPL", "EQUITY", "USD"), FeeEffect::Other); }

// positions.rs
#[test] fn quote_fee_reduces_realized() { let p = PositionMath { net_qty: 1.0, avg_cost: 100.0, realized_pnl: 5.0 }.apply_fee(1.5, FeeEffect::Quote); approx(p.realized_pnl, 3.5); approx(p.net_qty, 1.0); }
#[test] fn rebate_increases_realized() { approx(PositionMath::FLAT.apply_fee(-0.2, FeeEffect::Quote).realized_pnl, 0.2); }
#[test] fn base_fee_shrinks_long_at_cost() { let p = PositionMath { net_qty: 1.0, avg_cost: 64000.0, realized_pnl: 0.0 }.apply_fee(0.001, FeeEffect::Base); approx(p.net_qty, 0.999); approx(p.avg_cost, 64000.0); approx(p.realized_pnl, 0.0); }
#[test] fn base_fee_on_short_moves_toward_flat() { approx(PositionMath { net_qty: -2.0, avg_cost: 10.0, realized_pnl: 0.0 }.apply_fee(0.5, FeeEffect::Base).net_qty, -1.5); }
#[test] fn base_fee_never_crosses_zero() { let p = PositionMath { net_qty: 0.0005, avg_cost: 10.0, realized_pnl: 0.0 }.apply_fee(0.001, FeeEffect::Base); approx(p.net_qty, 0.0); approx(p.avg_cost, 0.0); }
#[test] fn other_fee_changes_nothing() { let p = PositionMath { net_qty: 1.0, avg_cost: 1.0, realized_pnl: 1.0 }; assert_eq!(p.apply_fee(3.0, FeeEffect::Other), p); }
```

Also add an aggregate test: applying an `OrderFeeCharged` event to a filled
order bumps `version` and leaves `status`, `cum_qty` and `avg_px` unchanged.

- [ ] **Step 2: Run** `cargo test fees:: positions:: aggregate` → FAIL (not defined).
- [ ] **Step 3: Implement** the interfaces above. `PositionMath` needs `PartialEq` and `Debug` if missing.
- [ ] **Step 4: Run** `cargo test fees:: positions:: aggregate` → PASS; `cargo test --workspace` green.
- [ ] **Step 5: Commit** `feat(fees): fee effect, fee event and position fee math`

### Task 2: Book a fee — table, booking, rebuild

**Files:**
- Create: `db/migrations/ods/oms/0046_CREATE_ORDER_FEE.sql`
- Modify: `src/fees.rs` (DB part)
- Modify: `src/positions.rs` (`persist_fee`, `rebuild_positions`)

The migration:

```sql
CREATE TABLE order_fee (
    order_id      UUID NOT NULL REFERENCES order_state(order_id),
    fee_id        TEXT NOT NULL,
    execution_id  TEXT,
    portfolio_id  UUID NOT NULL,
    instrument_id TEXT NOT NULL,
    amount        NUMERIC NOT NULL,
    currency      TEXT NOT NULL,
    effect        TEXT NOT NULL CHECK (effect IN ('quote','base','other')),
    booked_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (order_id, fee_id)
);
CREATE INDEX order_fee_position_idx ON order_fee (portfolio_id, instrument_id, currency);
```

Grant the runtime role the same rights as on `position`; copy the grant
pattern from `0011`/`0041`.

**Interfaces:**
- Consumes: Task 1's `FeeEffect`, `fee_effect`, `apply_fee`, `OrderFeeCharged`.
- Produces:
  - `pub struct FeeCharge { pub fee_id: String, pub execution_id: Option<String>, pub amount: f64, pub currency: String }`
  - `pub enum FeeBooked { Booked, Duplicate, UnknownOrder }`
  - `pub async fn book_fee(pool: &PgPool, bus: Option<&EventBus>, order_id: Uuid, fee: FeeCharge, actor: &str) -> Result<FeeBooked, BoxError>`
  - `pub async fn booked_fee_total(pool: &PgPool, order_id: Uuid, currency: &str) -> Result<f64, sqlx::Error>`
  - `pub async fn persist_fee(conn: &mut PgConnection, portfolio_id: Uuid, instrument_id: &str, amount: f64, effect: FeeEffect) -> Result<(), sqlx::Error>` (in `positions.rs`)

`book_fee`, in one transaction:
1. `SELECT … FROM order_state WHERE order_id=$1 FOR UPDATE`. Join `instrument`
   on `order_state.instrument_id` for `symbol`, `asset_class` and `currency`;
   check how `order_entry.rs:656` reads the instrument row. Missing →
   `UnknownOrder`, log a warning. **Do not skip terminal orders.**
2. `INSERT INTO order_fee … ON CONFLICT DO NOTHING`. Zero rows →
   `Duplicate`; roll back and return.
3. `persist_fee`.
4. Append one `order_fee_charged` event with `expected_version = version`, as
   `execution.rs:apply_once` does.
5. `UPDATE order_state SET version = version + 1`.
6. Commit. Publish appended, and `PositionChanged` when the effect is not
   `Other`.

Retry on `ConcurrencyRetry` like `process_execution_report`; reuse it or
lift its retry helper.

`rebuild_positions` must:
- select `order_fee_charged` events in the same `global_position` order as
  fills;
- apply `apply_fee(amount, effect)` using the event's stored `effect`, with
  no instrument lookup.

`order_fee` is not rebuilt; it is the source of truth for the fee rows.

- [ ] **Step 1: Write the failing DB tests** (`#[ignore]`, seed via `execution::tests::seeded_order`):

```rust
#[tokio::test] #[ignore] async fn a_quote_fee_books_event_row_and_pnl()          // FeeBooked::Booked; one order_fee row; one order_fee_charged event; position.realized_pnl == -fee
#[tokio::test] #[ignore] async fn the_same_fee_id_books_once()                   // second call → FeeBooked::Duplicate; still one row, one event
#[tokio::test] #[ignore] async fn a_fee_on_a_filled_order_still_books()         // order status 'filled' → Booked
#[tokio::test] #[ignore] async fn a_fee_for_an_unknown_order_is_reported()      // random uuid → UnknownOrder, nothing written
#[tokio::test] #[ignore] async fn rebuild_matches_incremental_with_fees()       // fee booked BEFORE fill, then fill, then base fee; snapshot position; rebuild_positions; position equal (net_qty, avg_cost, realized_pnl)
#[tokio::test] #[ignore] async fn booked_fee_total_sums_one_currency()          // two USD fees + one BNB fee → booked_fee_total(USD) == sum of the two
```

- [ ] **Step 2: Run** against a throwaway Postgres (HANDOFF.md recipe): `cargo test fees -- --ignored` → FAIL.
- [ ] **Step 3: Implement** the migration, `book_fee`, `booked_fee_total`, `persist_fee` and the rebuild change.
- [ ] **Step 4: Run** the same → PASS; `cargo test --workspace` green.
- [ ] **Step 5: Commit** `feat(fees): book fees as order events with an order_fee read model`

### Task 3: Engine — `Fee` and `FeeTotal` execution kinds

**Files:**
- Modify: `crates/gateway/src/contract.rs` (`ExecKind`)
- Modify: `src/order_engine.rs` (`apply_exec`)
- Modify: every exhaustive `match` on `ExecKind`; `cargo build` finds them.

**Interfaces:**
- Produces:
  - `ExecKind::Fee { fee_id: String, execution_id: Option<String>, amount: f64, currency: String }`
  - `ExecKind::FeeTotal { total: f64, currency: String }`: a running total
    of the fees charged so far on this order, in one currency.
  - `pub fn fee_delta(order_id: Uuid, venue: &str, booked: f64, total: f64, currency: &str) -> Option<FeeCharge>`
    in `src/fees.rs`. It returns `None` when `|total - booked| < 1e-12`.
    Otherwise `amount = total - booked` and
    `fee_id = format!("{venue}-{order_id}-{currency}-{total}")`.
- Consumes: Task 2's `book_fee` and `booked_fee_total`.

`apply_exec` rules:
- `Fee` → `book_fee` directly.
- `FeeTotal` → `booked_fee_total`, then `fee_delta`, then `book_fee`.
- Neither goes through `reports_for`; it returns an empty `Vec` for both.
- Log `UnknownOrder` at warn level.

- [ ] **Step 1: Write the failing tests** (`src/fees.rs`, pure):

```rust
#[test] fn fee_delta_books_the_increase() { let f = fee_delta(id, "COINBASE", 0.5, 0.8, "USD").unwrap(); approx(f.amount, 0.3); assert_eq!(f.currency, "USD"); }
#[test] fn a_repeated_total_books_nothing() { assert!(fee_delta(id, "COINBASE", 0.8, 0.8, "USD").is_none()); }
#[test] fn a_lower_total_books_a_negative_correction() { approx(fee_delta(id, "OKX", 0.8, 0.6, "USDT").unwrap().amount, -0.2); }
#[test] fn the_fee_id_names_the_total_reached() { assert_eq!(fee_delta(id, "OKX", 0.0, 0.6, "USDT").unwrap().fee_id, format!("OKX-{id}-USDT-0.6")); }
```

Also extend `order_engine.rs` tests:
`reports_for` yields no execution report for `Fee`/`FeeTotal`; and a DB test
`a_fee_total_sent_twice_books_once` (`#[ignore]`), modelled on
`cumulative_update_twice_books_once`.

- [ ] **Step 2: Run** `cargo test fees:: order_engine::` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** → PASS; `cargo test --workspace` green.
- [ ] **Step 5: Commit** `feat(engine): fee and running fee total execution kinds`

### Task 4: Per-fill connectors — Binance, Bybit, IBKR_TWS, colo

**Files:**
- Modify: `crates/gateway/src/connectors/binance/stream.rs` (user-data
  `executionReport`: `n` = amount, `N` = currency).
- Modify: the Binance recon/snapshot path that already reads `myTrades`
  (`adapter.rs:parse_my_trades`). Emit fees there too; the same `fee_id`
  dedupes.
- Modify: `crates/gateway/src/connectors/bybit/stream.rs` (`execution`
  topic, `execFee`). Fee currency follows Bybit v5 spot: buy → base coin,
  sell → quote coin, unless the message carries `feeCurrency`, which wins.
  Check the v5 docs and write the rule in a comment.
- Modify: `crates/gateway/src/connectors/ibkr_tws/stream.rs:111-113`. Replace
  the log line with `ExecKind::Fee`, matched to the order through the exec id
  → order mapping the stream already keeps for executions.
- Modify: `src/colo/booking.rs:193` `send_fill`. After the fill, send
  `ExecKind::Fee` when `fill.fee_amount` and `fill.fee_currency` are `Some`.

`fee_id` per venue: `"{VENUE}-{trade or exec id}"`, so session and recon
give the same id. Each connector sends the `Fill` first, then the `Fee`.
Alpaca: no change.

- [ ] **Step 1: Write the failing tests**, one per connector, in each one's
  existing test module, using its testdata frames:
  - `binance_trade_report_sends_fill_then_fee` (`n`=`0.01`, `N`=`BNB` →
    `ExecKind::Fee { fee_id: "BINANCE-<t>", amount: 0.01, currency: "BNB", .. }`)
  - `binance_zero_commission_sends_no_fee` (`n`=`0`)
  - `bybit_execution_sends_fee_in_base_on_buy` (`ws_execution.json`:
    `execFee` `0.000001`, `side` Buy → currency `BTC`)
  - `ibkr_commission_report_becomes_fee` (commission `1.0` USD for a known
    exec id → `Fee { fee_id: "IBKR_TWS-<exec_id>", execution_id: Some(..) }`)
  - `ibkr_commission_for_unknown_exec_is_dropped_with_warning`
  - `ibkr_replayed_commission_has_the_same_fee_id` (restart catch-up replays
    `executions_today`)
  - `colo_fill_with_fee_sends_fee_event` (in `src/colo/booking.rs` tests)

  A zero fee (`0`) sends nothing. It is a real case on Binance testnet and
  for commission-free fills.
- [ ] **Step 2: Run** `cargo test -p gateway binance bybit ibkr_tws` and `cargo test colo::booking` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** → PASS; `cargo test --workspace` green.
- [ ] **Step 5: Commit** `feat(connectors): pass venue fees for Binance, Bybit, IBKR and colo fills`

### Task 5: Running-total connectors — OKX, Coinbase

**Files:**
- Modify: `crates/gateway/src/connectors/okx/stream.rs`. Use the `orders`
  channel's `fee` (cumulative, negative = charged) and `feeCcy`. Send
  `ExecKind::FeeTotal { total: -fee, currency: feeCcy }`, next to the
  `Cumulative` it already sends. A positive OKX `fee` is a rebate, so the
  total goes negative.
- Modify: `crates/gateway/src/connectors/coinbase/stream.rs`. Use
  `total_fees` (cumulative, quote currency = the product's quote, e.g. `USD`
  for `BTC-USD`) and send `FeeTotal`.
- Modify: both connectors' snapshot/recon reads, if they produce
  `Cumulative`/`Terminal`, to also send `FeeTotal`. The delta logic makes
  this safe.

- [ ] **Step 1: Write the failing tests:**
  - `okx_order_update_sends_fee_total_with_charged_positive` (`fee`=`-0.06`,
    `feeCcy`=`USDT` → `FeeTotal { total: 0.06, currency: "USDT" }`)
  - `okx_rebate_is_a_negative_total`
  - `coinbase_update_sends_fee_total_in_quote` (edit `ws_user_partial.json`
    or a copy so `total_fees` is non-zero → `FeeTotal { total: <it>, currency: "USD" }`)
  - `coinbase_zero_total_fees_sends_nothing`
- [ ] **Step 2: Run** `cargo test -p gateway okx coinbase` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** → PASS; `cargo test --workspace` green.
- [ ] **Step 5: Commit** `feat(connectors): running fee totals for OKX and Coinbase`

### Task 6: API, Kafka, OpenAPI

**Files:**
- Modify: the orders list/blotter handler. Add
  `fees: Vec<FeeAmount>` per order, summed from `order_fee` per currency.
  `pub struct FeeAmount { pub currency: String, pub amount: f64 }`
  (utoipa `ToSchema`).
- Modify: `src/positions.rs` `Position`. Add `other_fees: Vec<FeeAmount>`
  (rows with `effect = 'other'` for this portfolio and instrument, summed per
  currency), filled where positions are loaded for
  `/portfolios/{id}/positions` (`handlers.rs:509`) and the cockpit's
  `/trade/positions`.
- Modify: `src/order_events.rs`. Make sure the `/orders/{id}/events` summary
  text covers `order_fee_charged`, e.g. `"fee 1.00 USD"`.
- Modify: `src/kafka_outbox.rs:169`. For `order_fee_charged`, publish a fill
  record update or a fee record with
  `fee_amount`/`fee_currency`/`execution_id`, the same way fills are mapped.
  If the existing record shapes make that awkward, add one optional fee
  record type and note the choice in the commit message.
- Regenerate: `cargo run -- openapi > docs/openapi.json`.

- [ ] **Step 1: Write the failing tests:**
  - `#[ignore]` DB `orders_list_carries_fees_per_currency` (one order, fees
    `1.0 USD`, `0.5 USD`, `0.01 BNB` → `[{USD,1.5},{BNB,0.01}]`)
  - `#[ignore]` DB `position_carries_other_currency_fees`
  - `order_fee_event_has_a_summary`
  - `fee_event_maps_to_an_outbox_record`
  - Existing OpenAPI staleness test (fails until regenerated).
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace` and the DB tests on the throwaway Postgres → PASS.
- [ ] **Step 5: Commit** `feat(api): fees on orders, positions, order events and Kafka`

### Task 7: Cockpit, docs, live check

**Files:**
- Modify: `cockpit/src/trade/components/TradeBlotter.tsx` and `cockpit/src/trade/types.ts`. Add a **Fees**
  column: per currency `"<amount> <CCY>"`, joined with `" · "`, empty when
  there are none. Amounts use the table's existing number formatting.
- Modify: the positions table (find it under `cockpit/src/trade/` or
  `cockpit/src/pages/`). Realized P&L is unchanged in code (already net).
  Add an **Other fees** column, formatted the same way.
- Modify: the cockpit order detail or events view, if one exists, so the
  `order_fee_charged` row is readable.
- Create: `docs-site/src/content/docs/concepts/fees.md`, covering:
  - the three effects, with the BTCUSDT, BNB and CAD-stock examples;
  - rebates;
  - running-total venues;
  - IBKR's late commission;
  - no backfill;
  - no FX.

  Link it from the sidebar and from each connector page's notes.
- Rebuild the cockpit: `cd cockpit && npm run build` before `cargo build`.

- [ ] **Step 1: Write the failing test.** `cockpit` node test
  `formatFees joins per-currency amounts`: `formatFees([{currency:"USD",amount:1.5},{currency:"BNB",amount:0.01}]) === "1.50 USD · 0.01 BNB"`.
  `formatFees([])` returns `""`. Put `formatFees` in a small `.ts` module
  next to the blotter, following `ibkrImport.ts`.
- [ ] **Step 2: Run** `cd cockpit && npm test` → FAIL.
- [ ] **Step 3: Implement** the columns and the docs page.
- [ ] **Step 4: Run** `cd cockpit && npm test`, `cd docs-site && npm run check`, and `cargo test --workspace` → all PASS.
- [ ] **Step 5: Live check.** Testnet fees are 0, so this checks the
  plumbing only. The user enters any credentials themselves.
  - Binance testnet: one fill. Its `order_fee` row is either absent (zero fee
    sends nothing) or present. The blotter renders.
  - IBKR paper: one market fill of 1 share. A USD commission appears in the
    Fees column, and realized P&L moves by the commission when the position
    is closed.

  Record the results in the commit message.
- [ ] **Step 6: Commit** `feat(cockpit): fee columns on orders and positions; fees docs page`
