# Futures via IBKR (CME equity index, front month, one-click roll)

## Intent

Make the OMS a credible multi-asset tool. The target set is **crypto, equities,
options and futures**. Crypto (five venues), equities (Alpaca, IBKR) and options
(Alpaca) exist; futures do not. This step adds futures through IBKR, starting with
CME equity index contracts, and generalises the IBKR connector so options through
IBKR (and later futures options) come out of the same work.

Fixed income and FX are parked: maybe later, not now. Perpetual futures (Binance,
Hyperliquid) are expected later; this design must not block them.

## Goals

- **Trade ES, MES, NQ on CME through IBKR.** Contract discovery, order routing,
  fills, positions and P&L with the right multiplier.
- **Front month by root.** The trader picks "ES"; the OMS resolves it to the current
  front contract (e.g. ESZ6) and switches to the next one on the roll date.
- **Roll warning.** A job notices positions in a contract approaching its roll date
  and emits a warning event (stored, on Kafka, pushed to the cockpit).
- **One-click roll.** From a position in an expiring contract, one action sends a
  single calendar-spread order (sell old, buy next — or the reverse for shorts) so
  both legs fill together.
- **Generic IBKR contracts.** Contract lookup, routing and fill mapping work for any
  IBKR security type, not only stocks. FUT is wired end to end; OPT is supported by
  the lookup/routing layer (see Non-goals for how far).
- **Adding a root is data, not code** (e.g. CL, GC later): one row in the root table
  plus, if needed, an exchange→MIC mapping.

## Non-goals

- **Automatic rolls.** The OMS never rolls a position by itself. Decided.
- **Cash, margin, variation margin, daily settlement.** The OMS tracks positions and
  P&L only; cash and margin are the book of record's concern (existing decision, see
  `src/expiry.rs` module doc and the lean-recon scope). Futures P&L is
  `(mark − avg) × qty × contract_size`, exactly as options.
- **Expiry/settlement handling of held positions.** As for options: the broker
  cancels orders and settles the position; the OMS receives those events and
  preflight reports positions stranded in an expired contract. Nothing new.
- **Live CME market data.** IBKR delayed data, as today. Databento GLBX is a later
  market-data upgrade and must slot in without touching anything else here.
- **IBKR option chains in the cockpit.** The generic lookup must be able to resolve
  and route an OPT contract, but chain browsing for IBKR options, OPT sync and FOP are
  follow-ups.
- **Perpetuals, funding payments, inverse (coin-margined) contracts**, Binance
  futures and Hyperliquid connectors.
- **Fixed income, FX, CFDs, warrants.**
- **General multi-leg orders** (option combos, arbitrary spreads). Only the
  two-leg calendar roll of a future is in scope.

## Anti-goals

- An OMS that places orders nobody clicked.
- A design where adding CL or GC needs a code change in the connector.
- Schema or valuation choices that would have to be undone for perpetuals (no
  "every derivative expires" assumption, no single hardcoded P&L formula).

## Constraints and facts from the codebase

- `instrument.instrument_class` already allows `FUTURE`;
  `instrument_derivative` carries `underlying_symbol`, `expiry_date`, `expires_at`;
  `contract_size` already scales risk (`src/risk_engine.rs:73`) and valuation
  (`src/positions.rs:104`). No schema change is needed to *store* a dated future.
- `src/expiry.rs` runs hourly, derives `expires_at` from `expiry_date` + the venue
  calendar's close time, and marks passed contracts `EXPIRED`.
- The IBKR TWS connector is stock-only: `ContractQuery` is documented "sec type is
  always STK" (`crates/gateway/src/connectors/ibkr_tws/client.rs:137`),
  `stock_contract()` builds every outgoing `Contract` (`client.rs:392`),
  `to_broker_instrument()` hardcodes `EQUITY`/`SPOT` and `contract_size: 1.0`
  (`resolve.rs:119`), and the exchange→MIC table has stock exchanges only
  (`exchanges.rs`).
- IBKR routes by conId; `broker_instrument.native_id` holds it.
- IBKR executions carry `contract.symbol`, which for futures is the **root** ("ES"),
  shared by every expiry. Fill/order mapping must key on conId or `localSymbol`
  ("ESZ6"), never `symbol`. `TwsExecution` today only carries `symbol`
  (`client.rs:124`) — this is the most likely silent bug.
