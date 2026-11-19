# External orders & execution reconciliation — design

Date: 2026-11-18 · Status: draft for approval

## Intent

The OMS must book what actually happens on every account it is connected to,
not only the orders it placed itself. Manual orders (Binance web, TWS GUI,
Alpaca dashboard), orders from other systems (path2 strategies, co-located
strategies) and fills missed during a disconnect must all end up in the book,
exactly once, through one mechanism that every connector shares.

This comes before new venues (Polymarket): once it exists, a drop-copy venue
is just a connector that emits events and has no outbound order path.

## Today

- Connectors parse venue reports and send `ExecEvent`s keyed by the OMS order
  UUID to the engine actor via `EngineHandle` (`crates/gateway/src/contract.rs`).
  A report whose client id is not an OMS UUID is dropped (Binance stream,
  `fix/dialect.rs`) or treated as a fault that marks health down and triggers
  recon (`ibkr_tws/stream.rs:74 unknown_order`).
- `src/recon_orders.rs` diffs broker open orders against OMS working orders on
  reconnect/heartbeat. It only knows OMS-routed orders; `OrphanAtBroker` is
  logged and ignored. It applies fills only at terminal state because booking
  has **no per-fill idempotency** (the aggregate does not dedupe on
  `execution_id`).
- The colo follow path (`src/colo/booking.rs`, `FollowEvent`, `ExternalFill`)
  already adopts external Binance fills as `Origin::External` orders, with a
  `myTrades` gap-fill on reconnect — but Binance-only, colo-only, and one OMS
  order **per trade**.
- The in-process `EventBus` and Kafka outbox carry committed events outbound
  only.

## Goals

1. Every fill on a connected account is booked exactly once, whether it
   arrives by stream, by pull, or both.
2. Orders the OMS did not place are adopted as real orders (one OMS order per
   venue order) into a book chosen by the connection's attribution rules,
   falling back to the connection's **default portfolio**.
3. Gaps from disconnects heal automatically from a pull-since-cursor.
4. One shared path: connectors do venue I/O only; matching, dedup, adoption,
   cursors and scheduling live in one place.
5. Adopted orders are visible and cancelable from the OMS.

## Non-goals

- Position / cash / custodian reconciliation (stays out of the OMS —
  book-of-record concern).
- Pre-trade risk on external orders (impossible: they already happened).
  Post-trade limit checks may alert on them as on any position change.
- Amending external orders from the OMS.
- A separate reconciliation service or an inbound message bus.
- Polymarket itself (follows as a separate project).

## Anti-goals

- A fill booked twice, or a stream/pull race that double-counts.
- An external fill silently dropped (every unmatched event is booked or
  becomes a visible break, never only a log line).
- Per-connector variants of adopt/dedup logic creeping back in.

## Constraints

- Single-user prerelease: no compat shims; the colo follow path and
  `recon_orders.rs` are replaced, not wrapped. Existing colo
  one-order-per-trade rows may be left as history or wiped (builder's call
  with a migration note).
- Engine actor stays the single writer of order state.
- Venue rate limits: pulls must be paced per connection.

## Approaches considered

- **A. Separate recon service.** Own venue connections or a bus carrying raw
  venue events. Double WS/rate-limit usage, new deploy unit, cross-process
  ordering. Right at multi-instance scale; overhead here. Rejected.
- **B. Everything in each adapter.** Today's shape; produced three divergent
  behaviors (drop / fault / colo path). Rejected.
- **C. Adapters do venue I/O; a central in-process reconciler schedules pulls
  and feeds one idempotent ingest path in the engine. — chosen.** Inbound
  needs no bus: `EngineHandle` is already the single-writer pipe. If the
  system is ever split, the adapter/reconciler boundary is the seam.

## Design (decided)

### 0. One ingest path

`VenueEvent` replaces today's `ExecEvent`/`VenueReport` flow for OMS-placed
orders too: every connector emits only `VenueEvent`s, ingest matches them.
One path, one dedup.

Unknown instruments are auto-added from the venue's own instrument list (as
colo's `ensure_instrument` does today); not listed → break.

### 1. Adapter contract (gateway crate)

Each connector exposes, in addition to `BrokerAdapter`'s order entry:

