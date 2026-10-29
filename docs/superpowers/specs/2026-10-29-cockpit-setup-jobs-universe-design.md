# Cockpit-driven setup: background jobs, instrument universes, on-demand derivatives — design

Date: 2026-10-29
Status: approved by owner 2026-10-29

## Intent

The cockpit is the intended way to configure openoms. Today the published guide
"Your first paper trade" (docs-site/src/content/docs/getting-started/first-paper-trade.md)
walks a new user through `curl` against the admin API plus a terminal command,
because one step — loading a broker's instruments — cannot be done from the cockpit
at all, and because work the OMS does in the background (catalog sync on boot,
enrichment, reload after a credential change) is invisible there.

The owner's framing:

1. The guide must show the cockpit UI, because that is how the OMS is meant to be
   configured.
2. Actions in the cockpit (saving credentials, loading instruments) start long-running
   work. The user must be able to see what is running, what finished, and what failed.

Discussed and decided along the way: derivative contracts (options) are not loaded
as a universe at all; they are browsed live from the broker and stored only when used.

**Success:** a new user goes from `oms` running to a paper order using the cockpit
for every configuration step — no `curl`, no terminal — and can see, in the cockpit,
every background job those steps started and how it ended.

## Scope, in delivery order

Three parts that ship separately, in this order (B depends on A; C is independent but
changes what B covers; D needs A+B released):

- **A. Background jobs** — persisted jobs and a Task Center in the cockpit.
- **B. Instrument universe per broker connection** — status, load, reload, and the
  prompt after saving credentials; runs as jobs.
- **C. On-demand derivatives** — browse option chains live from the broker; store a
  contract only when it is used.
- **D. Cockpit-first "first paper trade" guide** — rewrite after A+B ship in a release.

## Decisions (owner's)

- **The cockpit is the configuration surface.** No new admin CLI. The CLI stays for
  what the cockpit cannot do (install, `oms init`, migrate, update, service).
  Automation uses the documented admin API.
- **Jobs are persisted in Postgres.** A job that was running when the OMS stopped is
  shown as **interrupted** after the next start.
- **Saving credentials prompts** whether to load the instrument universe for that
  connection now ("Load now" / "Later"). It does not start silently.
- **Each connection shows its universe status:** not loaded, loading (with progress),
  loaded, partial, failed. **Partial covers both**: a load that was interrupted, and a
  load that finished but skipped instruments (e.g. Alpaca assets on a venue with no
  MIC mapping — today "TXSE", 10 assets).
- **Users can reload the universe or load it manually** from the connection view.
- **Derivatives: browse live, store on use** (the IBKR TWS / institutional-OMS
  "create security on the fly" pattern):
  - Browsing an option chain (underlying → expiries → a strike window around the
    current price) queries the broker live and is **not** stored.
  - A contract is written to the catalog the moment it is **ordered, held or
    watchlisted**, and from then on is a normal instrument.
  - The strike window (e.g. ±10 strikes) is a display setting, not a storage rule.
- **The instrument universe of a connection = its base instruments only** (Alpaca:
  US equities and ETFs, ~14,400; Binance: spot pairs). Option chains are never part
  of it.

## A. Background jobs

### What a job is

One-off background work with a beginning and an end. Existing work that becomes jobs:

| Job type | Today |
|---|---|
| Instrument universe load / reload (per connection) | `oms setup sync-broker`; `bootstrap::spawn_sync` on boot |
| Catalog enrichment (OpenFIGI) | part of sync unless `--no-enrich` |
| Symbology backfill | `POST /admin/symbology/backfill` |
| Expiry sweep | `POST /admin/instruments/expiry-sweep` |

Always-on services (execution streams, price feeds, daily-stats poller, order recon on
reconnect) are **not** jobs. They are out of scope here; a separate status view built
on `src/stream_health.rs` is a later project.

### Record

One row per job in a new `oms`-schema table. Fields:

- id, type, subject (e.g. connection code `alpaca-paper`), started by (an admin action
  or `system` — e.g. boot), parameters (JSON)
- status: `queued` → `running` → `succeeded` | `failed` | `interrupted`
- created / started / finished timestamps
- progress: `done` and `total` counts when the job can count them; null otherwise
- result summary (JSON, shown as text — e.g. "14,404 loaded, 10 skipped: TXSE")
- error text when failed
- the job's own log lines (append-only; bounded — see builder notes)

