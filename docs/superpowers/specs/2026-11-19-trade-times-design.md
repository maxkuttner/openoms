# Trade times & external order lifecycle — design

Date: 2026-11-19 · Status: draft for approval

## Intent

Reconciled (external) orders currently look like they happened when the OMS
reconciled them: every event is stamped with the booking time, and an adopted
order shows a fabricated `submitted → routed → filled` history at one instant.
Positions and realized P&L are built in booking order, so a late-reconciled
fill lands in the wrong place. The user needs the venue's real times, an honest
history, and positions that follow trade time — "you own it from the moment it
traded".

## Goals

1. Every fill carries its **trade time** (venue execution time) separately from
   its **booking time** (when the OMS recorded it). Nothing overwrites the
   booking time.
2. The orders list shows, per order: **Placed**, **Trade time**, **Booked**,
   **Updated** (definitions below). Order history shows each step's venue time
   with its booking time beside it.
3. External orders start their lifecycle at what the OMS first observed
   (working, partially filled, or filled) — no fabricated submit/route steps.
4. Positions and realized P&L follow trade-time order, including when a fill
   arrives late (out of order).
5. Positions show the instrument name (separate small fix).

## Non-goals

- Settlement, value, accounting, valuation dates (book-of-record concerns, out of
  the OMS per the lean recon scope).
- Effective date (instrument-dependent meaning; not stored).
- As-of position/P&L reports (enabled later by stored trade times; not built).
- Day P&L intraday correctness (today = net_qty × (mid − prev_close), ignores
  intraday fill prices; existing daily-stats design debt, unchanged).
- Moving to a fills-table-as-truth model (approach B).

## Anti-goals

- Changing event append order or `global_position` semantics (Kafka feed and
  replays depend on booking order).
- A rebuild that gives a different position than the incremental path for the
  same fills.

## Definitions

| Name | On a fill | On an order |
|---|---|---|
| Trade time | venue execution time (`VenueFill.executed_at`) | **Trade time** column = last fill's trade time (`order_state.last_trade_at`) |
| Booking time | when ingest committed the fill (existing event `created_at`/metadata timestamp) | **Booked** = when the OMS first recorded the order (existing `order_state.created_at`) |
| Placed | — | external: venue create time where the API gives it (`order_state.venue_created_at`); OMS orders: our submit time (`created_at`) |
| Updated | — | existing `order_state.updated_at` |

If a venue gives no execution time for a fill, trade time = booking time and the
fill is flagged `trade_time_estimated` (shown as such in the UI).

## Approaches considered

- **A. Venue time on events + observed lifecycle + rebuild on late fill —
  chosen.** Additive: `occurred_at` on order events, `order_observed` entry event
  for external orders, positions incremental with a scoped rebuild when a fill is
  older than the latest applied fill on that position.
- **B. Fills table as source of truth.** Cleanest (trade-time order by
  construction, as-of trivial) but rewrites positions/Kafka position/fee
  production; too large right after a release. Remains possible later because A
  stores trade time on every fill.
- **Overwrite event time with venue time.** Rejected: breaks append order/audit.

## Design (decided)

### 1. Storage

- `order_event.occurred_at TIMESTAMPTZ NULL` — venue time of what the event
  records (fill execution time, venue order update time). NULL for events the
  OMS originates (submit, route, cancel request) — their booking time is the
  real time.
- `order_state.last_trade_at TIMESTAMPTZ NULL`, `order_state.venue_created_at
  TIMESTAMPTZ NULL`, `trade_time_estimated BOOLEAN` on fill events' payload.
- Index for the orders list sort on `last_trade_at`.

### 2. Ingest carries venue times

- `VenueFill.executed_at` → fill event `occurred_at` (all connectors already
  produce it on pulls; streams where the venue sends it).
- `VenueOrderUpdate` gains `created_at: Option<DateTime>`; connectors fill it
  where the API provides it (Binance `time`/`O`, Alpaca `created_at`, Bybit
  `createdTime`, OKX `cTime`, Coinbase `created_time`; IBKR none). Its `ts`
  (update time) → `occurred_at` on status events.