- **Push:** a stream of normalized `VenueEvent`s for the whole account:
  - `OrderUpdate { venue_order_id, client_order_id: Option, symbol, side,
    order_type, qty, limit_px, status, cum_qty, ts }`
  - `Fill { exec_id, venue_order_id, client_order_id: Option, symbol, side,
    qty, price, fee: Option<(amount, ccy)>, ts }`
  - `Connected` / `Disconnected`
- **Pull:** `fills_since(cursor) -> (Vec<Fill>, next_cursor)` and
  `open_orders() -> Vec<OrderUpdate>`.
- **Hints:** default poll interval, page size, whether the stream is
  account-wide.

Generalizes `FollowEvent` / `ExternalFill`, which are removed.
Per-venue sources: Binance user stream + `myTrades`; Alpaca `trade_updates` +
account activities (FILL); Bybit private `order`/`execution` +
`/v5/execution/list`; OKX `orders` + `/trade/fills`; Coinbase `user` +
historical fills; IBKR TWS `openOrder`/`orderStatus`/`execDetails` +
`reqExecutions`; IBKR FIX: own orders only (drop-copy session out of scope).

IBKR TWS must connect as the **master API client id** (configurable, replaces
hardcoded `CLIENT_ID = 1`) so manual/other-client orders are visible.

### 1b. Accounts and multi-account connectors

`account` stays: it is the venue's own account number under a connection.
Each connector declares `multi_account: bool`:

- **IBKR (TWS; FIX once it sends tag 1 `Account`)**: `true`. One login
  (master credentials) trades several venue accounts (U1, U2…); the account
  ref on each order selects which. The UI shows an Accounts list on such
  connections.
- **Binance, Bybit, OKX, Coinbase, Alpaca**: `false`. An API key belongs to
  exactly one account (Binance sub-accounts need their own keys and so are
  separate connections). The UI hides accounts; one is auto-created per
  connection.

IBKR FIX must send tag 1 from the order's `external_account_ref` (today it is
omitted, so all orders hit the login's default account).

### 2. Reconciler (OMS, one task per broker connection)

- Consumes the connector's push stream and forwards events to ingest.
- Pulls run per `(connection, account)`.
- Three layers, by window:
  - **Stream**: real time.
  - **Intraday pull**: `fills_since(cursor)` + `open_orders` on `Connected`,
    on a per-connection timer (default from adapter hint), and on demand.
    Window = since cursor only; a new account's cursor starts at "now".
  - **End-of-day sweep**: pulls fills over a user-chosen lookback — 1 day,
    1 week, 1 month, 1 year, 5 years, 10 years — per connection. Dedup makes
    the overlap harmless; it books only what the other layers missed. The
    sweep reports the window actually obtained, since venues cap history
    (e.g. IBKR `reqExecutions` ~7 days, Binance `myTrades` per symbol).
    Statement/flex-report import for older history is a non-goal.
- Owns cursors in a table `recon_cursor (broker_connection_code, account_id,
  scope, cursor, updated_at)`; `scope` covers venues that page per symbol
  (Binance `myTrades`).
- Advances a cursor only after ingest has committed the pulled page.
- Coalesces overlapping pull requests (as the engine does for reconcile
  today).

### 3. Ingest (engine actor)

New `GatewayMsg::Venue(connection, account, VenueEvent)`. For each event:

1. **Match the order:** client id parses as an OMS UUID with an existing
   order → that order. Else `(venue, account, venue_order_id)` lookup → that
   order. Else apply the account's **origin policy**.
2. **Attribution** (several books trade through one connection):
   1. Attribution rules on the connection: match on account and/or client
      order id prefix/tag → book (e.g. `p2-` → path2 book). External
      systems must tag their orders to be attributed.
   2. Else the connection's `default_portfolio_id` (manual GUI orders land
      here).
   3. Else, or if the connection's policy is `break`: record a break.
   An adopted order is created as `Origin::External`: submitted + routed
   with the venue order id, no risk check, then the event is applied.
   A break is keyed by venue order; later events for that order attach to
   it, and resolving it adopts the whole order and replays its events.
