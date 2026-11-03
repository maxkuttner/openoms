# Coinbase spot as a broker connection — design

Date: 2026-11-03. Status: draft for approval.

## Intent

openOMS should offer basic trading on venues a broad audience is licensed to use. Coinbase
is regulated in the US, the EU (MiCA) and the UK, and the user will open an account in
Switzerland. "Basic" means what Binance and Bybit spot have: pick the venue, enter
credentials, load instruments, place and cancel orders, see fills come back, have open
orders reconciled on reconnect, and see positions priced.

Coinbase has **no paper trading**: its Advanced Trade "sandbox" returns fixed, canned
responses. So Coinbase is **LIVE only**, and every order is real money. (A per-connection
max order value was built and then withdrawn by the user: the account balance already
bounds what an order can buy.)

Coinbase is done when **a small real order (a few CHF/EUR/USD of BTC) placed from the
trader UI shows as filled**, from the user's Swiss account.

## Goals

1. Coinbase appears in the venue dropdown ("Coinbase — Crypto spot pairs") with **only a
   LIVE environment**. The connection code is `coinbase-live`.
2. The credentials drawer looks like every other venue's: **API key** (the CDP key name,
   `organizations/…/apiKeys/…`) and **Private key (PEM)**, both secret, no section.
   **Test** verifies them against Coinbase (`GET /key_permissions`) and shows a failure if
   the key lacks the `trade` permission.
3. Loading the instrument universe brings in Coinbase's spot products (`GET /products`),
   venue `COINBASE`, symbol as Coinbase writes it (`BTC-USD`), quote currency from the
   product, increments and min/max sizes from the product. Only `online`, not
   `trading_disabled` products are tradeable. Same entry points as the other venues.
4. Orders route over Advanced Trade REST (`https://api.coinbase.com/api/v3/brokerage`):
   market and limit, with the time-in-force values the OMS supports that Coinbase spot
   accepts. Cancel through `POST /orders/batch_cancel` with one id.
5. Order updates arrive on the authenticated **user** channel
   (`wss://advanced-trade-ws-user.coinbase.com`) and update the order: fills, partial
   fills, cancels, rejects, expiries. The stream shows in "Live execution streams".
6. Open orders are reconciled on (re)connect and on a heartbeat, as for the other venues.
7. **Coinbase positions are priced**: a public Coinbase market-data feed (ticker channel on
   `wss://advanced-trade-ws.coinbase.com`, no auth) marks `COINBASE` instruments; without
   it they sit unmarked.
8. Day P&L gets a previous close for Coinbase instruments, as Binance and Bybit do.
9. Saving credentials applies them with no restart (REST + WebSocket, like Bybit).
10. A Coinbase docs page with the same Credentials table every venue page has, a clear
    "orders are real money" warning, and the LIVE-only note.

## Non-goals

- Paper trading in any form, including wiring the canned sandbox as PAPER.
- Derivatives: US futures and the international (Deribit-based) gateway.
- FIX, order amend/edit, stop and bracket orders, post-only flags.
- Coinbase Prime / Exchange / International; only Advanced Trade (the retail-and-pro API).
- Converting between currencies; each pair trades in its own quote currency.

## Anti-goals

- Fills counted twice. The execution path does not dedupe by execution id (it only skips
  events for orders already terminal), and Coinbase's user channel reports **cumulative**
  order state, not one message per fill. A fill must be booked as the difference between
  Coinbase's cumulative filled quantity and what the OMS has already booked for the order,
  so a repeated or reordered message books nothing new.
- A Coinbase-specific look in the drawer, or help text in the UI.
- The user's Coinbase key handled by anyone but the user.

## Constraints

- Follow the Bybit integration's shape (`src/adapters/bybit.rs`, `src/bybit_stream.rs`,
  registration in `reload.rs` with stream restart on reload, `Broker` enum, venue entry in
  `src/venues.rs`, docs table checked by `docs-site/test/venue-credentials.test.mjs`).
- Authentication: a fresh JWT per REST request and per WebSocket subscribe message (they
  expire after 2 minutes), signed with the CDP key's private key. The builder confirms the
  exact algorithm and claims from Coinbase's key-authentication docs and supports the key
  type Coinbase issues for Advanced Trade.
- The user channel requires a subscribe within 5 seconds of connecting.
- The venue list currently assumes every venue has PAPER and LIVE; it must allow a
  LIVE-only venue, and the cockpit's environment picker must then only offer LIVE.
- `Broker::Coinbase.environment()` is always `LIVE`; boot creates `coinbase-live`.
- Network-free unit tests from recorded or documented JSON; DB tests stay `#[ignore]`;
  never create databases on the shared dev Postgres.
- Docs ship in the same release as the feature.

## Decided by the user

- Coinbase next; spot; Advanced Trade.
- LIVE only — no PAPER, not even the canned sandbox.
- Done means a small real order from the user's Swiss account, filled.
- The user opens the account and creates the API key, and enters it in the drawer.
- No max order value: built (`2aeec5d`), then reverted at the user's request.

## Decided by me (overrule if wrong)

- Coinbase products are their own instruments, venue `COINBASE`, symbols like `BTC-USD`
  (as Bybit pairs are separate from Binance's).
- All spot products are loaded, every quote currency (USD, USDC, EUR, GBP, CHF where
  offered); the Swiss account decides which it can actually trade.
- The OMS order UUID is sent as `client_order_id`.
- A fill's price is derived from the change in Coinbase's average price:
  `(avg_new × cum_new − avg_old × cum_old) / (cum_new − cum_old)`, falling back to the new
  average when the old state isn't known.
- The Coinbase price feed ranks first for `COINBASE` instruments and is not used for other
  venues.
- **Test refuses a key that can move money out.** `GET /key_permissions` reports
  `can_view`, `can_trade`, `can_transfer`: Test fails unless `can_trade` is set and
  `can_transfer` is not, and the docs tell the user to create a key with View and Trade
  only, scoped to one portfolio. An OMS never needs to withdraw.
- **Spot only from `/products`:** it also returns futures (and, increasingly, other
  product types); the loader keeps `product_type = SPOT` only.
- **Everything but the live order is built and checked without the user's key**:
  recorded-JSON tests, the public price feed, and the universe load (public endpoint).
  Credentials, the private stream and the real order are the user's final step.

## Left to the builder

- The JWT algorithm and claims, and which CDP key type to document in the drawer table.
- Market-order sizing: Coinbase market buys may need a quote size; the OMS quantity is in
  the base coin, so use `base_size` where Coinbase accepts it, else derive the quote size
  from the mark and say so in the docs.
- Mapping Coinbase order statuses onto OMS events, and where `cumulative_quantity`,
  `average_filled_price` and `leaves_quantity` live in the user-channel schema.
- Pagination of `/products` and `/orders/historical/batch`.
- Sharing WebSocket plumbing between the user stream and the price feed.
- Where previous close comes from (product stats or candles).

## Done when

1. All tests pass (`cargo test`, cockpit `npm test`, docs `npm run check`), including the
   venue-list and docs-table tests listing Coinbase.
2. On the dev stack, with the user's key saved in `coinbase-live`: **Test** passes, the
   Coinbase stream shows live, the universe loads, and BTC (or the account's pair) has a
   price.
3. With an account linked to `coinbase-live`, a market buy worth a few CHF/EUR/USD from
   the trader UI shows **Filled** with Coinbase's quantity and price.
4. A limit order far from the market can be placed and cancelled from the UI.
