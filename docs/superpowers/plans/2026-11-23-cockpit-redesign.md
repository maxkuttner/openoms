# Cockpit Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the cockpit shell and pages with the agreed redesign (shadcn sidebar with Trade/Admin groups, Binance-style Trade entry, Strategies, API, Portfolios, Orders, Jobs, Events), UI only, sample data where no API exists.

**Architecture:** One nav table in `src/nav.ts` drives sidebar, command menu and titles. Pages consume real APIs where they exist; market data, jobs and events go through small source interfaces (`src/marketData.ts`, `src/sampleFeeds.ts`) whose only implementations today are deterministic sample generators, so a back end can replace them without touching components. Pure logic lives in tested `.ts` modules; components stay thin.

**Tech Stack:** React 18, react-router 7, TanStack Query, Tailwind, shadcn/ui (radix-nova), lightweight-charts, shadcn Charts (Recharts). Rust/axum + sqlx for the one server field.

**Spec:** `docs/superpowers/specs/2026-11-23-cockpit-redesign-design.md` (mockups: `docs/superpowers/specs/cockpit-redesign-canvas/*.dc.html`)

## Global Constraints

- Work in `/Users/max/workarea/oms-cockpit-redesign` (branch `feat/cockpit-redesign`); front end in `cockpit/`.
- shadcn components only (add via `npx shadcn@latest add <name>` into `src/components/ui/`); the single non-shadcn UI dependency is `lightweight-charts`, with its TradingView attribution left on.
- Theme from `src/styles/tokens.css` only: buy/primary `#25b083`, amber `#ce9a3b`, sell `#db3440`, radius 5px. No new colours or hex literals in components; use theme classes / CSS vars.
- Every sample-fed surface shows a shadcn `Badge` with the exact text `Sample data`.
- Sample prices never prefill or reach the order ticket.
- `OrderTicket`, `StrategyTicket`, `TradeBlotter`, `PositionsPage` keep their logic unchanged.
- No compat shims; old paths redirect: `/positions` → `/portfolios`, `/reported` → `/strategies`.
- Front-end tests: `node --test src/*.test.ts` (`npm test`); tested modules import no runtime dependencies.
- Rust DB tests: only against the test Postgres (`OMS_RELOAD_TEST_DATABASE_URL=postgres://postgres:pw@127.0.0.1:55432/ods`), never `.env`; with `--ignored` always add `--skip real_docker`.
- Gate per task: `cd cockpit && npm test && npx tsc --noEmit -p .` green; final task also `npm run build`.

## Review Focus

1. Non-admin session: Admin group and admin command items absent; `/admin/*` still refused by the existing guard. → Task 2 test `non-admin sidebar has no Admin group`.
2. Instrument with no sample seed / unknown id: sample source must still return a stable, non-empty series (no crash, no NaN). → Task 3 test `unknown instrument gets a stable series`.
3. Order book cumulative totals and bar widths with zero/empty levels: widths in 0–100, no division by zero. → Task 3 test `empty book yields no rows and no NaN`.
4. Old bookmarks `/positions`, `/reported` land on the new pages instead of `/`. → Task 2 test `legacy paths map to new pages` (pure `legacyRedirect`).
5. Portfolio with no positions / user with no viewable portfolios on Portfolios and Orders: empty state, no request with an undefined id. → Task 8 test `pickDefaultPortfolio returns null for none`.

---

### Task 1: Server — `token_active` on reported strategy status

**Files:**
- Modify: `src/reported/status.rs` (struct `Status`, `get_status`)
- Modify: `cockpit/src/reported.ts` (`ReportedStatus`)
- Test: `src/reported/ingest_tests.rs`

**Interfaces:**
- Produces: `pub async fn token_active(pool: &PgPool, code: &str) -> Result<bool, sqlx::Error>` in `src/reported/status.rs`; `Status.token_active: bool` in JSON; TS `ReportedStatus.token_active: boolean`.

