# UI Updates (Phase 3) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The trade cockpit learns about changes over one WebSocket fed from both bus lanes. Blotter, positions and watchlist stop polling while it is connected.

**Architecture:**
- **Server:** `GET /updates` upgrades to a WebSocket behind the existing auth middleware. Each connection subscribes to both lanes and conflates events. Every `FLUSH_MS` it sends small JSON notices: `orders`, `positions`, `marks{instrument_ids}`, and `resync` after a lag.
- **Notices, not data:** a notice carries no order or position data. The cockpit reacts by invalidating the matching react-query queries, which refetch through the existing REST endpoints. Permission filtering therefore stays where it is today, in the REST handlers.
- **Fallback:** while the socket is down, the cockpit falls back to its old polling intervals and reconnects with backoff. On every (re)connect it invalidates everything once, to catch up.

**Tech Stack:** Rust, axum 0.7 (`ws` feature already on), tokio `broadcast`, tokio-tungstenite 0.21 (tests only). The cockpit uses React 18, @tanstack/react-query 5, and `node --test` for tests.

**Spec:** `docs/superpowers/specs/2026-11-04-target-architecture-design.md` on branch `docs/target-architecture`: §4.1 ("UI updates" row), §5 flows 1 and 2, §7 phase 3. Success check: "A fill appears in the blotter without polling".

## Global Constraints

- **Branch:** `feat/ui-updates`, created from `feat/kafka-outbox`. Merge order: … → kafka-outbox → ui-updates.
- **Scope (spec §7):** blotter (`TradeBlotter.tsx`, both queries), positions (`Positions.tsx`, positions and marks) and watchlist (`Watchlist.tsx`, marks).
  - Out of scope: the admin cockpit (`src/App.tsx`), `TaskCenter`, `UniverseStatus`, `StreamHealthStrip`, `api/jobs.ts` and the trade `ConnectionDot` health check. They keep polling.
- **The OMS can't be slowed by UI clients (spec §2):** sockets subscribe to `broadcast` lanes and never publish. A slow socket lags its own receiver only. On an order-lane `Lagged` the server sends `resync`; a price-lane `Lagged` is ignored, because marks conflate anyway.
- **Constants (exact):**
  - `FLUSH_MS = 250` for orders and positions;
  - `MARKS_FLUSH_MS = 1000` (marks at most once a second per socket);
  - reconnect backoff 1s doubling to a 30s cap.
- **Wire format (spec §8 leaves it to the builder, so this is my call):** one JSON text frame per notice, tagged by `"type"` in snake_case:
  - `{"type":"orders"}`
  - `{"type":"positions"}`
  - `{"type":"marks","instrument_ids":[1,2]}`
  - `{"type":"resync"}`

  No ids for orders or positions, so a socket leaks nothing a REST call wouldn't. Marks aren't sensitive.
- **Cross-site WebSocket hijacking:** an upgrade that carries an `Origin` header must pass `sessions::origin_is_allowed(headers, &Method::POST, public_base_url)`; with no `public_base_url`, refuse. The existing middleware exempts GET from the origin check, and a WebSocket upgrade is a GET, so the handler checks it itself. Non-browser API-key clients send no `Origin` and are not affected.
- **Dev proxy:** `cockpit/vite.config.ts` `/api` needs `ws: true`. Its `Origin` rewrite must also hook `proxyReqWs`, or a dev-browser upgrade is refused by the rule above.
- **Phase 1 deferred item folded in:** allocation transfers publish `PositionChanged` for every affected portfolio after commit. Without it, the positions screen would only notice a transfer once polling ran, which is exactly what this phase removes.
- **Don't commit the user's files** (`.gitignore`, `.ignore`, `docs/superpowers/plans/2026-09-28-options-search-chain-strategy.md`). Stage files explicitly.
- **No help text in the UI** (standing rule). The only visible change is the live/polling state of the existing connection dot's label; no new copy blocks.

## Review Focus

