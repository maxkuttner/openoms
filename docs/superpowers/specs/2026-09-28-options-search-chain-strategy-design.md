# Instrument search, option chain & strategy builder — design

## Context

The trade dashboard's only instrument picker today is `InstrumentSelect` — a
flat symbol/name search (`GET /instruments` → `search_instruments`,
`src/instruments_api.rs:27`) feeding a single-instrument `OrderTicket`. There
is no way to browse an underlying's option chain or build a multi-leg
strategy (straddle, vertical spread, custom combo); a trader who wants
options today would have to already know the exact OCC symbol.

A companion design artifact (mockup) explored this as four screens: universal
search → underlying hub with strategy templates → option chain → strategy
ticket. Implementing it required first understanding what data actually
backs it:

1. **Instruments and their strike/expiry (`instrument_derivative`) are real**
   and already seeded from a broker sync (`src/setup/catalog.rs`) or, since
   [`2026-09-28-part1`], `oms setup seed-test-chain` — a synthetic SPY
   equity + 210-contract option chain (5 expiries × 21 strikes × call/put),
   reusing the same `catalog::upsert_catalog` path. No new table, no
   migration.
2. **Bid/ask is real** — `GET /marks?instrument_ids=…` (`src/handlers.rs`)
   already returns live top-of-book per instrument. **IV, greeks and open
   interest have no data source anywhere in the codebase** — they stay out
   of scope entirely rather than showing fabricated numbers.
3. **There is no multi-leg order concept in the domain.** `order_state` /
   `src/domain/orders/aggregate.rs` know only single-leg orders. Building a
   combo-order aggregate was explicitly ruled out (see Non-goals) in favor
   of linking independently-submitted legs.
4. **The trade dashboard is a single page, not a router.** `TradePage`
   (`cockpit/src/trade/pages/Trade.tsx`) is one `Grid` (Watchlist |
   OrderTicket | Blotter tabs) with no navigation; `TradeApp`
   (`cockpit/src/trade/App.tsx`) has exactly one route. The mockup's
   page-per-screen flow is reshaped here into a modal with internal steps,
   not new routes.
5. **`client_order_id` is already persisted** on every order
   (`order_state.client_order_id`, bound at `src/handlers.rs:648`) but is
   never selected by the blotter query (`BlotterRow`, `src/handlers.rs:1785`
   omits it). It is free text, not used for broker routing or
   reconciliation (broker-side idempotency uses `order_id`, not this field —
   see `src/adapters/alpaca.rs:319`), so it is safe to repurpose as a
   shared tag across a strategy's legs.

## Goals

- A trader can search for any instrument and, for one that has options,
  browse its live chain by expiry/strike.
- A trader can build a strategy either from a named template (Long
  Call/Put, Straddle, Strangle, Vertical Spread) or by hand-picking any
  combination of legs off the chain ("Custom Combo").
- Submitting a multi-leg strategy sends each leg as its own independent,
  existing single-leg order (`POST /orders/submit`), sharing one
  generated `client_order_id` so the legs are identifiable as one trade
  after the fact, from the blotter.
- A single-instrument pick (equity, or one option leg) behaves exactly as
  `OrderTicket` does today — unchanged code path, unchanged risk profile.

## Non-goals

- **A backend multi-leg/combo order concept.** No new domain aggregate, no
  atomic all-or-nothing routing, no `src/domain/orders/aggregate.rs`
  changes. Legs are independent orders from the moment they're submitted.
- **IV, greeks, open interest, or any options-pricing computation.** No
  data source exists; not approximated, not displayed.
- **Covered Call and Iron Condor templates.** Covered Call mixes an equity
  leg (different qty/lot-sizing) with an option leg; Iron Condor is 4 legs.
  Both are real added complexity for a first cut and are left out — the
  "Custom Combo" path can still build either by hand.
- **New routes / URL-addressable screens.** The flow is a modal over the
  existing single-page dashboard, not a rework of `TradeApp`'s routing.
- **Partial-fill-aware or all-or-nothing leg submission.** If leg 2 of 3
  fails after leg 1 succeeded, leg 1 stays submitted; nothing rolls it
  back. The ticket reports this honestly (see Error handling) — an atomic
  combo is a backend project this design explicitly defers.