At startup, every job left `queued` or `running` is set to `interrupted`.

### Behaviour

- Jobs started from the cockpit return immediately with the job id; the work runs in
  the background.
- Only one universe load per connection at a time; a second request while one runs
  is refused with a message naming the running job.
- Interrupted and failed jobs are never re-run automatically. The UI offers "Run
  again" where the job type is safe to repeat (universe load is: it upserts).

### Cockpit

- A **jobs indicator** in the cockpit top bar: hidden when idle, "Jobs · N running"
  while work runs, and a failure marker if a job failed since it was last opened.
- It opens a **Task Center** panel: recent jobs, newest first, with status, subject,
  progress bar (when counts are known), duration, result or error, and the job's log
  on expand.
- No raw server-log pane.

## B. Instrument universe per broker connection

### Connection view

The Broker connections page gets an "Instrument universe" column per connection
(mockup: `.superpowers/brainstorm/*/content/universe-status.html`, ignored by git):

| State | Shows | Actions |
|---|---|---|
| Not loaded | — | Load (disabled until credentials are configured) |
| Loading | progress `done / total` | — |
| Loaded | count, what it covers, when | Reload |
| Partial | count, and why: interrupted, or N skipped (with reason) | Resume / Reload |
| Failed | the error | Retry |

The status derives from the latest universe-load job for that connection plus the
catalog's current count for that broker.

### After saving credentials

When credentials are saved for a connection whose universe is not loaded, the cockpit
asks: "Load Alpaca's instrument universe now? About 14,400 stocks and ETFs — runs in
the background." with **Load now** / **Later**. "Load now" starts the job and links to
it. When the universe is already loaded, no prompt.

### Load on boot

The existing automatic sync on boot (`OMS_SYNC_ON_BOOT`, `bootstrap::will_sync_on_boot`
/ `spawn_sync`) becomes a universe-load job started by `system`, visible in the Task
Center like any other.

### Enrichment

Universe load from the cockpit does not enrich (the `--no-enrich` path: seconds instead
of a long rate-limited OpenFIGI run). Enrichment becomes its own job type, started
manually.

## C. On-demand derivatives

### Browse (not stored)

- The trade app's option chain flow (today: `GET /instruments/options/expiries` and
  `GET /instruments/options/chain`, `src/instruments_api.rs:219-236`, read from
  `public.instrument_derivative`) changes to query the broker live: expiries for an
  underlying, then the contracts of one expiry.
- Results are cached in memory for a short time (minutes) so reopening a chain is
  instant; nothing is written to Postgres.
- The UI shows a strike window around the underlying's current price (default ±10,
  user-adjustable) — display only.
