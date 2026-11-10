---
title: Changelog
description: What changed in each OpenOMS release.
---

Generated from the commit history by `cz bump` on every release.

## v0.9.0 (2026-11-10)

### Feat

- **cockpit**: import IBKR instruments from the broker connection
- **cockpit**: import IBKR instruments from a CSV or search
- **ibkr**: refresh imported instruments nightly
- **ibkr**: import instruments from a CSV, resolved by conId
- **ibkr**: orders and fills over the TWS API
- **ibkr**: a TWS client seam over ibapi
- **ibkr**: run and adopt the IB Gateway container
- **ibkr**: TWS API connector skeleton and exchange map
- **positions**: totals per currency
- **risk**: per-currency notional and a per-venue price-band exemption

### Fix

- **ibkr**: apply live paper test findings
- **ibkr**: address the final branch review
- **ibkr**: cap import requests, stop listing IBKR_TWS as a loadable universe
- **ibkr**: replay-safe catch-up, retry docker outages, unknown fills reconcile
- **ibkr**: TWS client time zones, order refs, prev close and quantities
- **ibkr**: harden gateway env file and adoption checks
- **ibkr**: correct Toronto/Tokyo exchange codes and docs url
- **risk**: do not hold or refuse a band-exempt order for a missing quote
- **colo**: keep the testnet demo's state outside the checkout

## v0.8.0 (2026-11-10)

## v0.7.1 (2026-11-07)

### Feat

- **colo**: a one-command Binance testnet demo, and its docs
- **cockpit**: show a co-located runtime's status and its operator actions
- **colo**: a runtime takes its epoch from the OMS's bootstrap
- **colo**: record a verified runtime stop without retiring
- **colo**: settle a lost submit as never accepted once its window passes
- **colo**: reconcile venue inventory and stop orders on a shortfall
- **colo**: flag a deployment kill the runtime has not cleared in time
- **colo**: retry booking inbox events in the background
- **colo**: configure the fee allowance per deployment
- **colo**: record runtime acknowledgements and recovery status
- **runtime**: acknowledge kill, enable and policy; report recovery
- **runtime**: checkpoint and compact the journal automatically
- **runtime**: checkpoint journal record; compaction requires one
- **runtime**: checkpoint and restore the book
- **runtime**: inject v2 link and wire control/reporting
- **runtime**: add embedded runner and replay reporter
- **colo**: add durable event inbox and asset ledger
- **runtime**: add recovery barrier and policy authority
- **colo**: register durable deployments and bootstrap
- **link**: add versioned v2 control and event protocol
- **runtime**: add durable local journal
- **runtime**: add deterministic reservation and execution reducer
- **gateway**: expose Binance runtime recovery contract
- **risk**: share pure pre-trade evaluator

### Fix

- **colo-runtime**: match a fill made before a crash lost the submit answer
- **colo-runtime**: keep the event sequence across restarts
- **colo-runtime**: stay ready across an OMS reconnect
- **colo-runtime**: resume trading after an enable
- **colo-runtime**: let the OMS's kill record decide after a restart
- **colo-runtime**: acknowledge the bootstrap's policy revision
- **binance**: a cancel of an order no longer open is no-such-order
- **colo**: count a v2 runtime's control connection as its link
- **binance**: send prices and quantities without float noise
- **runtime**: resync after the venue feed returns
- **runtime**: never reuse order ids from an earlier journal
- **runtime**: refuse orders on missing prices or an unhealthy feed
- **link**: validate runtime control through the v2 session
- **runtime**: recover from the latest checkpoint and end with one
- **runtime**: release orders dropped before they are sent
- **runtime**: record why admission stopped; settle unclear orders
- **runtime**: retry cancels and send the ones asked for in flight
- **runtime**: kill cancels every working order
- **runtime**: deliver authority and restore ready transitions
- **colo**: harden freshness, cursors, fees and link ownership
- **runtime**: close review safety gaps

### Refactor

- **example**: run reference strategy on embedded runtime
- **colo**: move example to shared risk evaluator
- **risk**: route OMS admission through shared evaluator

## v0.7.0 (2026-11-06)