3. **Dedup fills** on `(venue, account, exec_id)` via a unique constraint
   (new `order_fill` table or unique index on the fill event — builder's
   call). A duplicate is a no-op. This makes stream/pull overlap and recon
   fill application safe at any time, removing the terminal-only restriction
   of today's recon.
4. **Order status**: open-order snapshots close OMS orders the venue no longer
   lists (as `ResolveTerminal` does today) and update adopted orders' qty /
   status.
5. Publish committed events on the `EventBus` as today; adopted orders and
   breaks flow to Kafka/UI like any order event.

Adopted order quantity: from the venue's `OrderUpdate` when seen; if the
first thing seen is a `Fill` (pull, or venue without order updates), the order
is created with qty = filled qty and grown by later updates. *(My call.)*

### 4. Schema

- `broker_connection.default_portfolio_id UUID NULL REFERENCES portfolio`,
  `broker_connection.external_policy TEXT NOT NULL DEFAULT 'adopt'`
  (`adopt`|`break`), `broker_connection.eod_lookback` (enum of the six
  windows).
- `attribution_rule (id, broker_connection_code, account_id NULL,
  client_id_prefix NULL, portfolio_id, priority)`.
- Fill-dedup migration seeds the unique key from existing fill events'
  `execution_id`s, so overlapping pulls can't re-book old fills.
- Fees keep their own dedup key (as today), so a commission arriving after
  its fill (IBKR `commissionReport`) books once.
- Unique `(venue, account_id, venue_order_id)` on `order_state` where
  `venue_order_id` is set.
- Fill uniqueness on `(venue, account_id, exec_id)`.
- `recon_cursor` (above).
- `recon_break (id, broker_connection_code, account_id, kind, venue_order_id,
  exec_id, payload_json, status open|resolved, created_at, resolved_at,
  resolved_by)`.

### 5. Breaks

A break is any event ingest could not book: policy `break`, adopt without a
default portfolio, unknown instrument that cannot be auto-added, venue
fill > order qty, etc. Breaks are listed in the cockpit; resolving = adopt
into a chosen portfolio or dismiss. *(Minimal UI: list + two actions — my
call.)*

### 6. Cancel

Adopted orders are cancelable from the OMS by venue order id via the existing
`cancel_order`. Amend is not offered.

### 7. Removals

- `src/colo/booking.rs` fill-adoption and the `FollowEvent` path → colo
  accounts become ordinary accounts with `adopt` + default portfolio = the
  strategy's portfolio.
- `src/recon_orders.rs` diff → reconciler + ingest.
- Per-connector unknown-order handling (`unknown_order`, UUID-or-skip).

## Known gaps

- **Per-symbol venues (Binance): an unseen pair's round trip.** Pulls are per
  symbol. The scopes are the account's open-order symbols, the symbols the OMS
  has orders on for the account, and (on reconnect and in the EOD sweep) every
  catalog pair whose base asset the account holds a non-zero balance of. A
  pair the OMS has never seen, bought and sold back to zero entirely while
  the stream was down, leaves no open order, no OMS order and no balance, so
  no pull finds its fills. A pair that is found (by balance discovery on
  reconnect, or newly traded) on an account that already has cursors is
  pulled at once from the account's oldest cursor stamp, not from "now".
- **Pre-upgrade Binance fills are not re-read.** Migration 0066 re-keys legacy
  ledger ids to today's formats where it can (IBKR TWS, Alpaca, OKX, Binance
  REST); Binance FIX keyed fills on `ExecID`(17), which is indistinguishable
  from a trade id, so Binance connections with legacy fills (and any
  connection with an id that could not be re-keyed) get
  `recon_not_before` = upgrade time and pulls/sweeps drop older fills.
- **One Binance sub-account per environment** (`broker_connection` is
  `UNIQUE (broker_code, environment)`): co-located sub-accounts share the main
  connection and are pulled by their own follow reconciler.

## Left to the builder

- Exact Rust module layout and trait names.
- Fill-dedup storage form (table vs unique index).
- Per-venue cursor encoding and paging.
- Order of connector migration (suggest Binance → Alpaca → IBKR TWS → rest).
- Whether to keep or wipe historical colo per-trade orders.
- Testing approach per connector (recorded venue payloads exist in
  `connectors/*/testdata`).