- The OMS order model is single-instrument. The live socket (`UiNotice`,
  `src/live_updates.rs:41`) only sends refresh hints; the event store and Kafka
  outbox are order-scoped (`orders` and `fills` topics). There is no alert/warning
  channel today.
- Natural `code` PK for catalog/config tables (project convention).
- Single user, pre-release: no compatibility shims needed.
- Cockpit UI uses shadcn/ui only.

## Approaches

1. **Generic IBKR contracts + futures roots (chosen).** IBKR lookup/routing becomes
   generic over security type; a `future_root` table maps ES/MES/NQ to specs and a
   roll rule and resolves the front month; the hourly expiry job also raises roll
   warnings. New roots are rows; IBKR options fall out of the same lookup work.
2. **Futures-only bolt-on.** Hardcode three roots, add FUT only. Faster, but options
   stay Alpaca-only and each new root is a code change — against the multi-asset
   goal. Rejected.
3. **Approach 1 + Databento GLBX for CME data and reference.** Live quotes without an
   IBKR market-data subscription, but a second vendor relationship now. Deferred as
   an upgrade; approach 1 must leave room for it.

Picked by the user: approach 1, IBKR delayed data.

## Design

### 1. Reference data

- **Venue** `XCME` (CME Globex) via a migration like `0025_INSERT_COINBASE_VENUE.sql`,
  plus its calendar row (America/Chicago). Exchange map gets `("CME", "XCME")`.
  NQ and ES are both CME (not CBOT), so one venue covers the first three roots.
