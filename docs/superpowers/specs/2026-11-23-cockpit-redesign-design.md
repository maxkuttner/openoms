# Cockpit redesign — design

Status: approved approach A (replace in place), UI only. Canvas with the agreed
screens: the "OpenOMS UI Redesign" design artifact (private to the owner).

## Intent

The cockpit grew page by page: a small sidebar (Trade, Positions, Reported
strategies), account Settings that hide the API keys, and an Admin Panel behind
the user menu. The owner drew a new structure (whiteboard sketch) and we
iterated it into a canvas. The goal is a trading UI that feels like Binance /
mmt.gg on the trade screen and like a modern shadcn app everywhere else, so the
OMS is pleasant to test and demo while the back end grows.

## Goals

- New app shell: shadcn `Sidebar` (`collapsible="offcanvas"`), official logo
  (`components/Logo.tsx`), top bar with `SidebarTrigger` (Ctrl/Cmd+B) and the
  page title, existing top-bar extras kept (`JobsIndicator` for admins,
  `ConnectionDot`, ⌘K `CommandMenu`).
- Navigation (in this order):
  - **Trade** group: Trade entry `/`, Strategies `/strategies`, API `/api`
  - Portfolios `/portfolios`
  - Orders `/orders`
  - Jobs `/jobs`
  - Events `/events`
  - **Admin** group (admins only): People, Trading setup, Connectivity,
    Monitoring → the existing Admin Panel tab routes (`/admin/users`,
    `/admin/portfolios`, `/admin/broker-connections`, `/admin/blotter`).
  - Footer: account `DropdownMenu` (Docs, Settings, Sign out). Admin is no
    longer in it.
- **Trade entry** (Binance layout): header strip (instrument button opening the
  search, last price, change, bid/ask, position, tick/lot); left a large chart
  (TradingView Lightweight Charts: candles + volume + MA line, timeframe Tabs,
  Price/Depth tab, Depth = two area series); right column order book (shadcn
  Table rows with cumulative depth bars, spread row) above the order ticket.
  Below the chart: the existing Orders / Positions tabs (TradeBlotter,
  positions table) so nothing the current trade page does is lost.
- Instrument search: shadcn `Popover` + `Command`, asset-class filter Tabs,
  watchlist shown as the first `CommandGroup`. Reuses the existing instrument
  search API and the existing combo/legs flow (StrategyTicket stays reachable
  from the ticket as Single | Combo).
- **Strategies**: today's `ReportedStrategies` page, restyled to the canvas
  (create form, one Card per strategy with status/token Badges, rotate/issue,
  revoke only while a token is active, link to fills/batches/breaks).
  Depends on the `token_active` status field (see Constraints).
- **API**: the existing `ApiKeysBox` with the session's own keys, moved out of
  Settings. Settings keeps password and two-step sign-in.
- **Portfolios**: portfolio picker (Tabs), summary tiles, positions table (the
  current Positions page content), recent orders for the portfolio, P&L chart
  and exposure bars (shadcn Charts / Recharts).
- **Orders**: full-width table with the blotter's columns (Placed, Trade time,
  Booked, Updated, Instrument, Side, Type, Status, Qty, Cum, Leaves, Avg px,
  Fees, Cancel), status filter Tabs, search; a selected order opens a detail
  Card with the existing order timeline (`OrderTimeline`, `/orders/:id/events`).
- **Jobs** and **Events**: the canvas screens, rendered from sample data.

## Non-goals (explicitly out of scope)

- Any market-data plumbing: candles history, live ticks, order book. The
  architecture (OMS server vs market gateway component vs browser-direct) is a
  separate decision.
- A job scheduler, message-bus event listing or Kafka publishing.
- Changing the theme tokens, or the Admin Panel's inner pages.
- New server endpoints.

## Anti-goals

- Sample data that could be mistaken for live data. Every surface fed by
  sample data shows a "Sample data" `Badge`, and sample prices never reach the
  order ticket's submit path.
- Custom widgets where a shadcn component exists. The only non-shadcn UI
  dependency is `lightweight-charts`.
- Keeping the old shell or old routes alongside (single user, pre-release: no
  compat shims). Old paths redirect: `/positions` → `/portfolios`,
  `/reported` → `/strategies`.

## Constraints

- Stack as is: React 18, react-router 7, TanStack Query, Tailwind, shadcn
  (`components.json`, style radix-nova). Add shadcn components with the CLI
  (`sidebar`, `toggle-group`, `chart`, and any other needed) into
  `src/components/ui/`.
