# Live watchlist + Day P&L / Net Exposure — design

## Context

`Design.pdf` (a UI mockup for the trader webapp) shows a fuller trade screen
than what exists today: a live watchlist, a price chart, a richer order
ticket, and an expanded Positions & P&L page. Investigating what's real
behind each element turned up three very different tiers:

- **Real and buildable now**: live watchlist pricing (the streaming feed
  data already exists server-side, `src/marks.rs`, just isn't exposed to the
  browser), and two Positions-page additions (Day P&L, Net Exposure) that
  are computable from data the app already has or can cheaply get from
  brokers' own existing endpoints.
- **Real concept, real new work**: a price chart needs historical bar
  storage that doesn't exist anywhere in this codebase; stop/stop-limit
  orders need domain-model + adapter work. Both explicitly deferred — not
  in this spec.
- **No backing data model at all**: buying power, a restricted-instrument
  list, order routing choice, iceberg/trail order attributes, sector
  allocation, VaR, beta-weighted delta, margin utilization. This OMS has no
  cash/margin engine, no compliance list, no smart-order-router, no sector
  metadata, no benchmark/beta data. Building any of these for real is its
  own project; building fake UI for them was explicitly rejected — left out
  entirely, not stubbed.

This spec covers only the first tier: **live watchlist pricing** and
**Day P&L / Net Exposure** on the Positions page. Order book / bid-ask depth
ladder is explicitly out of scope per the request that started this design.

## Goals

- A trader can search-add/remove symbols to a personal watchlist and see
  live price + day % change for each, ticking without a page reload.
- Clicking a watchlist row sets that instrument on the order ticket.
- Positions & P&L shows two more real numbers: Day P&L and Net Exposure.
- Reuses the transport pattern already in this app (polling via React
  Query) rather than introducing a new one (WebSocket) for one feature.
- Reuses/extends the existing per-instrument mark infrastructure
  (`MarkStore`) rather than building a parallel one-off cache — this is the
  "standardised setup" the request asked for: one new store, two consumers.

## Non-goals

- Historical price chart (needs new bar/candle storage — separate design).
- Order book / bid-ask depth ladder (explicitly excluded by the request).
- Stop / stop-limit order types (separate design; depends on whether
  Alpaca/Binance accept them natively, not yet investigated).
- Buying power, restricted-list checks, order routing, iceberg/trail
  attributes, est. commission, sector allocation, VaR, beta-weighted delta,
  margin utilization — no backing data model exists for any of these; not
  built as real features, not stubbed as decoration.
- Auto-seeding the watchlist from held positions — starts empty, trader
  adds symbols themselves.

## Architecture

`MarkStore` (`src/marks.rs`) already holds live bid/ask per instrument,
written continuously by `mark_router` from the streaming quote feeds. It has
no day-over-day reference price, and none of the streaming feeds carry one.

Add a sibling, same shape, different cadence and source:

- **`DailyStatsStore`** (new, `src/daily_stats.rs`) — `instrument_id ->
  DailyStat { prev_close: f64, ts: DateTime<Utc> }`, `Arc<RwLock<HashMap<..>>>`
  exactly like `MarkStore`. Read by both the watchlist's %-change column and
  the Positions page's Day P&L card — one store, two consumers, instead of a
  one-off cache per feature.