### Feat

- **colo**: reference market maker, FIX-free gateway build, co-located strategy docs
- **colo**: control link, kill that keeps fighting, unmonitored and order-rate alarms
- **colo**: follow co-located accounts and book their fills as external orders
- **strategies**: co-located mode, follow credential and control settings in the registry
- **colo-link**: control-link protocol and client with a dead man's switch
- **colo-risk**: in-memory pre-trade checks for co-located strategies
- **recon**: orders stuck in submitted are settled from what the venue says
- **orders**: orders refused before they leave are stored as rejected, with the reason
- **cockpit**: strategies page with kill, enable, flatten; FIX and REST docs for strategy developers
- **strategies**: kill, enable and flatten; cancel on disconnect and at boot; stopped strategies lose REST
- **strategies**: execution reports to strategies from the order lane, replayed on logon
- **strategies**: FIX acceptor for strategies — logon, new order, cancel, status, security list
- **strategies**: strategy orders take the one order path with limits, price band and a wait for the first quote
- **strategies**: strategy registry with FIX logon identity and connections
- **md**: OPRA runs under the market data gateway; the mark router's only feeder is the gateway
- **md**: the market data gateway runs every feed from the core's subscriptions
- **engine**: one order-engine task books every execution event; recon goes through it
- **engine**: inbound execution contract and pure booking plan
- **ui-updates**: an open live-updates socket keeps its session alive and ends with it
- **cockpit**: blotter, positions and watchlist update live instead of polling
- **cockpit**: live-updates socket invalidates stale trade queries
- **bus**: allocation transfers publish position changes
- **ui-updates**: /updates websocket forwards conflated bus notices
- **ui-updates**: conflate bus events into UI notices
- **kafka**: publish positions from every position write, so transfers reach oms.positions
- **kafka**: outbox bridge publishes from the event store, catching up after outages
- **kafka**: ordered at-least-once delivery with outage logging
- **kafka**: map outbox events to orders, fills and positions records
- **kafka**: durable outbox queued by an order_event trigger
- **bus**: mark router publishes accepted marks on the price lane
- **bus**: publish committed order events and position changes on the order lane
- **event-store**: return global positions from append
- **bus**: in-process event bus with order and price lanes
- **okx**: credentials, venue entry, registration, reload and docs
- **okx**: private orders stream and public tickers feed
- **okx**: v5 REST adapter, spot catalog, order entry and recon reads
- **coinbase**: register, reload and stream Coinbase connections
- **coinbase**: public ticker feed marks Coinbase instruments
- **coinbase**: user stream turns cumulative order state into fills
- **coinbase**: credentials, live-only venue, catalog sync and docs
- **coinbase**: spot order entry, recon reads and daily stats
- **coinbase**: Advanced Trade adapter core and spot catalog

### Fix

- **colo**: fixes from the testnet end-to-end run
- **colo-link**: serve the control link with rustls
- **colo**: phase 8 review fixes — gap recon, kill sweep, dead man from the OMS
- **gateway**: ambiguous venue answers are unknown, not rejects; stuck orders keep their venue id
- **orders**: a broker's reject is booked through the order engine; a transport error waits for recon
- **fix-acceptor**: a restart frees the old sessions before the new ones register; session events no longer log passwords
- **strategies**: an acceptor with no strategy yet waits for a restart instead of failing
- **strategies**: review fixes — queued FIX reports, in-flight orders on kill, finite prices, per-field limits, ordered requests
- **md**: a carried-forward connection keeps its previous-close source on reload
- **connectors**: show flag defaults in redaction, refuse another kind's credential, tighten the boundary guard
- **engine**: recon books only through the engine; per-connection coalescing recon; contain booking panics
- **ui-updates**: never cancel in-flight refetches; heartbeat and watchdog catch dead sockets
- **kafka**: keep warning while the outbox is held back, naming the stuck event
- **bus**: coalesce position-change bursts; document order-lane ordering limits
- **okx**: book fills from cumulative order state; close orders OKX no longer knows
- **coinbase**: price missed fills from the OMS's booked state; refuse non-LIVE Coinbase connections

