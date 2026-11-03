# Bybit spot as a broker connection — design

Date: 2026-11-03. Status: draft for approval.

## Intent

openOMS should offer the basic functionality on a range of venues, not just Alpaca and
Binance. Bybit is the next one. "Basic functionality" means what Binance spot has
today: pick the venue, enter credentials, load its instruments, place and cancel
orders, see fills come back, and have open orders reconciled on reconnect.

The user starts on **testnet** (PAPER). Bybit is done when **a real order placed from
the trader UI on Bybit testnet shows as filled**: an end-to-end check that Binance
itself has not had yet.

## Goals

1. Bybit appears in the cockpit's venue dropdown ("Bybit — Crypto spot pairs") with
   PAPER = testnet and LIVE = live account.
2. The credentials drawer for Bybit looks like every other venue's: **API key** and
   **API secret** (HMAC), with no settings section. **Test** verifies them against Bybit.
3. Loading the instrument universe brings in Bybit's spot pairs, the same way Binance's
   load does (Broker connections → Instrument universe → Load; and
   `oms setup sync-broker --broker bybit`).
4. Orders route to Bybit over its v5 REST API: submit (market and limit, with the
   time-in-force values the OMS already supports that Bybit spot accepts), and cancel.
5. Fills, partial fills, cancels and rejects arrive from Bybit's private WebSocket
   (v5 `order.spot` / execution topics) and update the order as Binance's stream does,
   with the stream visible in the cockpit's "Live execution streams" strip.
6. On reconnect, open orders are reconciled from Bybit's open-orders REST call, as
   `recon_orders.rs` does for Binance.
7. Day P&L gets its previous close from Bybit's 24h ticker, as Binance does.
8. A saved credential applies with **Reload**, without a restart (REST only, so nothing
   forces a restart).
9. The Bybit docs page (`docs-site/.../integrations/bybit.md`, today a price-feed page)
   covers trading too, with the same **Credentials** table every venue page has.

## Non-goals

- Perpetuals, futures, options, margin and leverage. The OMS has no perpetuals
  support anywhere; that's a separate project, deliberately deferred.
- FIX. Bybit's FIX access is for institutional clients; REST and WebSocket only.
- Order amend/replace. Binance doesn't support it here either.
- Bybit's "demo trading" environment on mainnet: PAPER means the testnet
  (`api-testnet.bybit.com`).
- Any change to the existing Bybit price feed (`src/bybit_feed.rs`) beyond what sharing
  code with it naturally gives.

## Anti-goals

- A Bybit-specific look in the drawer, or help text in the UI. Explanations go on the
  docs page; the venue entry carries fields, environments and links only.
- A second hand-maintained list of venues or credential fields. Bybit is added to
  `src/venues.rs`, and the existing tests must then force the rest into line
  (`known_broker_codes`, the `BrokerCredentials` variant, the docs Credentials table).
- Touching or creating databases on the shared dev Postgres (100.80.233.3) beyond what
  the running dev OMS normally writes; DB tests stay `#[ignore]`.

## Constraints

- Follow the Binance spot integration's shape: an adapter implementing
  `BrokerAdapter` (`src/adapters/`), a private stream task reporting into
  `StreamHealthRegistry`, registration in `reload.rs`, preflight and boot loads, the
  `Broker` enum in `setup/brokers.rs` for catalog sync, and the credential parse/test
  in `credentials_api.rs`. Binance touches about 30 files; Bybit touches the same
  places.
- Credentials are a new `BrokerCredentials::Bybit { api_key, api_secret }` variant,
  stored encrypted like the others. The drawer field labels are "API key" and
  "API secret"; both secret.
- Bybit v5 REST signing: headers `X-BAPI-API-KEY`, `X-BAPI-TIMESTAMP`, `X-BAPI-SIGN`,
  `X-BAPI-RECV-WINDOW`; HMAC-SHA256, lowercase hex, over
  `timestamp + api_key + recv_window + (query string | JSON body)`.
- Hosts: PAPER `https://api-testnet.bybit.com` and the testnet private WebSocket;
  LIVE `https://api.bybit.com` and the mainnet private WebSocket. The environment comes
  from the broker connection, as for Binance.