1. **The socket drops mid-session (OMS restart, laptop sleep).** The screens fall back to polling at once, reconnect with backoff, and refetch everything on reconnect, so nothing stays stale. Covered by Task 4 `reconnect_delay_doubles_to_cap` and `open_invalidates_everything`.
2. **A malicious page in another tab opens `wss://oms/updates` with the trader's cookie.** The upgrade is refused (403). Covered by Task 2 `upgrade_refused_for_foreign_origin`.
3. **A burst of 200 partial fills.** The socket sends a handful of `orders`/`positions` notices, not 400. Covered by Task 1 `burst_conflates_to_one_notice_each`.
4. **A slow socket falls behind the order lane.** It gets `resync` and the cockpit invalidates everything; the server never blocks. Covered by Task 1 `order_lag_becomes_resync`.
5. **An allocation transfer** updates the positions screen with no fill. Covered by Task 3 `transfer_publishes_position_changed_for_each_portfolio`.

---

### Task 1: Notices and conflation

**Files:**
- Create: `src/live_updates.rs`; add `mod live_updates;` in `src/main.rs`.

**Interfaces:**
- Consumes: phase 1 `OrderLaneEvent`, `MarkUpdate`.
- Produces:
  ```rust
  #[derive(Debug, Clone, PartialEq, serde::Serialize)]
  #[serde(tag = "type", rename_all = "snake_case")]
  pub enum UiNotice { Orders, Positions, Marks { instrument_ids: Vec<i64> }, Resync }

  #[derive(Default)]
  pub struct Conflator { /* orders: bool, positions: bool, resync: bool, marks: BTreeSet<i64> */ }
  impl Conflator {
      pub fn on_order(&mut self, r: Result<OrderLaneEvent, broadcast::error::RecvError>); // Appended→orders, PositionChanged→positions, Lagged→resync
      pub fn on_mark(&mut self, m: MarkUpdate);
      /// Pending order/position/resync notices; resync replaces orders+positions.
      pub fn take_fast(&mut self) -> Vec<UiNotice>;
      /// Pending marks as one notice (ids ascending), or nothing.
      pub fn take_marks(&mut self) -> Option<UiNotice>;
  }
  ```

- [ ] **Step 1: Write the failing unit tests:**
  - `burst_conflates_to_one_notice_each`: 200 `Appended` plus 200 `PositionChanged` make `take_fast()` return `[Orders, Positions]`; a second `take_fast()` returns `[]`.
  - `order_lag_becomes_resync`: `Appended` then `Err(Lagged(5))` make `take_fast()` return `[Resync]`.
  - `marks_collect_distinct_ids`: marks for ids 3, 1, 3 make `take_marks()` return `Some(Marks{instrument_ids: vec![1, 3]})`; then `None`.
  - `notices_serialize_with_type_tag`: `serde_json::to_string(&UiNotice::Orders)` is `{"type":"orders"}`, and the marks notice is `{"type":"marks","instrument_ids":[1,3]}`.
- [ ] **Step 2: Run, expect FAIL.** `cargo test --bin oms live_updates` fails: `UiNotice` not found.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run.** `cargo test --bin oms live_updates` passes 4/4.
- [ ] **Step 5: Commit** `src/live_updates.rs` and `src/main.rs`: `feat(ui-updates): conflate bus events into UI notices`.

### Task 2: `/updates` WebSocket endpoint

**Files:**
- Modify: `src/live_updates.rs` (`upgrade_allowed`, `pump`, `ws_updates` handler)
- Modify: `src/main.rs`: add `.route("/updates", get(live_updates::ws_updates))` to `orders_router` (behind `auth_middleware`).

**Interfaces:**
- Consumes: Task 1; `AppState::event_bus()`, `AppState::session_config`, `sessions::origin_is_allowed`.
- Produces:
  ```rust
  pub fn upgrade_allowed(headers: &HeaderMap, public_base_url: Option<&str>) -> bool;
  /// Forward conflated notices to `out` until it closes. Never publishes.
  pub async fn pump(order_rx: broadcast::Receiver<OrderLaneEvent>, price_rx: broadcast::Receiver<MarkUpdate>, out: mpsc::Sender<UiNotice>);
  pub async fn ws_updates(ws: WebSocketUpgrade, headers: HeaderMap, State(state): State<AppState>) -> Response;
  ```
  - `upgrade_allowed`: no `Origin` → true. `Origin` with no configured URL → false. Otherwise `origin_is_allowed(headers, &Method::POST, url)`.
  - `ws_updates`: 403 if not allowed. Otherwise upgrade. The socket task runs `pump` into an `mpsc(64)`, writes each notice as a JSON text frame, and stops when the client closes or a send fails.
  - `pump` timing: two `tokio::time::interval`s (`FLUSH_MS`, `MARKS_FLUSH_MS`, `MissedTickBehavior::Delay`) inside a `select!` with both receivers. On price-lane `Lagged`, continue. If either lane is `Closed`, stop reading it.