- Theme from `src/styles/tokens.css` only: buy/primary #25b083, amber
  #ce9a3b, sell #db3440, radius 5px. No new colours.
- `lightweight-charts` (Apache-2.0) requires a visible TradingView attribution;
  keep the library's default attribution logo on.
- The order-book component throttles redraws to ~10 Hz so the later live feed
  can't flood React.
- Tests are `node --test` over pure `.ts` modules (`src/*.test.ts`); logic
  (nav model, sample-data generators, depth accumulation, filters) lives in
  pure modules with tests; components stay thin.
- `token_active` on `/strategies/reported/:code/status` is uncommitted work on
  `feat/python-strategy-sdk`. This branch (`feat/cockpit-redesign`, from
  `origin/dev`) carries that small server change itself so Strategies works.

## Approaches considered

- **A. Replace in place (chosen).** New shell and routes replace the old ones;
  real APIs where they exist, sample data behind interfaces where they don't.
  Least code, no duplicate pages; fine for a single pre-release user.
- **B. Parallel `/app/next`.** Safer cut-over but every page exists twice and
  needs a cleanup pass later. Rejected.

## Decisions on the how

- Data seams, so the back end can plug in later without touching components:
  - `MarketDataSource` (`src/marketData.ts`): `candles(instrumentId, timeframe)`,
    `subscribeBook(instrumentId, cb) → unsubscribe`, `ticker(instrumentId)`.
    One implementation now: `sampleMarketData` (deterministic generator seeded
    by instrument id so screenshots are stable, slow random walk for "live").
  - `JobsSource` and `EventsSource` with sample implementations.
- Sidebar active state from the router (`NavLink`), Admin group rendered only
  when `useSession().admin`; the server already enforces admin routes.
- Nav model stays in `src/nav.ts` (pure, tested): sidebar groups, admin group,
  command-menu entries and page titles come from one table.
- Charts: Lightweight Charts for price and depth; shadcn Charts (Recharts) for
  Portfolios P&L/exposure.

## Reference mockups

The canvas screens are copied as HTML into
`docs/superpowers/specs/cockpit-redesign-canvas/` (`Main` = Trade entry,
`Strategies`, `Api`, `Portfolios`, `Orders`, `Jobs`, `Events`). They are the
source for columns, fields, copy and layout of every screen. Their inline
styles are mockup-only; build with shadcn components and theme classes.

## Data per surface (builder-check answers; calls marked *)

| Surface | Source | Sample badge |
|---|---|---|
| Trade header: last, change, bid/ask | sample `MarketDataSource` | yes |
| Trade header: position | real positions API for the ticket's portfolio * | no |
| Chart, depth, order book | sample `MarketDataSource` | yes |
| Strategies, API, Orders, order timeline | real APIs | no |
| Portfolios: positions table, exposure bars, recent orders | real (`/portfolios/:id/positions`, `/orders`) * | no |
| Portfolios: P&L-over-time chart, Net value / Day P&L tiles | sample (no history store exists) * | yes |
| Jobs | sample `JobsSource` (scheduled jobs don't exist; `/admin/jobs` background tasks stay in the Task Center) * | yes |
| Events | sample `EventsSource` | yes |

- Jobs and Events are visible to every signed-in user while they are sample
  screens. *
- Sample prices never prefill the ticket: clicking the book or the last
  price does nothing yet; the ticket keeps its own price entry. *
- `OrderTicket` and `StrategyTicket` keep their submission logic unchanged;
  only layout/classes change. *

## Navigation details (calls marked *)

- "Trade" is a group label, not a page; its children are Trade entry (`/`),
  Strategies, API. Admin likewise. *
- Admin Panel frame and its inner tabs stay. Sidebar Admin items deep-link to
  each tab's first page; `adminTabFor` keeps working. *
- The Admin Panel's own "API" page keeps its name; the user's "API" item lives
  under Trade, so the two are separated by group. *

## Page details (calls marked *)

- Trade's bottom tabs reuse `TradeBlotter` and `PositionsPage` unchanged. *
- Orders: `/orders` needs a portfolio, so the page has the same portfolio
  Tabs picker as Portfolios (default: first viewable); status filter and
  search are client-side over the loaded page. *
- Pure modules under test import no runtime dependencies; anything needing
  React or `lightweight-charts` stays in components. *

## Left to the builder

Component file layout, exact sample data, breakpoints (layout must work at
phone width: right column stacks under the chart), empty/loading states using
existing `Spinner`/`Notice`, and how the instrument header shows when nothing
is selected.