- **New table `future_root`** (natural PK `code`):

  | column | example | note |
  |---|---|---|
  | `code` | `ES` | PK, the root the trader types |
  | `venue` | `XCME` | FK `venue(code)` |
  | `name` | E-mini S&P 500 | |
  | `asset_class` | `INDEX` | |
  | `currency` | `USD` | |
  | `broker_symbol` | `ES` | IBKR `symbol` for discovery |
  | `broker_exchange` | `CME` | IBKR exchange for discovery |
  | `contract_months` | `HMUZ` | listed cycle, for display/sanity |
  | `roll_offset_days` | `8` | roll date = last trade date − N calendar days |
  | `roll_warn_days` | `3` | warn this many days before the roll date |
  | `status` | `ACTIVE` | |

  Seeded rows: ES (×50), MES (×5), NQ (×20). Multiplier, tick and min tick come
  from IBKR contract details at sync, not from this table (IBKR is the source of
  truth; the table holds only what IBKR doesn't tell us: the roll rule).
- **Contracts are ordinary instruments.** Each listed expiry is an `instrument`
  row: `symbol` = IBKR `localSymbol` (`ESZ6`), `venue` = `XCME`,
  `instrument_class = 'FUTURE'`, `asset_class = 'INDEX'`, `contract_size` = IBKR
  multiplier, tick from contract details; plus `instrument_derivative` with
  `underlying_symbol = 'ES'` (the root code) and `expiry_date` = last trade date.
  `broker_instrument`: `broker_symbol` = `localSymbol`, `native_id` = conId,
  `broker_exchange` = `CME`.
- **Expiry instant.** CME equity index futures stop trading at 09:30 ET on the
  expiry day (special opening quotation), not at the venue's daily close. So for
  futures, `expires_at` must come from IBKR's last-trade date **and time**
  (`lastTradeDateOrContractMonth` / `lastTradeTime` with the contract's time zone),
  written at sync. The expiry job's calendar derivation must skip rows whose
  `expires_at` was set from the broker (builder picks the mechanism — e.g. only fill
  NULLs for FUTURE, or a source flag).
- **Roll date** is derived, not stored: `expiry_date − roll_offset_days` in the
  venue's local calendar. For ES: third-Friday expiry − 8 days = Thursday of the
  prior week, which matches CME's published roll date.

### 2. Front-month resolution

- **Front contract of a root** = the `ACTIVE` future with `underlying_symbol = root`
  whose roll date is after today (venue-local), earliest first. Before the roll
  date the expiring contract is front; on and after it, the next one is.
- One SQL function / Rust fn used by API and UI. Exposed as
  `GET /futures/roots` (each root with its front and next contract) and used by the
  instrument search: typing `ES` offers "ES → ESZ6 (Dec 2026)" first, then the
  specific contracts.
- **Orders always go to a concrete contract** (instrument id of ESZ6). The root is
  a picker convenience only; nothing downstream knows about roots.

### 3. IBKR connector: generic contracts

- `ContractQuery` gains `sec_type` (STK | FUT | OPT, extensible), optional
  `last_trade_date_or_month`, and `include_expired = false`.
- `TwsContract` gains `sec_type`, `local_symbol`, `multiplier`,
  `last_trade_date`, `last_trade_time`/time zone, and for OPT `strike` and `right`.
- `stock_contract()` becomes a generic builder from the stored conId + exchange +
  sec type. Routing by conId stays; for FUT the exchange is `CME`, never `SMART`.
- `to_broker_instrument()` maps by sec type: STK → EQUITY/SPOT (as today), FUT →
  root's asset class/FUTURE with multiplier and derivative row, OPT → EQUITY/OPTION
  with strike/right/expiry (routing-ready; no sync job for it yet).
- **Fill and order mapping by `localSymbol`/conId.** `TwsExecution` and order
  updates carry `local_symbol` and `con_id`; `fill_from_exec` sets `symbol` to the
  local symbol (for STK it equals `symbol`, so equities don't change). Test: two ES
  expiries filled in one session book to two different instruments.
- **Contract sync for roots.** A sync step (alongside the existing IBKR import in
  `oms setup sync-broker` / the universe job) queries `contract_details` per active
  root (`sec_type = FUT`, `symbol = broker_symbol`, `exchange = broker_exchange`),
  upserts every listed, unexpired contract, and runs daily so newly listed
  quarterlies appear. Unknown exchange → reported, not guessed (as today).
- **Quotes.** The existing IBKR delayed-quote path (`QuoteScope::NonCrypto`) must
  build FUT contracts through the generic builder; nothing else changes. Previous
  close for Day P&L uses the same path.

### 4. Order entry and risk

- Futures orders use the existing ticket: MKT/LMT, DAY/GTC, integer quantities
  (`size_increment` 1), price on the 0.25 tick.
- Risk: notional = `qty × price × contract_size` (already implemented). No new
  futures-specific limits in this step.
- Trading grants: venue grants apply to the IBKR broker connection as today; the
  planned instrument-class grant will be able to allow/deny `FUTURE` — no change
  here.

### 5. Roll warnings

- **Where.** The hourly sweep in `src/expiry.rs` gets a third step: for every open
  position (portfolio × instrument, net qty ≠ 0) in a `FUTURE` whose roll date is
  within `roll_warn_days`, raise a `ROLL_DUE` warning once.
- **New alert channel** (generic, the first user of it):
  - table `alert(id uuid pk, kind text, portfolio_id uuid, instrument_id bigint,
    dedupe_key text unique, payload jsonb, created_at, acknowledged_at)`; kind
    `ROLL_DUE`; dedupe key `ROLL_DUE:<portfolio>:<instrument>` so a restart or the
    next hourly tick never repeats it. Payload: root, contract, next contract, net
    qty, roll date, expires_at.
  - published through the Kafka outbox on a new `alerts` topic (configured like
    the `orders`/`fills` topics).
  - new `UiNotice::Alerts` so the cockpit refetches.
  - `GET /alerts` (unacknowledged, scoped by the caller's portfolio access) and
    `POST /alerts/{id}/ack`.
- **Cockpit.** The positions table shows a roll badge on any futures position whose
  roll date is near — derived live from the position and the roll date, not from the
  alert, so it is never stale. The alert drives a dismissible banner/toast ("ESZ6
  rolls in 3 days — 4 contracts long in Portfolio X · Roll").
- Perpetuals have no expiry and no root, so they never enter this step.

### 6. One-click roll

- **Action:** on a futures position (badge, banner or position row menu) "Roll to
  ESH7". Opens a small confirm dialog prefilled: legs (sell 4 ESZ6 / buy 4 ESH7 for
  a long), order type (LMT on the spread price, default = mid of the delayed spread
  quote, editable; MKT allowed), TIF DAY. Quantity defaults to the full position,
  editable down.
- **Order model: parent order with two child orders (decided).** Today every fill
  belongs to its order's single instrument and side (position replay in
  `src/positions.rs`, the Kafka `fills` topic). A roll keeps that invariant:
  - **Parent** = an OMS order on a spread instrument (`ESZ6-ESH7`). The spread is
    created on demand at roll time: an `instrument` row (class `FUTURE`, venue
    `XCME`) plus a `future_spread(instrument_id, near_id, far_id)` table naming the
    legs. It has no `broker_instrument` row: routing builds the BAG from the legs'
    conIds. The parent never books fills and never holds a position.
  - **Children** = two ordinary orders created by the OMS with the parent, one per
    leg (long roll: sell 4 ESZ6, buy 4 ESH7), linked by a new `parent_order_id`
    column/event field. Children are never sent to the broker on their own.
  - **Status** of the parent is derived from its children (both filled → filled;
    any fill → partially filled; cancel/reject applies to all three). Cancel is
    issued once, for the BAG, from the parent.
  - The dialog speaks in legs ("sell 4 ESZ6, buy 4 ESH7"); the CME spread side
    convention (buy spread = buy near / sell far) lives only in the adapter.
- **Routing:** the adapter sends **one** BAG order (`secType BAG`, two `ComboLeg`s
  by conId, exchange CME, `order_ref` = parent id) so it goes to the
  exchange-listed spread, not two legged orders.
- **Fills:** IBKR reports one execution per leg (and may report one for the BAG).
  Ingest maps each leg execution by conId/local symbol to the matching child and
  books it as a normal fill there; the BAG-level execution is ignored. Positions,
  replay and the Kafka fill shape are unchanged.
- **Recon:** on reconnect the BAG order is matched to the parent by `order_ref`; leg
  executions resolve to children by conId. Rolls done directly in TWS: BAG
  execution ignored, leg executions booked as ordinary observed fills on the leg
  instruments.
- **Risk:** each child goes through the existing risk engine as a normal order
  (per-leg quantity and notional limits). The parent itself is not risk-checked.
  No change to the risk crate.
- **Partial fills** are fine: legs fill together on a listed spread, so a partial
  roll leaves a smaller position in each contract, both visible.
- **Reuse later:** the parent/children shape is the intended base for option combos
  and other multi-leg orders.

### 6b. IBKR FIX connector

- Futures route through IBKR **TWS only**. The FIX IBKR connector (`connectors/ibkr`)
  refuses any non-`SPOT`/non-equity instrument with a clear error before sending.

### 7. Room for perpetuals (no build now)

- Add `PERPETUAL` to the `instrument_class` CHECK in this step's migration (cheap,
  avoids overloading `SWAP`, which stays for interest-rate swaps). No code uses it
  yet.
- Expiry, front-month and roll logic select on `FUTURE` with a non-null expiry, so
  perps are excluded by construction.
- Valuation: make `positions::value` dispatch on instrument class with one linear
  arm for everything today, so an inverse arm `qty × size × (1/avg − 1/mark)` can be
  added later without touching callers.

## Testing

- Unit: generic contract mapping per sec type; fill mapping by local symbol (two
  expiries, same root); roll date for a third-Friday expiry; front-month switch at
  the roll date boundary (venue-local); alert dedupe.
- `FakeTws` grows FUT and BAG support; adapter tests cover sending a FUT order by
  conId on CME and a BAG roll with two legs.
- Roll: parent + two children created atomically; leg executions land on the right
  child; parent status derived; cancel cancels all; BAG execution ignored; FIX
  connector refuses a FUTURE.
- DB tests (ignored suite, skip `real_docker`): sync upserts contracts with
  multiplier, derivative row and broker `expires_at`; expiry job leaves broker-set
  `expires_at` alone; ROLL_DUE fires once per position.
- Live: IBKR paper account, MES (cheapest): sync, buy 1 front, see position/P&L
  with ×5, see roll badge (temporarily lower `roll_warn_days` or pick a contract
  near its roll), one-click roll, verify both leg positions. Record the outcome in
  the release notes; restart/recon behaviour is checked in the same session.

## Calls made by Claude (user confirmed)

- FIX IBKR connector refuses futures; TWS only.
- TWS-originated rolls: BAG execution ignored, legs booked as observed fills.
- Children are risk-checked as normal orders; no risk-crate change.
- `PERPETUAL` class added now; `SWAP` kept for rates.
- Alerts as a new generic channel (table + Kafka `alerts` topic + `UiNotice`).
- Roll badge derived live; the alert only drives the banner.

## Left to the builder

- Exact column names, migration numbering, module placement.
- How the expiry job tells broker-set `expires_at` from calendar-derived.
- Where root contract sync hooks in (setup sync vs. scheduled job), as long as it
  runs at least daily.
- Cockpit placement and wording of badge, banner and roll dialog within shadcn/ui.
- Whether `/futures/roots` is its own endpoint or folded into instrument search.