- [ ] **Step 1: Failing test** `token_active_tracks_issue_and_revoke` (`#[tokio::test] #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]`): with the fixture strategy (which has an issued token) `assert!(token_active(&pool, code).await?)`; after `UPDATE strategy_reporting_token SET revoked_at = now() WHERE strategy_code = $1` → `assert!(!token_active(...))`.
- [ ] **Step 2:** `OMS_RELOAD_TEST_DATABASE_URL=postgres://postgres:pw@127.0.0.1:55432/ods cargo test reported::ingest_tests::token_active -- --ignored --skip real_docker` → FAIL (unresolved `token_active`).
- [ ] **Step 3:** Implement `token_active` as `SELECT EXISTS (SELECT 1 FROM strategy_reporting_token WHERE strategy_code = $1 AND revoked_at IS NULL)`; add `token_active` to `Status` and fill it in `get_status`; add the TS field.
- [ ] **Step 4:** Same command → PASS; `cargo check -q` clean.
- [ ] **Step 5:** `git commit -m "feat(reported): expose token_active on strategy status"`

### Task 2: Nav model and app shell

**Files:**
- Modify: `cockpit/src/nav.ts`, `cockpit/src/nav.test.ts`
- Modify: `cockpit/src/shell/Shell.tsx`, `cockpit/src/shell/UserMenu.tsx`, `cockpit/src/shell/CommandMenu.tsx`, `cockpit/src/App.tsx`
- Create (CLI): `cockpit/src/components/ui/sidebar.tsx` (+ its deps, e.g. `hooks/use-mobile.ts`)

**Interfaces:**
- Produces in `nav.ts`:
  - `type NavGroup = { id: "trade" | "admin"; title: string; pages: Page[] }`
  - `TRADE_GROUP: NavGroup` = Trade entry `/`, Strategies `/strategies`, API `/api`
  - `MAIN_PAGES: Page[]` = Portfolios `/portfolios`, Orders `/orders`, Jobs `/jobs`, Events `/events`
  - `ADMIN_GROUP: NavGroup` = People `/admin/users`, Trading setup `/admin/portfolios`, Connectivity `/admin/broker-connections`, Monitoring `/admin/blotter` (first page of each `ADMIN_TABS` entry, derived, not hand-copied)
  - `sidebarFor(isAdmin: boolean): { groups: NavGroup[]; pages: Page[] }` (admin group only when `isAdmin`)
  - `legacyRedirect(path: string): string | null` (`/positions`→`/portfolios`, `/reported`→`/strategies`, else null)
  - `SIDEBAR` removed; `commandItems`, `pageFor` cover the new pages. `ADMIN_TABS`, `adminTabFor`, `adminRouteAllowed` unchanged.
- Consumes: `useSession().admin`, `Logo`, `JobsIndicator`, `ConnectionDot`, `CommandMenu`.