- [ ] **Step 1: Write the failing tests:**
  - `upgrade_refused_for_foreign_origin`: `Origin: https://evil.example` with url `Some("https://oms.example")` → false. The matching origin → true. No `Origin` → true. `Origin` present with url `None` → false.
  - `socket_receives_orders_notice_after_fill` (tokio test):
    1. Build `Router::new().route("/updates", get(ws_updates)).with_state(test_state)`. Reuse the `AppState` test fixture from `handlers.rs` tests (`connect_lazy`, no DB). Serve it on `127.0.0.1:0`.
    2. Connect with `tokio_tungstenite::connect_async` and no `Origin`.
    3. Publish `Appended` on `state.event_bus()` after a 50ms settle.
    4. Within 2s the client reads the text `{"type":"orders"}`.
  - `pump_stops_when_receiver_closes`: drop the `mpsc::Receiver`, publish an event. `pump` returns within 1s (`tokio::time::timeout`).
- [ ] **Step 2: Run, expect FAIL.** `cargo test --bin oms live_updates` fails: `upgrade_allowed` not found.
- [ ] **Step 3: Implement**, plus the route.
- [ ] **Step 4: Run.** `cargo test --bin oms live_updates` is green and `cargo test --bin oms` is green.
- [ ] **Step 5: Commit** `src/live_updates.rs` and `src/main.rs`: `feat(ui-updates): /updates websocket forwards conflated bus notices`.

### Task 3: Allocation transfers announce position changes

**Files:**
- Modify: `src/handlers.rs` `create_allocations` (after `tx.commit()`, ~line 1730)
- Modify: `src/event_bus.rs`

**Interfaces:**
- Produces: `pub fn publish_transfer(bus: &EventBus, from_portfolio: Uuid, to_portfolios: &[Uuid], instrument_id: &str)`. It publishes one `PositionChanged` for `from_portfolio`, then one per distinct `to` portfolio.

- [ ] **Step 1: Write the failing unit test** `transfer_publishes_position_changed_for_each_portfolio` in `event_bus.rs`. With from A and to `[B, C, B]`, the subscriber receives `PositionChanged` for A, B, C in that order, then `Empty`.
- [ ] **Step 2: Run, expect FAIL.** `cargo test --bin oms event_bus` fails: `publish_transfer` not found.
- [ ] **Step 3: Implement**, then call it in `create_allocations` after commit with the splits' portfolio ids.
- [ ] **Step 4: Run.** `cargo test --bin oms` is green.
- [ ] **Step 5: Commit** `src/event_bus.rs` and `src/handlers.rs`: `feat(bus): allocation transfers publish position changes`.

### Task 4: Cockpit live-updates client

**Files:**
- Create: `cockpit/src/trade/api/liveUpdates.ts`
- Create: `cockpit/src/liveUpdates.test.ts`. The test runner only globs `src/*.test.ts`.
- Modify: `cockpit/src/trade/App.tsx`:
  - mount `<LiveUpdatesProvider>` inside the existing `QueryClientProvider`;
  - `ConnectionDot`'s label says "Live updates" while live and keeps its current labels otherwise.
- Modify: `cockpit/vite.config.ts`: `ws: true` on `/api`, and the Origin rewrite on `proxyReqWs` as well.