- `order_state.last_trade_at` = max fill `occurred_at` (or booking time when
  estimated).

### 3. External order lifecycle

- New domain event `order_observed` (and command) creating an `Origin::External`
  order directly in the observed status: Working, PartiallyFilled or Filled,
  with observed qty/cum. Subsequent fills/updates use the normal events.
- Adoption (ingest adopt path and break-adopt replay) uses `order_observed`
  instead of SubmitOrder+RouteOrder. OMS-placed orders unchanged.
- Order history UI renders `order_observed` as "Observed at venue" (with venue
  create time when known), never "Submitted/Routed".
- Legacy adopted orders (pre-change, with fabricated submit/route) stay as they
  are; no data rewrite.

### 4. Positions in trade-time order

- Fill application stays incremental (`positions::persist_fill`).
- Late fill: if the fill's trade time is earlier than the position's latest
  applied fill trade time (`position.last_trade_at`, new column), rebuild that
  one position (portfolio, instrument) in the same transaction by replaying its
  fill, fee and transfer effects ordered by `COALESCE(occurred_at, created_at),
  global_position` — extending the existing `rebuild_positions_scoped` to a
  per-position, trade-time-ordered variant.
- Fees attach to their fill's trade time; transfers (colo) use their own
  occurred time.
- Property test: the same set of fills/fees in any arrival order yields the
  same position (qty, avg cost, realized P&L) within EPS.
- Position events published to the bus/Kafka after a rebuild reflect the final
  state (one PositionChanged).

### 5. UI

- Orders list (blotter): columns Placed, Trade time, Booked, Updated; default
  sort Trade time desc (fallback Booked). Estimated trade times marked.
- Order history: each row shows venue time (occurred_at) and booking time.
- Positions: show instrument name/symbol (join instrument), id in tooltip.
- API: order list/detail responses include the new fields.

## Constraints

- Prerelease, single user: no compat shims; additive migration (next number
  after 0066).
- Engine actor stays single writer; rebuild runs inside the fill transaction.
- DB tests `--ignored --skip real_docker`.

## Settled details (designer's calls)

- **Estimated trade time** is stored as `occurred_at = booking time` plus the
  `trade_time_estimated` flag — never NULL for fills booked after this change.
  NULL `occurred_at` only on legacy events; ordering falls back to `created_at`.
- **Canonical fill order** for rebuilds: `(trade time, execution_id,
  global_position)`. Trade time = `COALESCE(occurred_at, created_at)`;
  `execution_id` is already in the fill payload. The property test asserts equal
  positions for any arrival order of fills with distinct `(trade time,
  execution_id)`.
- **Legacy and new fills mixed** in one position is fine: legacy fills sort by
  booking time. Rebuild must include every input the current incremental path
  applies (fills, fees, colo transfers) — the builder locates fee/transfer
  sources (`rebuild_positions_scoped`, `positions::persist_fee`,
  `persist_transfer`) and orders fees at their fill's trade time, transfers at
  their own time.
- **`order_observed` carries no fills.** It records venue status, qty, limit
  price, venue create time and venue-reported cum as information
  (`venue_cum_qty`); the order's booked `cum_qty` starts at 0. Positions move
  only through fill events, which follow (from the same ingest batch or the
  fill_gap-triggered pull). So an observed "Filled" order may briefly show
  booked cum < venue cum until its fills land.
- **Kafka / bus**: the new event type and `occurred_at` flow through the
  existing outbox unchanged; no external consumers to keep compatible
  (prerelease, single user).
- **`venue_created_at`** is set once (first value seen), never overwritten.
  **Placed** shows it; when unknown (IBKR, OMS-less), shows Booked marked
  "first seen".
- **Rebuild publishes one PositionChanged** with the final state; no size bound
  needed at current volumes, but log rebuild duration and fill count.

## Left to the builder

- Exact event/command names and payload field names.
- Whether `last_trade_at` on position is a new column or derived per rebuild
  check.
- Per-connector field extraction details for venue create/update times.
- Cockpit component structure.