- [ ] **Step 1: Failing tests** in `nav.test.ts` (replace the old SIDEBAR test):
```ts
test("trade group is Trade entry, Strategies, API", () =>
  assert.deepEqual(TRADE_GROUP.pages.map((p) => [p.title, p.path]), [["Trade entry", "/"], ["Strategies", "/strategies"], ["API", "/api"]]));
test("main pages", () => assert.deepEqual(MAIN_PAGES.map((p) => p.path), ["/portfolios", "/orders", "/jobs", "/events"]));
test("admin group deep-links each admin tab's first page", () =>
  assert.deepEqual(ADMIN_GROUP.pages.map((p) => [p.title, p.path]), [["People", "/admin/users"], ["Trading setup", "/admin/portfolios"], ["Connectivity", "/admin/broker-connections"], ["Monitoring", "/admin/blotter"]]));
test("non-admin sidebar has no Admin group", () => {
  assert.deepEqual(sidebarFor(false).groups.map((g) => g.id), ["trade"]);
  assert.deepEqual(sidebarFor(true).groups.map((g) => g.id), ["trade", "admin"]);
  assert.ok(commandItems(false).every((p) => !p.path.startsWith("/admin")));
});
test("legacy paths map to new pages", () => {
  assert.equal(legacyRedirect("/positions"), "/portfolios");
  assert.equal(legacyRedirect("/reported"), "/strategies");
  assert.equal(legacyRedirect("/orders"), null);
});
test("titles for new pages", () => {
  assert.equal(pageFor("/strategies")?.title, "Strategies");
  assert.equal(pageFor("/")?.title, "Trade entry");
});
```
- [ ] **Step 2:** `npm test` → FAIL (missing exports).
- [ ] **Step 3:** Implement the `nav.ts` exports above. `npx shadcn@latest add sidebar`. Rewrite `Shell.tsx` as `SidebarProvider` + `Sidebar collapsible="offcanvas"` (header: `Logo` + ⌘K button; `SidebarGroup` "Trade" with `SidebarMenuSub` items; `SidebarMenu` main pages with lucide icons; Admin group from `sidebarFor`; footer `UserMenu`) + `SidebarInset` with top bar (`SidebarTrigger`, `Separator`, page title from `pageFor`/"Admin Panel", `JobsIndicator` for admins, `ConnectionDot`). Active item via `NavLink`/`useLocation`. Remove "Admin" from `UserMenu` (keep Docs, Settings, Sign out). `App.tsx`: add routes `strategies`, `api`, `portfolios`, `orders`, `jobs`, `events` (until their own task: `strategies`→`ReportedStrategiesPage`, `portfolios`→`PositionsPage`, `orders`→`TradeBlotter`, `api`/`jobs`/`events`→ a `Notice` "Coming in this branch") and `positions`/`reported` → `<Navigate to={legacyRedirect(...)} replace />`.
- [ ] **Step 4:** `npm test && npx tsc --noEmit -p .` → PASS.
- [ ] **Step 5:** `git commit -m "feat(cockpit): shadcn sidebar shell with Trade and Admin groups"`

### Task 3: Sample market data module

**Files:**
- Create: `cockpit/src/marketData.ts`, `cockpit/src/marketData.test.ts`

**Interfaces:**
- Produces:
  - `type Timeframe = "1m" | "5m" | "1h" | "4h" | "1d"`
  - `type Candle = { time: number; open: number; high: number; low: number; close: number; volume: number }` (`time` = UTC seconds)
  - `type BookLevel = { price: number; size: number }`; `type Book = { bids: BookLevel[]; asks: BookLevel[] }` (bids desc, asks asc)
  - `type Ticker = { last: number; changePct: number; bid: number; ask: number }`
  - `interface MarketDataSource { sample: boolean; candles(instrumentId: string, tf: Timeframe, count: number): Candle[]; book(instrumentId: string, depth: number): Book; ticker(instrumentId: string): Ticker; subscribe(instrumentId: string, cb: () => void): () => void }`
  - `sampleMarketData: MarketDataSource` (`sample: true`; seeded PRNG from instrument id string hash; `subscribe` ticks via `setInterval` 1000 ms and returns a clearer)
  - `type BookRow = { price: number; size: number; total: number; widthPct: number }`; `bookRows(levels: BookLevel[], maxTotal: number): BookRow[]` (cumulative from best price outward)
  - `movingAverage(closes: number[], period: number): (number | null)[]`
- [ ] **Step 1: Failing tests** `marketData.test.ts`: `same instrument gives identical candles` (deepEqual for two calls, `candles("BTCUSDT","1h",100)` length 100, times strictly increasing by 3600); `candles are well formed` (every `low <= min(open,close)`, `high >= max(open,close)`, all finite); `unknown instrument gets a stable series` (`candles("",...)` finite and length as asked); `book is crossed-free` (`bids[0].price < asks[0].price`, bids descending, asks ascending); `bookRows accumulates` (`bookRows([{price:1,size:2},{price:0.9,size:3}], 5)` → totals `[2,5]`, widths `[40,100]`); `empty book yields no rows and no NaN` (`bookRows([], 0)` → `[]`); `moving average` (`movingAverage([1,2,3,4],2)` → `[null,1.5,2.5,3.5]`).
- [ ] **Step 2:** `npm test` → FAIL.
- [ ] **Step 3:** Implement (mulberry32 PRNG seeded by a 32-bit string hash; price level per instrument from the hash).
- [ ] **Step 4:** `npm test` → PASS.
- [ ] **Step 5:** `git commit -m "feat(cockpit): sample market data source"`