- **Quotes for browsed contracts (owner's decision):** from Alpaca's option chain
  snapshot endpoint, `GET https://data.alpaca.markets/v1beta1/options/snapshots/{underlying}`,
  filtered to the visible window (`expiration_date`, `strike_price_gte`/`_lte`,
  `type`). One call returns bid/ask, last trade, greeks and implied volatility per
  contract; refreshed every few seconds while the chain is open, not stored.
  - Feed: request `feed=opra` first; if Alpaca refuses it (the account has no OPRA
    subscription — real-time OPRA needs Algo Trader Plus, $99/month), fall back to
    `feed=indicative`, which every Alpaca account gets free (modelled quotes, trades
    delayed 15 minutes). Indicative prices are labelled "indicative" in the chain so
    nobody mistakes them for real quotes.
  - Rate limit on the free plan: 200 API calls/minute, shared with everything else the
    OMS calls on Alpaca — the refresh interval must respect it.
  - Stored contracts keep today's path: marks from the Databento OPRA stream, keyed
    by instrument id.
  - **Databento for browsed chains is a later upgrade, not this project (owner's
    decision):** when a Databento OPRA connection is configured, browsed chains would
    use its real NBBO, with Alpaca snapshots as the fallback. It needs the feed to
    subscribe and unsubscribe by option symbol for contracts that are not stored
    (today `src/opra_stream.rs` / `src/quote_feed.rs` follow stored contracts only,
    by instrument id), and greeks/IV computed by the OMS — so it stays off this
    project's critical path.

### Store on use

**Where (owner's decision): a separate "add this contract" step, never inside order
submission.** An admin/trading API endpoint takes an option contract (OCC symbol, via
the configured Alpaca connection), writes it to the catalog if missing, and returns
its instrument id. The cockpit calls it when a strike is picked; API and Python users
call it first. Order submission and watchlists keep taking an instrument id exactly as
today, so a slow or failing broker can never stall an order or a risk check.

A contract is written to the catalog (`instrument`, `instrument_derivative`, and the
broker's `broker_instrument` mapping — the same writer as today's sync,
`src/setup/catalog.rs`) when it is:

- **ordered** — before the order is accepted, so the order references a real
  instrument id as today;
- **held** — positions already require an instrument row; covered by "ordered";
- **watchlisted** — when added to a watchlist.

From then on it is a normal instrument: search, positions, risk checks, recon,
OPRA marks and expiry handling work unchanged.

### Removed

Wholesale option seeding by underlying (`--underlyings` on `sync-broker`,
`OMS_SYNC_UNDERLYINGS`) and the "also load option chains" field from the mockup go
away. (Single-user prerelease: no compatibility shims needed.)

## D. Guide rewrite

After A+B ship in a release, `first-paper-trade.md` becomes cockpit-first: credentials
(with the load prompt) → Principals → Portfolios → Accounts → trading token (Api page)
→ one order via API/Python. One line for remote servers:
`ssh -L 3001:localhost:3001 yourserver`. The current `curl` setup moves to an optional
"Prefer the API?" section. Verified by running it, as before.

Per the project rule recorded 2026-10-29: user-facing doc changes publish together
with the release that makes them true.

## Non-goals

- A status view for always-on services (streams, feeds, pollers).
- An activity/notification feed beyond the Task Center's job history.
- Raw server logs in the cockpit.
- An admin CLI.
- Loading a full derivatives universe from any broker.
- IBKR/Bybit universe loading (they have no `sync-broker` support today).
- Databento quotes for browsed (unstored) option chains — a later upgrade (see C).

## Anti-goals

- Background work that runs with no visible trace in the cockpit.
- A load that silently skips instruments and reports "Loaded".
- Storing every option contract a user merely looked at.

## Constraints

- Rust (axum, sqlx, Postgres) server; React + Mantine cockpit; migrations in
  `db/migrations/ods/{public,oms}/` applied by `oms database migrate` — the server never
  migrates on its own.
- The admin API stays documented: new endpoints get utoipa annotations, a summary, a
  unique operationId, and `docs/openapi.json` regenerated (tests enforce all three).

## Calls made by the designer (2026-10-29, after the builder check)

Not decided by the owner; marked so they can be checked.

- **Chain source:** only Alpaca lists option contracts, so live chains come from the
  configured Alpaca connection (`ALPACA_ENV` decides paper or live). With no Alpaca
  connection configured, the chain shows "Connect Alpaca to browse option chains".
  A stored contract gets a `broker_instrument` mapping for the connection that served
  it only.
- **Scheduled expiry sweep** (`expiry::run`, hourly, `src/main.rs:1096`): a run that
  expires nothing writes no job; a run that expires at least one instrument writes a
  `system` job with the count, so the Task Center stays quiet but nothing changes
  state invisibly.
- **Existing synchronous admin endpoints** (`POST /admin/symbology/backfill`,
  `POST /admin/instruments/expiry-sweep`) become jobs: they answer `202` with the job
  id. Single-user prerelease — no compatibility form kept. Enrichment, backfill and
  sweep stay API-only (no cockpit buttons) in this project; they appear in the Task
  Center when they run.
- **Boot load:** the trigger becomes per connection — at startup, a universe-load job
  starts for each connection with credentials whose universe was never loaded. It
  skips enrichment like cockpit loads. `OMS_SYNC_ON_BOOT=never` stays as the opt-out;
  `OMS_SYNC_UNDERLYINGS` goes.
- **"Resume" is "Load again":** a universe load re-fetches the full list and upserts,
  so there is no checkpointing. The Partial state's action is labelled "Load again".
- **Credential save → adapter/stream restart** (`src/reload.rs`) stays synchronous
  and is not a job: the save response already reports its outcome, and the cockpit
  shows it.
- **Option contracts already seeded** by `--underlyings` stay as normal instruments.
  No purge.

## Left to the builder

- Table and column names; the job log's storage (column vs. child table) and its size
  bound.
- How the cockpit learns about job changes (polling vs. push).
- The in-memory cache's exact lifetime and size.
- How the strike window centres when the underlying has no live price.
- How a running universe load reports progress `total` before it has fetched the list.