- Unit tests for parsing and signing run without network (recorded JSON samples, a
  known-answer signature test). Anything that needs Bybit is either `#[ignore]` or the
  final manual testnet check.
- Releases follow the usual flow, and the docs page ships in the same release as the
  feature (docs publish on push to main; users install the latest release).

## Decided by the user

- Bybit spot first, perpetuals left out while they go beyond Binance's scope.
- Testnet first.
- Done means a real testnet order from the trader UI, filled.
- The user creates the Bybit testnet account and API key and enters the key in the
  drawer themselves. The builder never handles the key.
- One look in the drawer; descriptions live in the docs, per adapter.

## Decided by me (overrule if wrong)

- REST order entry only; no transport switch like Binance's `BINANCE_*_TRANSPORT`.
- The connection code convention stays `bybit-paper` / `bybit-live`.
- `oms setup sync-broker --broker bybit` gets the same flags Binance has
  (`--no-enrich` and so on).
- The credentials **Test** button calls Bybit's API-key-info endpoint. Unlike wallet
  balance it doesn't depend on the account type (unified or classic), and it shows the
  key's permissions.
- **Bybit pairs are their own instruments**, with venue `BYBIT`, not the existing
  `BINANCE` rows. `db/scripts/seed_crypto_venues.sql` already seeds `BYBIT` for this, and
  `mark_router.rs` treats the two exchanges as separate markets. `broker_serves_venue`
  gets a `BYBIT` arm. Visible effect: BTCUSDT appears twice in the instrument picker,
  once per venue.
- **Reload starts and restarts the private stream** like Alpaca's fill stream does,
  not "restart required" like Binance's REST path (`reload.rs:69-75`). This is new for a
  crypto venue and is what goal 8 needs.
- **Market buys send `marketUnit=baseCoin`**: by default a Bybit spot market buy's
  quantity is in the quote coin (USDT), while the OMS quantity is in the base coin.
- The OMS order UUID (36 characters with hyphens) goes out as `orderLinkId`. That is
  exactly Bybit's limit, so it fits unchanged.
- The universe-load prompt's per-venue copy in `CredentialsPanel.tsx` (`UNIVERSE_COPY`:
  name and "About 2,000 spot pairs") moves into `src/venues.rs`, so no second venue list
  remains in the cockpit.
- `oms setup sync-broker --broker bybit` reads `BYBIT_ENV` (default `PAPER`) the way the
  other brokers read `{BROKER}_ENV`, giving `bybit-paper`.

## Left to the builder

- How to map Bybit order statuses and execution messages onto the OMS's order events,
  including partial fills and how fees are carried (follow what Binance does with
  its fee fields).
- How to correlate WebSocket updates to OMS orders (Bybit's `orderLinkId` is the
  natural carrier for the OMS's client order id, but check its length and character
  limits).
- Quantity and price precision: how Bybit's lot-size and tick-size filters map onto the
  instrument fields Binance's sync fills in.
- Whether to share HTTP/WebSocket plumbing with `bybit_feed.rs`.
- Reconnect, backoff and ping cadence for the private stream (mirror `binance_stream.rs`).
- Exact order of the work, as long as each step ends tested.

## Done when

1. All tests pass (`cargo test`, cockpit `npm test`, docs `npm run check`), including
   the venue-list and docs-table tests, which must list Bybit.
2. On the dev stack, with the user's testnet key saved in the `bybit-paper` drawer and
   an account linked to `bybit-paper` (orders only route through an account linked to
   the connection, `handlers.rs:379-492`):
   **Test** passes, **Reload** registers the connection, the Bybit stream shows as live,
   loading the universe brings in Bybit spot pairs, and a small market buy (e.g.
   BTCUSDT) placed from the trader UI shows as **Filled** in the blotter with the fill
   price and quantity Bybit reports.
3. A limit order far from the market can be placed and then cancelled from the UI, and
   the cancel arrives over the stream.
4. Restarting the OMS with an order open reconciles it from Bybit's open orders.