**Interfaces:**
- Produces:
  ```ts
  export type UiNotice =
    | { type: "orders" } | { type: "positions" } | { type: "resync" }
    | { type: "marks"; instrument_ids: number[] };
  /// Which cached queries a notice makes stale. Keys as used today:
  ///   ["/orders", …]                    blotter + followed order
  ///   ["/portfolios", id, "positions"]  positions
  ///   ["/marks", ids[]]                 watchlist + positions marks
  export function staleFor(n: UiNotice): (queryKey: readonly unknown[]) => boolean;
  export function reconnectDelayMs(attempt: number): number;   // 1000 * 2^attempt, capped at 30000
  export function updatesUrl(loc: { protocol: string; host: string }, apiBase: string): string; // ws(s)://host + apiBase + "/updates"
  export function LiveUpdatesProvider(props: { children: React.ReactNode }): JSX.Element;
  export function useLive(): boolean;   // true while the socket is open
  ```
  - `marks` matches `["/marks", ids]` when the ids intersect. It also matches every positions query, because the server derives unrealized P&L from marks (spec §5 flow 2).
  - `resync` matches everything.
  - The provider:
    - on open: `setLive(true)`, then `queryClient.invalidateQueries()` (catch-up);
    - on message: `invalidateQueries({ predicate: q => staleFor(n)(q.queryKey) })`;
    - on close: `setLive(false)` and reconnect after `reconnectDelayMs(attempt++)`; reset `attempt` on open;
    - on unmount: close the socket and clear the timer.

- [ ] **Step 1: Write the failing tests** in `cockpit/src/liveUpdates.test.ts`:
  - `orders notice stales blotter queries only`: matches `["/orders", {…}]` and `["/orders", "id", "state"]`, not `["/marks", [1]]`.
  - `positions notice stales positions queries only`.
  - `marks notice stales intersecting marks and all positions`: ids `[3]` match `["/marks", ["3", "4"]]` (ids compare as strings: the watchlist carries instrument ids as strings) and `["/portfolios", "p", "positions"]`, not `["/marks", ["5"]]`.
  - `resync stales everything`.
  - `reconnect delay doubles to cap`: attempts 0, 1, 2 and 10 give 1000, 2000, 4000 and 30000.
  - `updates url follows page protocol`: `https:` gives `wss://h/updates`, and `http:` with base `/api` gives `ws://h/api/updates`.
- [ ] **Step 2: Run, expect FAIL.** `cd cockpit && npm test` fails: cannot find module `./trade/api/liveUpdates.ts`.
- [ ] **Step 3: Implement** the pure functions, then the provider and the hook.
- [ ] **Step 4: Run.** `cd cockpit && npm test` is green, and `npx tsc --noEmit -p .` is clean.
- [ ] **Step 5: Commit** the new files, `App.tsx` and `vite.config.ts`: `feat(cockpit): live-updates socket invalidates stale trade queries`.

### Task 5: Screens stop polling while live

**Files:**
- Modify:
  - `cockpit/src/trade/components/TradeBlotter.tsx` (the `/orders` list, and the followed order: keep its stop-when-moved rule, polling only when not live);
  - `cockpit/src/trade/pages/Positions.tsx` (positions and marks);
  - `cockpit/src/trade/components/Watchlist.tsx` (marks).

**Interfaces:**
- Consumes: Task 4 `useLive()`.

- [ ] **Step 1: Change each listed `refetchInterval: N`** to `refetchInterval: live ? false : N`, with `const live = useLive();`. Keep each N as it is today (2000, 5000, 3000, 3000).
- [ ] **Step 2: Run.** `cd cockpit && npm test && npx tsc --noEmit -p . && npm run build` are green.
- [ ] **Step 3: Manual check, which is the spec's success test:**
  1. Run `cargo build && ./target/debug/oms`, and `cd cockpit && npm run dev`.
  2. Open the trade app, then DevTools → Network.
  3. One `updates` WebSocket is open, and no repeating `/orders` or `/marks` requests appear while idle.
  4. Place a paper order: the blotter row appears and moves to filled with no polling request in between, only the refetch after the `orders` notice.
  5. Stop the OMS: the connection dot leaves "Live updates" and polling resumes.
  6. Restart the OMS: the socket reconnects within 30s.
- [ ] **Step 4: Commit** the three files: `feat(cockpit): blotter, positions and watchlist update live instead of polling`.