## Architecture

### Backend

**1. Grouped search.** `InstrumentSummary` (`src/admin.rs:2521`) and
`search_instruments` (`src/instruments_api.rs:27`) gain two fields:
- `instrument_class: String` — already a column, just added to the SELECT.
- `has_options: bool` — `EXISTS (SELECT 1 FROM instrument_derivative d
  WHERE d.underlying_id = instrument.id OR d.underlying_symbol =
  instrument.symbol)`.

The trade webapp's search UI groups the flat result list client-side
(Underlyings-with-options / Equity / Futures) — no new endpoint.

**2. Chain endpoints**, new handlers in `src/instruments_api.rs`, mounted
next to the existing `/instruments` route (`src/main.rs:1275`):
- `GET /instruments/options/expiries?underlying=SPY` → sorted, distinct
  `expiry_date` values for that underlying (status `ACTIVE` only).
- `GET /instruments/options/chain?underlying=SPY&expiry=2026-10-17` →
  strike-sorted rows: `{ strike, call: { instrument_id, symbol } | null,
  put: { instrument_id, symbol } | null }`, built by a self-join on
  `instrument_derivative` keyed on underlying + expiry + strike (a strike
  can be call-only or put-only if the venue never listed the other side —
  hence nullable).

The frontend calls the existing `/marks?instrument_ids=…` with every
`instrument_id` in the chain response, batched into one request, for live
bid/ask. Rows with no active feed simply show blank cells — `MarkRow`'s
`Option<f64>` fields already model this.

**3. Ex-post leg linking.** `BlotterRow` (`src/handlers.rs:1785`) and
`load_blotter`'s SELECT (`src/handlers.rs:1826`) gain `client_order_id:
String` (already a column on `order_state`, just not currently selected).
`BlotterFilter` gains an optional `client_order_id` field, applied as
`AND os.client_order_id = $n` — "show this trade's other legs" is then a
real, server-side filter, not a client-side scan. No migration.

### Frontend

**Shared submit logic.** `OrderTicket.tsx`'s `confirmSubmit` (L285–400)
already contains the correct, comment-documented handling for every
response the server can give (`409` idempotent replay, `422` rejection,
`502` broker-rejected-but-recorded, network errors). That logic is
extracted verbatim — not rewritten — into
`cockpit/src/trade/api/submitOrder.ts` as a single function:

```ts
submitOrder(params: {
  orderId: string; clientOrderId: string; portfolioId: string;
  accountId?: string; instrumentId: string; side: Side; quantity: number;
  orderType: OrderType; timeInForce: TimeInForce; limitPrice?: number;
}): Promise<SubmitOutcome>
// SubmitOutcome = { kind: "sent" } | { kind: "recorded_not_routed"; message: string }
//               | { kind: "rejected"; message: string } | { kind: "replayed" }
```

`OrderTicket` is refactored to call this instead of inlining the fetch —
same branches, same UI behavior, verified by its existing tests/manual
flow. This is the only change to `OrderTicket.tsx`.

**`InstrumentSearchModal`** (new, `cockpit/src/trade/components/
InstrumentSearchModal.tsx`) — a full-screen Mantine `Modal` with three
internal steps (component state, not routes):

1. *Search* — text input, `GET /instruments` grouped client-side into
   Underlyings / Equity / Futures sections, mirroring the search mockup.
2. *Underlying* — the picked instrument's quote header; if `has_options`,
   strategy template cards (Long Call, Long Put, Straddle, Strangle,
   Vertical Spread, Custom Combo — see Non-goals for the two dropped).
   Picking a template fetches the nearest expiry
   (`/instruments/options/expiries`, first result) and the ATM strike's
   chain row, and pre-populates the leg list before jumping to step 3.
   Picking "Custom Combo" jumps to step 3 with an empty leg list. An
   instrument with no options closes the modal immediately, returning it
   as a single-leg pick (same as a Watchlist click today).
3. *Chain* — expiry pills + strike/call/put table
   (`/instruments/options/chain` + batched `/marks`), click a Bid/Ask cell
   to add/remove that leg. This step doubles as leg review — a running leg
   count and net debit/credit is always visible (no separate "builder"
   screen). "Use this combo" closes the modal, returning `legs: Leg[]`.

**`StrategyTicket`** (new, `cockpit/src/trade/components/
StrategyTicket.tsx`) — renders in `TradePage`'s existing order-ticket
`Grid.Col` **only** when the modal returns more than one leg; a single-leg
result still flows into the unmodified `OrderTicket`, exactly like a
Watchlist click does today. Shows the leg list (kind/strike/expiry/side,
qty, remove), one shared portfolio/account/order-type/TIF, and a computed
net debit/credit. On confirm: generates one `client_order_id` (`combo-
<8 hex chars>`), calls `submitOrder` once per leg in sequence tagged with
it, and renders each leg's own `SubmitOutcome` as it resolves.

**`TradePage` wiring** (`cockpit/src/trade/pages/Trade.tsx`): a new
"Search instruments" button opens `InstrumentSearchModal`; its result sets
either `selectedInstrument` (existing state, → `OrderTicket`) or a new
`pendingLegs` state (→ `StrategyTicket`), with a small affordance to clear
`pendingLegs` and fall back to `OrderTicket`.

**Blotter.** `BlotterRow`'s TS type gains `client_order_id`; `TradeBlotter`
shows it as a small muted tag on rows where it looks like a generated combo
tag (`combo-` prefix) — enough to visually group a strategy's legs without
new UI chrome.

## Data flow

```
Search (GET /instruments, grouped)
  → pick underlying with options
    → pick template (auto-fills legs) — or — Custom Combo (empty)
      → Chain step: GET /instruments/options/expiries, /options/chain,
        batched GET /marks for live bid/ask
        → click legs on/off → "Use this combo"
          → TradePage: legs.length > 1 ? StrategyTicket : OrderTicket
            → StrategyTicket confirm: generate client_order_id once
              → submitOrder() per leg, sequential, same tag
                → Blotter: each leg its own row, client_order_id visible