### Refactor

- **orders**: submit and cancel move into one order path the REST handlers call
- **md**: previous closes come from a market-data registry, not the order adapter
- **gateway**: every connector lives in crates/gateway as one directory; the OMS keeps the database side
- **connectors**: FIX start-up and feed wiring move into each connector; the FIX engine and feed driver name none
- **gateway**: new gateway crate holds the execution contract, adapter trait and stream supervision
- **connectors**: credential forms, universes, daily stats and public feeds come from connectors
- **reload**: register connections and restart their streams through connectors
- **connectors**: manifests own each venue's copy, fields and environments
- **credentials**: generic stored credentials parsed by each connector
- **fix**: FIX sessions emit execution events to the order engine
- **streams**: emit execution events to the order engine instead of booking
- **kafka**: drop the direct publish path; connectors no longer carry a Kafka client
- **feeds**: subscribe to the order lane instead of per-feed doorbells

## v0.6.5 (2026-11-03)

### Feat

- **risk**: cap the value of one order per broker connection
- **bybit**: register, reload and stream Bybit connections
- **bybit**: private order and execution stream
- **bybit**: credentials, venue entry, catalog sync and docs
- **bybit**: spot order entry, recon reads and daily stats
- **bybit**: v5 REST adapter core and spot catalog
- **cockpit**: pick a venue from a list and get guided to its credentials

### Fix

- **db**: drop max_order_value again for databases that applied 0029
- **bybit**: book fills a cancel overtook before closing the order

## v0.6.4 (2026-11-03)

### Feat

- **cli**: oms status shows what runs on this machine and where
- **cockpit**: deactivate and reactivate principals, portfolios, accounts and broker connections

## v0.6.3 (2026-11-02)

### Feat

- **docs**: theme switch for phones on the landing page

### Fix

- **docs**: keep the landing page black in the middle on phones
- **deps**: rustls 0.23.46 and form-data 4.0.6 for Dependabot alerts

## v0.6.2 (2026-11-01)

### Feat

- **install**: ask before adding ~/.local/bin to PATH in a shell startup file

## v0.6.1 (2026-10-30)

### Feat

- one colour palette, black dark mode, broker carousel on the landing page
- **install**: add ~/.local/bin to PATH in the shell startup file
- **cockpit**: redirect the site root to /cockpit/

### Fix

- **cockpit**: replace the remaining blue with the brand orange

## v0.6.0 (2026-10-30)

### Feat

- Homebrew and Docker install channels; Bun-style Installation page (#26)

### Fix

- **release**: download only the binaries, not docker build records

## v0.5.0 (2026-10-29)

### Feat

- **init**: guided oms init and one Getting started page (#25)

### Fix

- **jobs**: retry a job insert whose blocker vanished; serialize job DB tests

## v0.4.0 (2026-10-29)

### Feat

- **cockpit**: instrument universe status, load actions, post-save prompt
- **cockpit**: jobs indicator and Task Center
- **api**: job list/detail; backfill and expiry sweep run as jobs
- **api**: broker connection universe status and load endpoints
- **jobs**: universe loads run as jobs; boot loads per connection
- **jobs**: persisted background job records
- **docs**: light-scheme variant of the landing grain background
- **docs**: high-resolution grain background on the landing page
- **docs**: gradient background on the landing page, dark mode only
- **docs**: cURL, Python and Rust samples in the API reference

### Fix

- **jobs**: no stuck jobs, one universe writer per connection, no boot re-runs
- **docs**: list test files explicitly for node --test

### Refactor

- **setup**: universe load callable with progress, CLI wraps it

## v0.3.0 (2026-10-29)

### Feat

- **docs**: publish the Starlight site as the whole Pages site; remove site/
- **docs**: port the architecture page and OPRA walkthrough
- **docs**: generate the API reference from docs/openapi.json
- **docs**: Starlight site skeleton with splash home and install.sh
- **config**: find oms.toml from any directory
- **cli**: `oms update` — replace the binary with a verified release

### Fix

- **docs**: address final review — Node floor, duplicate operationId, old URLs, accuracy