### Task 4: Chart and order book components

**Files:**
- Modify: `cockpit/package.json` (`lightweight-charts`)
- Create: `cockpit/src/components/market/PriceChart.tsx`, `cockpit/src/components/market/OrderBook.tsx`, `cockpit/src/components/market/SampleBadge.tsx`, `cockpit/src/components/market/useThrottled.ts`

**Interfaces:**
- Consumes: Task 3 `MarketDataSource`, `bookRows`, `movingAverage`, `Timeframe`.
- Produces: `<PriceChart source={MarketDataSource} instrumentId={string} />` (internal Tabs: timeframe `1m 5m 1h 4h 1d`, view `Price | Depth`; Price = candlestick + volume histogram on its own price scale + MA(20) line; Depth = two area series bids/asks; colours from CSS vars read via `getComputedStyle`; resizes with `ResizeObserver`; disposes chart on unmount); `<OrderBook source instrumentId depth={number} />` (Card + Table: Price/Size/Total, asks above, spread row, bids below; bar width from `widthPct` as a positioned background div; updates throttled to ≤10/s via `useThrottled(value, 100)`); `<SampleBadge />` = `Badge variant="outline"` with text `Sample data`. Both components render `SampleBadge` when `source.sample`.
- [ ] **Step 1:** `npm i lightweight-charts`; build the components.
- [ ] **Step 2:** `npm test && npx tsc --noEmit -p .` → PASS.
- [ ] **Step 3:** `git commit -m "feat(cockpit): price chart and order book components"`

### Task 5: Trade entry page

**Files:**
- Modify: `cockpit/src/pages/trading/Trade.tsx`
- Create: `cockpit/src/components/trading/InstrumentHeader.tsx`, `cockpit/src/components/trading/InstrumentPicker.tsx`
- Delete when unused: `cockpit/src/components/trading/InstrumentSearchModal.tsx` (only if the picker fully replaces it, including the legs flow)

**Interfaces:**
- Consumes: `PriceChart`, `OrderBook`, `SampleBadge`, `sampleMarketData`; existing `Watchlist` data/API, instrument search API used by `InstrumentSearchModal`, `OrderTicket`, `StrategyTicket`, `TradeBlotter`, `PositionsPage`.
- Produces: `<InstrumentPicker onPickSingle onPickLegs />` (`Popover` + `Command`; first `CommandGroup` = watchlist; asset-class filter `Tabs`; same callbacks/behaviour as `InstrumentSearchModal`); `<InstrumentHeader instrument portfolioId />` (picker trigger showing symbol + asset class/venue; last/change/bid/ask from `sampleMarketData.ticker` with `SampleBadge`; position from the real positions API).
- [ ] **Step 1:** Layout per `cockpit-redesign-canvas/Main.dc.html`: header strip; row with `PriceChart` (flex-grow) and a right column (≤360px) holding `OrderBook` above the ticket (`ToggleGroup` Single | Combo switching `OrderTicket` / `StrategyTicket`); under the chart the existing Orders/Positions `Tabs`. Right column stacks below the chart under `lg`. No prefill from book/price.
- [ ] **Step 2:** `npm test && npx tsc --noEmit -p .` → PASS; `npm run dev` and load `/` once to check it renders (report what you saw).
- [ ] **Step 3:** `git commit -m "feat(cockpit): Binance-style trade entry page"`

### Task 6: Strategies page

**Files:**
- Create: `cockpit/src/pages/trading/Strategies.tsx`
- Delete: `cockpit/src/pages/trading/ReportedStrategies.tsx` (after moving its pieces)
- Modify: `cockpit/src/App.tsx`, `cockpit/src/reported.ts` (+ `reported.test.ts`)