```

A single-instrument pick (equity, or an underlying with no options) exits
the modal at step 2 and never touches the chain endpoints or
`StrategyTicket` at all.

## Error handling

- Chain/expiries fetch failure: shown inline in the modal (Mantine `Alert`),
  same pattern as existing `notifyError` usage elsewhere in the trade app.
- `/marks` failure for the chain: bid/ask cells fall back to blank (same
  contract as `MarkRow`'s nullable fields today) — never blocks adding a
  leg.
- Multi-leg submit: legs are submitted **strictly sequentially**, not in
  parallel — leg *n+1* is only sent once leg *n*'s `submitOrder` call has
  resolved. Each leg's own `SubmitOutcome` renders next to it (sent /
  recorded-not-routed / rejected). A rejected or failed leg does **not**
  cancel or roll back legs already sent — consistent with the Non-goals
  (no atomicity) — and does **not** block attempting the remaining legs;
  the trader sees exactly which legs went through and which didn't, and
  can retry a failed leg individually (it's just another `OrderTicket`-
  style submit, addressable from the blotter by its shared
  `client_order_id`).

## Testing

- Backend: unit tests on the `has_options` SQL (an instrument with and
  without derivative rows), the two chain endpoints (empty underlying,
  call-only strike, full call+put row, expired contracts excluded), and
  `load_blotter` returning/filtering `client_order_id` — following the
  existing patterns in `src/instruments_api.rs`'s `mod tests`
  (`test_pool`, `seed_instrument`).
- Frontend: no test harness currently exists for the trade webapp beyond
  manual verification (consistent with `OrderTicket.tsx`/`TradeBlotter.tsx`
  having none today) — verified by running the dev server against the
  seeded SPY chain and walking search → straddle → submit → blotter tag.

## Open questions for the implementation plan

- Exact Mantine components/layout for the modal steps (left to the plan/
  implementation, not the architecture).
- Whether the "combo-" blotter tag should be a clickable filter (jumps the
  blotter to `?client_order_id=…`) in v1, or just a visible tag — leaning
  visible-only for v1, filter is a cheap follow-up once the field exists.
