# IBKR TWS API connector — design

Date: 2026-11-10 · Status: draft for approval

## Intent

Trade stocks at Interactive Brokers from the OMS the way we trade them at Alpaca
today — US and non-US — without paying for FIX CTCI. IBKR's TWS API (spoken by
IB Gateway) is free on any account, including paper, so it is the feasible route
now. The existing FIX 4.2 connector (`IBKR`) stays as it is: experimental,
untouched.

Success: on an IBKR **paper** account, from the cockpit, import a CSV of US and
European stocks, place/cancel market and limit orders, see fills, Day P&L on
delayed prices, and survive IB Gateway's nightly restart without losing orders.

## Goals

1. New connector `IBKR_TWS` ("Interactive Brokers (TWS API)"), paper and live.
2. Parity with the Alpaca connector for stocks/ETFs:
   - submit market and limit orders (DAY/GTC), cancel, order status, open-orders
     snapshot (so existing order recon works after a reconnect);
   - execution stream: order status + executions + commissions → engine;
   - "Verify connection" checks the account;
   - previous close for Day P&L (`daily_stats`) and quotes, from **delayed**
     market data (free; no subscription needed).
3. Non-US stocks: any IBKR stock exchange, priced in its own currency.
4. The OMS **runs IB Gateway itself**, in Docker, with the IBKR login it stores.
5. Instruments pinned by IBKR **conId**, added on demand (CSV import, search).

## Non-goals

- Options, futures, FX, bonds (later; conId model allows them).
- A full IBKR catalog mirror (no API lists it; scraping is fragile).
- Real-time market data subscriptions.
- Running the gateway ourselves outside Docker, or TWS desktop.
- Changing the FIX connector.

## Anti-goals (failure even if it "works")

- An order sent to the wrong listing because a symbol was ambiguous
  (e.g. `SAP` US ADR vs `SAP` Xetra). Identity is always the conId.
- Losing track of an order across a gateway restart (duplicate or orphan).
- IBKR password in logs, process lists, the repo, or plain-text config.
- The OMS silently trading on a gateway that is half-logged-in.

## Decisions (made with the user)

- TWS API via IB Gateway, not FIX.
- OMS stores the IBKR username/password and runs the gateway container.
- Delayed prices are fine for now.
- Non-US stocks are in scope.
- Instruments: no full catalog; each pinned by conId; added via CSV import
  (cockpit file picker + CLI) and cockpit search.

## Design

### 1. Gateway management

- Image: a public IB Gateway + IBC image (e.g. `ghcr.io/gnzsnz/ib-gateway`),
  pinned by digest. IBC fills the login screen and handles the daily restart.
- One container per `IBKR_TWS` connection, named `oms-ibgw-<connection>`.
  Username, password and trading mode (paper/live) are passed as environment
  variables to `docker run` via an env file with mode 0600 in the OMS state dir
  that is deleted right after start — never on the command line.
- API port bound to `127.0.0.1` only (random free host port, recorded in memory).
  Read-only API mode off; "allow connections from localhost only" on.
- Supervisor task in the connector:
  - start container → wait for API port → connect `ibapi` client (client id
    fixed per connection) → `Connected`.
  - health states shown in the cockpit stream-health panel:
    `starting`, `connected`, `awaiting 2FA` (live; IBC log line), `login failed`
    (bad credentials — stop retrying, surface it), `disconnected`.
  - on disconnect (nightly reset ~23:45 local, crash): reconnect with backoff;
    if the container died, restart it. Each reconnect triggers the existing
    order recon (open-orders snapshot).
  - stopping or deleting the connection stops and removes the container.
- Docker missing → connection fails with a clear message (no fallback).
- Live 2FA: IBKR requires phone approval at login (at least weekly). We cannot
  automate it; the cockpit shows `awaiting 2FA` and trading is refused until
  connected.

### 2. Credentials

`credential_kind: "IbkrTws"`, fields `username` (Clear), `password` (Hidden),
stored encrypted like other brokers. Environment PAPER/LIVE picks IBC's
trading mode. `session_bound: true` (a credential change restarts the gateway).

### 3. Orders

- Library: Rust `ibapi` crate (pin version), our call.
- Order IDs: TWS requires a monotonically increasing integer per client id.
  Take `nextValidId` on connect; keep the OMS client order id in `orderRef` so
  fills and recon map back even after a restart.
- Contract on the wire: `conId` + `exchange = SMART` (or a pinned exchange
  from the instrument's `broker_exchange`), plus `primaryExch` and currency.
- Types: MKT, LMT; TIF DAY, GTC. Quantity whole shares; non-US lot sizes from
  contract details enforced before submit.
- Cancel by IBKR order id. Order status and open orders via `reqOpenOrders` /
  `reqAllOpenOrders`; completed via `reqCompletedOrders`.
- Errors: map TWS error codes to `BrokerError` (reject vs transient vs
  no-such-order), table in the adapter with tests.

### 4. Execution stream

`exec_stream: true`. Subscribe to order status, `execDetails` and
`commissionReport`; turn executions into the engine's fill events (exec id is
the idempotency key; commission enriches the fill when it arrives, as the
Binance fee path already does). On reconnect, `reqExecutions` for the day
fills any gap.