**Interfaces:**
- Consumes: Task 1 `token_active`; existing `listReported`, `createReported`, `rotateToken`, `revokeToken`, `getStatus`, `getBatches`, `fillsPath`, `staleLabel`, `TokenModal` logic.
- Produces: `tokenActions(status: { token_active: boolean } | undefined): { rotateLabel: "Rotate token" | "Issue token"; canRevoke: boolean }` in `reported.ts`.
- [ ] **Step 1: Failing test** `token actions follow token_active`: `tokenActions({token_active:true})` → `{rotateLabel:"Rotate token", canRevoke:true}`; `false` → `{"Issue token", false}`; `undefined` → `{"Rotate token", false}`.
- [ ] **Step 2:** `npm test` → FAIL. **Step 3:** implement; build the page per `Strategies.dc.html`: create form Card; grid of strategy Cards (code, name, health `Badge`, last report, last sequence, open breaks, portfolio, stale-after, token `Badge`, rotate/issue, revoke only when `canRevoke`, invalidate the status query after rotate/revoke); fills/batches/breaks open in a `Sheet` with the existing tables. Route `/strategies`.
- [ ] **Step 4:** `npm test && npx tsc --noEmit -p .` → PASS. **Step 5:** `git commit -m "feat(cockpit): strategies page"`

### Task 7: API page and slimmer Settings

**Files:**
- Create: `cockpit/src/pages/trading/Api.tsx`
- Modify: `cockpit/src/login/AccountSettings.tsx`, `cockpit/src/App.tsx`

**Interfaces:** Consumes `ApiKeysBox`, the `ownKeys` source (move it to `src/components/userKeys.ts` if not already exported there). Produces route `/api`.
- [ ] **Step 1:** `ApiPage` = page header + `ApiKeysBox source={ownKeys}` with the existing intro text; remove `ApiKeysBox` from `AccountSettings`.
- [ ] **Step 2:** `npm test && npx tsc --noEmit -p .` → PASS. **Step 3:** `git commit -m "feat(cockpit): API keys page under Trade"`

### Task 8: Orders page

**Files:**
- Create: `cockpit/src/pages/trading/Orders.tsx`, `cockpit/src/orders.ts`, `cockpit/src/orders.test.ts`, `cockpit/src/portfolioView.ts`, `cockpit/src/portfolioView.test.ts`
- Modify: `cockpit/src/App.tsx`

**Interfaces:**
- Consumes: `/orders?portfolioId&limit` (as `TradeBlotter`), `BlotterRow`, `placedCell`, `tradeTimeCell`, `formatFees`, `OrderStatus`, `SideBadge`, `OrderTimeline`.
- Produces in `portfolioView.ts` (new, with `portfolioView.test.ts`): `pickDefaultPortfolio(portfolios: { portfolio_id: string; can_view?: boolean }[]): string | null` (first viewable, else null).
- Produces in `orders.ts`: `type StatusFilter = "all" | "open" | "filled" | "closed"`; `filterOrders<T extends { status: string; order_id: string; instrument_symbol?: string | null; client_order_id: string }>(rows: T[], f: StatusFilter, q: string): T[]` (open = not terminal; closed = terminal and not filled; `q` case-insensitive over order id, client order id, symbol).
- [ ] **Step 1: Failing tests** `pickDefaultPortfolio returns null for none` (`[]` → null; first `can_view !== false` otherwise) and `filterOrders`: open keeps `working`/`partially_filled`; filled keeps only `filled`; closed keeps `cancelled`/`rejected`/`expired`; query `"btc"` matches symbol `BTCUSDT`; empty query keeps all. Use the status strings `TradeBlotter`'s `TERMINAL` set uses (import it, don't copy).
- [ ] **Step 2:** FAIL. **Step 3:** implement; page per `Orders.dc.html`: portfolio `Tabs`, status `Tabs`, search `Input`, full-width `Table` with columns Placed, Trade time, Booked, Updated, Instrument, Side, Type, Status, Qty, Cum, Leaves, Avg px, Fees, Cancel (cancel via the same call `TradeBlotter` uses), row click → detail `Card` below with fields and `OrderTimeline`.
- [ ] **Step 4:** PASS + tsc. **Step 5:** `git commit -m "feat(cockpit): orders page"`

### Task 9: Portfolios page