- **`DailySnapshot` trait** (new, `crates/dataprovider`) — `async fn
  daily_stats(&self, symbols: &[String]) -> Result<Vec<(String, f64)>,
  ProviderError>` (symbol -> prev_close). Implemented for the Alpaca and
  Binance adapters to start, using each broker's own existing snapshot/24hr-
  stats endpoint (Binance's 24hr ticker literally returns day change
  pre-computed; Alpaca's snapshot endpoint carries the previous daily bar).
  An adapter without an implementation simply never populates entries for
  its instruments — not an error, a quiet absence, same convention
  `Positions.tsx` already uses for positions with no live mark.
- **Poller** (new, alongside `mark_router` or as its own small task) — wakes
  every 5 minutes (day-change doesn't need sub-minute freshness), computes
  the "interesting set" (watchlist entries unioned with currently-held
  instruments — the same kind of derivation `mark_router`'s subscription set
  already does for live quotes), batches by adapter, calls `daily_stats`,
  writes results into `DailyStatsStore`. Runs independent of any HTTP
  request — a request never blocks on a broker call.
- **Watchlist persistence** (new table) — `watchlist_item(principal_id,
  instrument_id, created_at)`, composite primary key `(principal_id,
  instrument_id)`, no surrogate `id` (matches this repo's natural-key
  convention for this kind of row).

### API

- `GET /watchlist` / `POST /watchlist {instrument_id}` / `DELETE
  /watchlist/{instrument_id}` — principal-scoped via the existing session
  cookie, same as every other `/trade`-mounted route. No separate ownership
  check to write: every query is naturally `WHERE principal_id =
  $session_principal`.
- `GET /marks?instrument_ids=1,2,3` — reads both `MarkStore` and
  `DailyStatsStore`, returns `[{instrument_id, bid, ask, mid, prev_close,
  pct_change}]`. Fields with no data (adapter doesn't support daily stats,
  or no live mark yet) come back `null`, never a fabricated `0`.

### Frontend

- `cockpit/src/trade/components/Watchlist.tsx` — row list + an
  `InstrumentSelect`-based add control + a remove button per row. Polls
  `GET /marks` for its current symbol set on a ~3s `refetchInterval`
  (matches `TradeBlotter`'s existing polling cadence — no new transport
  pattern). Clicking a row sets `OrderTicket`'s instrument field.
- Wired into `TradePage`'s layout as a new column alongside the existing
  order ticket / blotter columns.
- `PositionsPage` gains two more `SummaryCard`s: **Day P&L** and **Net
  Exposure**. Both computed client-side from the existing
  `/portfolios/{id}/positions` response merged with a `/marks` call for the
  held instrument ids — no new server-side aggregation endpoint.
  - Day P&L = `Σ qty * (mark - prev_close)` over positions whose instrument
    has both a live mark and a `prev_close`; positions missing either are
    excluded from the sum, same "excluded, not zeroed" convention the page
    already uses for unrealized P&L.
  - Net Exposure = `Σ |market_value|` — pure computation over data already
    fetched, no new store involved.

## Data flow

1. **Watchlist pricing** — browser polls `GET /marks?instrument_ids=...`
   every ~3s. Handler reads `MarkStore.get()` + `DailyStatsStore.get()` per
   id, merges, returns. The live tick was already flowing into `MarkStore`
   before any of this; this just exposes it.
2. **Daily stats freshness** — independent of any request. Poller wakes
   every 5 min, computes the interesting set, calls each adapter's
   `daily_stats`, writes into `DailyStatsStore`.
3. **Day P&L / Net Exposure** — `PositionsPage` already polls positions
   every 5s; additionally polls `/marks` for held instrument ids and
   computes both cards client-side from the merge.

## Error handling

- Adapter has no `daily_stats` impl → that instrument's `prev_close` /
  `pct_change` are `null` in `/marks`; watchlist row shows price with no
  %-change, never a fake `0%`.
- Poller's per-adapter call fails (rate limit, network) → logged, that
  batch's instruments keep their last-known `DailyStat` (stale-but-present)
  rather than being wiped. `ts` is carried so a future pass can decide to
  visually flag staleness — not building that display logic now, just not
  architecting it out.
- `POST /watchlist` for a nonexistent `instrument_id` → 404, mirrors
  `InstrumentSelect`'s existing validation path (it only ever hands up ids
  its own search returned).
- Duplicate `POST /watchlist` for an already-watched instrument → idempotent
  (composite PK conflict handled, not a 500).
- Empty watchlist (including first-ever use) → empty list, no seeding.

## Testing

Backend Rust only — the cockpit frontend has no existing test suite or
convention (checked: no `*.test.ts*`/`*.spec.ts*` anywhere under
`cockpit/src`); frontend changes get the same manual in-browser verification
used for other recent trade-app work in this repo.

- `DailyStatsStore`: get/set/get_all — same shape of test as `MarkStore`'s
  own.
- `daily_stats` per adapter: unit tests against a mocked HTTP response
  (existing pattern — Alpaca/Binance adapters already mock their HTTP
  clients for order-submit tests).
- Interesting-set computation (watchlist ∪ held instruments, deduped): pure
  function, table test.
- `/marks`: merged fields correct; missing `DailyStat` → null not zero;
  unknown `instrument_id` in the query → excluded from the response, not a
  500 for the whole batch.
- `/watchlist` CRUD: scoped to session principal; add of nonexistent
  instrument → 404; duplicate add → idempotent, not a 500.
- Poller: one adapter failing doesn't stop others from updating; a
  stale-but-present entry isn't overwritten with nothing on a failed pass.

## Open questions for the implementation plan

- Exact poll interval for the daily-stats poller (proposed: 5 min) and the
  frontend's `/marks` poll interval (proposed: ~3s, matching `TradeBlotter`).
- Which migration number this lands on (next available in
  `db/migrations/ods/oms/`).