### 5. Market data

`reqMarketDataType(3)` (delayed). `daily_stats` = previous close from delayed
snapshot / one-day historical bar. Quotes scope: stocks the connection holds.
Missing data skips the symbol (never zero), as Alpaca does.

### 6. Instruments

- Identity: `native_id = conId`. `venue` = IBKR connection's code; listing
  exchange as MIC (`provider_exchange`), currency from IBKR, `price_increment`
  = minTick, `lot_size`/`size_increment` from contract details, name, ISIN
  into identifiers when IBKR returns it. Same symbol on two exchanges = two
  instruments.
- IBKR exchange code ↔ MIC table (`NASDAQ→XNAS`, `IBIS→XETR`, `LSE→XLON`, …)
  with tests; unknown exchange → row reported, not guessed.
- **CSV import** (shared by cockpit and CLI):
  `symbol,exchange,currency` (exchange/currency optional; exchange as IBKR code
  or MIC) or a `conid` column. Each row resolved with `reqContractDetails`
  (STK). Outcomes: found / ambiguous (candidates listed) / not found.
  - Cockpit: Instruments page → "Import from IBKR" → file picker → preview
    table → pick ambiguous rows → confirm saves resolved rows. Idempotent.
  - CLI: `oms setup sync-broker IBKR_TWS --symbols file.csv` saves unique
    matches, prints ambiguous and not-found rows, nonzero exit if any.
- **Search**: cockpit search box using `reqMatchingSymbols` → pick → save.
- Nightly refresh of stored IBKR instruments; a conId IBKR no longer knows is
  marked not tradeable, never deleted.
- Import needs the gateway connected (it is the lookup source).

### 7. Cockpit

Venue card "Interactive Brokers (TWS API)" with username/password fields and
paper/live; gateway status in the existing stream-health UI; Instruments
import dialog and search as above.

### 8. Docs

`docs-site/.../integrations/interactive-brokers.md`: TWS API section (Docker
required, paper first, 2FA for live, delayed data, CSV format), FIX section
kept and marked experimental.

## Decisions from the design review (user)

- **Currencies — per-currency, no FX yet.** Amounts stay in the instrument's
  own currency. Day P&L and position views total per currency (no cross-
  currency sum). Notional limits are per currency: a limit set in USD applies
  only to USD instruments; a non-USD instrument is checked only if a limit in
  its currency exists, else its notional check is off (quantity limits still
  apply). The risk crate's USD-only inherited-notional rule
  (`crates/risk/src/evaluate.rs`, `USD_LIKE`) is extended accordingly.
  Proper FX conversion is a later, separate project.
- **Price band skipped on IBKR_TWS.** Delayed prices are too stale for a
  band. The skip is explicit per connector (a manifest flag), not by missing
  reference — today a band without a reference refuses the order, and that
  stays true for every other venue.

## Builder's calls (from the design review)

- **Gateway owner:** the OMS process (where connectors live), not the colo
  runtime. On OMS restart it adopts a running `oms-ibgw-<connection>` container
  by name instead of recreating it. One OMS per connection is assumed; the
  fixed client id makes a second one fail to connect, which is surfaced.
- **IBKR order id mapping:** the OMS already stores `external_order_id` per
  order (`order_state`); the IBKR order id goes there — no migration. A cancel
  while the gateway is reconnecting is rejected as transient, not queued.
- **Previous close outside the US:** delayed snapshot first, else a one-day
  historical bar; if neither is available, Day P&L skips that stock.
- **Import and refresh:** both run inside the OMS. `oms setup sync-broker
  IBKR_TWS --symbols` calls the running OMS's admin API; it never starts Docker
  itself. The nightly refresh is a task in the OMS.
- **Fills after a long outage:** on reconnect, `reqExecutions` (same day) plus
  `reqCompletedOrders` reconcile final states; per-fill detail older than the
  day may be lost and is a documented known gap.

## Testing

- Unit: exchange↔MIC map, CSV parsing, resolution outcomes, error-code map,
  orderRef round trip, order-id allocation across reconnect.
- Adapter tests against a recorded/fake TWS message stream where `ibapi`
  allows; otherwise a trait seam around the client.
- Live check on an IBKR paper account (owed before release): import a mixed
  US/EU CSV, place/cancel limit orders, market fill, survive a forced
  container restart, recon finds the open order.

## Left to the builder

- Exact image and IBC settings; port selection; backoff values.
- Whether import preview is one endpoint (resolve) + one (save) or a job.
- How `ibapi` is wrapped for testability.
- Full TWS error-code table contents.