**Files:**
- Create: `cockpit/src/pages/trading/Portfolios.tsx`
- Modify: `cockpit/src/portfolioView.ts`, `cockpit/src/portfolioView.test.ts`
- Create (CLI): `cockpit/src/components/ui/chart.tsx` (`npx shadcn@latest add chart`)
- Modify: `cockpit/src/App.tsx`

**Interfaces:**
- Consumes Task 8 `pickDefaultPortfolio`.
- Produces in `portfolioView.ts`: `exposureShares(rows: { instrument_symbol: string; market_value: number }[], top: number): { label: string; pct: number }[]` (by |market value|, top N then `"Other"`, pcts sum to 100 rounded); `samplePnl(seed: string, days: number): { date: string; pnl: number }[]`.
- Consumes: `me.portfolios`, positions API and `PositionRow` (`src/positions.ts`), `/orders`, `Me`.
- [ ] **Step 1: Failing tests**: `exposureShares` with 5 rows, top 3 → 4 entries ending `"Other"`, sum 100, negative values use absolute size; `samplePnl` deterministic per seed, length = days.
- [ ] **Step 2:** FAIL. **Step 3:** implement (field names as in `PositionRow`; adapt the exposure input type to it). Page per `Portfolios.dc.html`: portfolio `Tabs`; tiles (Open orders, Open positions real; Net value, Day P&L sample with `SampleBadge`); P&L `ChartContainer` area chart (sample, badge); exposure bars (real); positions table (reuse the table from `PositionsPage`, extract it if needed without changing its logic); recent orders (latest 10 from `/orders`). Empty states with `Notice` when no portfolio / no positions.
- [ ] **Step 4:** PASS + tsc. **Step 5:** `git commit -m "feat(cockpit): portfolios page"`

### Task 10: Jobs and Events pages (sample)

**Files:**
- Create: `cockpit/src/sampleFeeds.ts`, `cockpit/src/sampleFeeds.test.ts`, `cockpit/src/pages/Jobs.tsx`, `cockpit/src/pages/Events.tsx`
- Modify: `cockpit/src/App.tsx`

**Interfaces:**
- Produces: `type ScheduledJob = { name: string; description: string; cron: string; lastRun: string | null; nextRun: string; status: "ok" | "running" | "failed" | "paused" }`; `type JobRun = { at: string; status: ScheduledJob["status"]; durationMs: number | null; note: string }`; `interface JobsSource { sample: boolean; jobs(): ScheduledJob[]; runs(name: string): JobRun[] }`; `type BusEvent = { at: string; topic: string; key: string; summary: string; stream: "bus" | "published" }`; `interface EventsSource { sample: boolean; events(stream: BusEvent["stream"], q: string): BusEvent[] }`; `sampleJobs`, `sampleEvents` (contents from `Jobs.dc.html` / `Events.dc.html`).
- [ ] **Step 1: Failing tests**: `sampleJobs.jobs()` has the five jobs named in `Jobs.dc.html`; `runs` returns newest first; `sampleEvents.events("published","")` only `published`; filter `"ord-8f21"` matches by key; filter is case-insensitive.
- [ ] **Step 2:** FAIL. **Step 3:** implement; pages per the mockups with `SampleBadge`, `Tabs` for streams, `Table`s, disabled "Run now"/"New job" buttons with a tooltip "Not wired yet".
- [ ] **Step 4:** PASS + tsc. **Step 5:** `git commit -m "feat(cockpit): jobs and events pages with sample data"`

### Task 11: Cleanup and build

**Files:** any now-unused files (`InstrumentSearchModal.tsx` if replaced, old placeholders), `docs-site` pages that name old cockpit paths (`/reported`, Settings → API keys).
- [ ] **Step 1:** `grep -rn "reported\"\|/positions\|API keys" cockpit/src docs-site/src` and fix stale references; remove dead code.
- [ ] **Step 2:** `cd cockpit && npm test && npx tsc --noEmit -p . && npm run build` → all green.
- [ ] **Step 3:** `git commit -m "chore(cockpit): remove old shell leftovers, update docs"`
