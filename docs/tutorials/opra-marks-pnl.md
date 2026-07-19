# Tutorial: OPRA live marks → unrealized P&L

Implement mark-to-market valuation: feed Databento OPRA live quotes into a marks
store, then value open positions (unrealized P&L). The heavy accounting already
exists — `position` carries `net_qty` + `avg_cost` and `src/positions.rs`
(`PositionMath`) does average-cost accounting + realized P&L on every fill. You
are adding **one missing input (the mark)** plus a feed and a valuation join.

Options-first (matches the basic OPRA subscription). Equities/crypto marks can
feed the same store later.

## Mental model

```
Databento OPRA live (bbo, held symbols)
   → opra_stream task ──writes──► marks store (Arc in AppState)
                                     ├─► /positions valuation (net_qty + avg_cost + mark)
                                     └─► market-order pre-trade risk (est_price)
```

Two new things (marks store, feed) + one join (valuation).

Patterns to mirror that already exist in the repo:
- `src/stream_health.rs` — `Arc<RwLock<HashMap>>` behind a cheap-clone handle.
- `src/binance_stream.rs` — reconnect/backoff stream task + `stream_health` badge.
- `crates/dataprovider/src/providers/databento.rs` — fixed-point price decoding.

---

## Step 1 — The marks store

New file `src/marks.rs` (shape copied from `stream_health.rs`):

```rust
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use chrono::{DateTime, Utc};

#[derive(Clone, Copy)]
pub struct Mark {
    pub bid: f64,
    pub ask: f64,
    pub ts: DateTime<Utc>,
}
impl Mark {
    pub fn mid(&self) -> f64 { (self.bid + self.ask) / 2.0 }
}

#[derive(Clone, Default)]
pub struct MarksStore {
    inner: Arc<RwLock<HashMap<i64, Mark>>>, // key = our instrument.id
}

impl MarksStore {
    pub fn new() -> Self { Self::default() }

    pub fn set(&self, instrument_id: i64, bid: f64, ask: f64) {
        if let Ok(mut m) = self.inner.write() {
            m.insert(instrument_id, Mark { bid, ask, ts: Utc::now() });
        }
    }

    pub fn get(&self, instrument_id: i64) -> Option<Mark> {
        self.inner.read().ok().and_then(|m| m.get(&instrument_id).copied())
    }

    /// Snapshot for a bulk valuation query.
    pub fn all(&self) -> HashMap<i64, Mark> {
        self.inner.read().map(|m| m.clone()).unwrap_or_default()
    }
}
```

Start in-memory. Add an `oms.instrument_mark` table later only if you need
persistence/EOD — the store interface won't change.

---

## Step 2 — Put it in `AppState`

In `src/app_state.rs`, mirror the `stream_health` wiring: field + init + accessor.

```rust
// field
marks: MarksStore,
// in new():
marks: MarksStore::new(),
// accessor:
pub fn marks(&self) -> &MarksStore { &self.marks }
```

Add `mod marks;` in `src/main.rs`.

---

## Step 3 — Enable the Databento `live` dependency

The `databento` crate (0.54) has a `live` module; the OMS binary doesn't depend
on it directly yet (only `crates/dataprovider` uses `historical`). Add to the
**root** `Cargo.toml`:

```toml
databento = { version = "0.54", default-features = false, features = ["live"] }
```

Verify exact item names for your version: `cargo doc -p databento --no-deps --open`.

---

## Step 4 — The OPRA stream task

New file `src/opra_stream.rs`, structured like `binance_stream.rs`: a `run()`
with a reconnect loop, using a `stream_health::StreamHandle` for the cockpit badge.

### 4a. Load the symbol → instrument_id map

Databento OPRA `raw_symbol` is the space-padded OSI, stored verbatim in
`instrument.symbol` — no transform needed.

```rust
let rows = sqlx::query(
    "SELECT i.symbol, i.id \
     FROM position p \
     JOIN instrument i ON i.id = p.instrument_id \
     WHERE p.net_qty <> 0 AND i.instrument_class = 'OPTION'",
).fetch_all(&pool).await?;

let sym_to_id: HashMap<String, i64> = rows.iter()
    .map(|r| (r.get::<String,_>("symbol"), r.get::<i64,_>("id")))
    .collect();
let symbols: Vec<String> = sym_to_id.keys().cloned().collect();
```

### 4b. Connect + subscribe

Use `bbo-1s` (1-second top-of-book — tiny volume, ideal for marks):

```rust
use databento::{
    dbn::{Schema, SType},
    live::Subscription,
    LiveClient,
};

let mut client = LiveClient::builder()
    .key_from_env()?              // DATABENTO_API_KEY
    .dataset("OPRA.PILLAR")
    .build().await?;

client.subscribe(
    Subscription::builder()
        .symbols(symbols)         // the held OSI strings
        .schema(Schema::Bbo1S)    // or Schema::Mbp1 for every top-of-book tick
        .stype_in(SType::RawSymbol)
        .build(),
).await?;

client.start().await?;
health.set_live();
```

### 4c. Decode loop

Databento sends a `SymbolMappingMsg` (numeric `instrument_id` ↔ `raw_symbol`),
then quote records keyed by that numeric id. Keep a `dbn_id → our_id` map.
Prices are fixed-point `i64` scaled by `1e-9`; `i64::MAX` = undefined.

```rust
const UNDEF: i64 = i64::MAX;
fn px(v: i64) -> Option<f64> { (v != UNDEF).then(|| v as f64 / 1e9) }

let mut dbn_to_id: HashMap<u32, i64> = HashMap::new();

while let Some(rec) = client.next_record().await? {
    health.record_event();

    if let Some(sm) = rec.get::<databento::dbn::SymbolMappingMsg>() {
        if let Ok(osi) = sm.stype_out_symbol() {
            if let Some(&our_id) = sym_to_id.get(osi) {
                dbn_to_id.insert(sm.hd.instrument_id, our_id);
            }
        }
        continue;
    }

    // bbo-1s decodes to a BBO record; verify the exact struct name in your dbn version.
    if let Some(q) = rec.get::<databento::dbn::Bbo1SMsg>() {
        let level = &q.levels[0];               // top of book
        if let (Some(bid), Some(ask)) = (px(level.bid_px), px(level.ask_px)) {
            if let Some(&our_id) = dbn_to_id.get(&q.hd.instrument_id) {
                marks.set(our_id, bid, ask);
            }
        }
    }
}
```

Wrap 4b–4c in `connect_and_run()`; loop with backoff + `health.set_down()` on
error — copy `binance_stream.rs::run` almost verbatim.

> The one API detail to confirm in dbn 0.54: the record struct for `bbo-1s`
> (may be `BboMsg`/`Bbo1SMsg`) and that `.levels[0].bid_px/ask_px` are the fields.
> `Mbp1Msg` (`Schema::Mbp1`) is the certain fallback — same `levels[0]` shape,
> higher volume.

---

## Step 5 — Spawn it

In `src/main.rs`, next to the Binance spawn, guarded by the key:

```rust
if env::var("DATABENTO_API_KEY").is_ok() {
    let health = state.stream_health().handle("DATABENTO", "OPRA");
    tokio::spawn(opra_stream::run(state.pool().clone(), state.marks().clone(), health));
}
```

The cockpit stream-health strip then shows `DATABENTO/OPRA live` for free.

---

## Step 6 — Pick up newly-opened positions

**The problem.** The subscription is built once, at connect time, from what you
hold *right then* (step 4a). Buy an option an hour later and the stream has never
heard of it — no quotes, no mark, no unrealized P&L for that position, until you
restart the OMS.

**The fix.** Have the fill path ring a doorbell when it applies a fill; the stream
wakes, re-reads the held set, and subscribes the difference. No timer: the task
sleeps until an actual fill happens.

An **mpsc** is a queue between tasks: many `Sender`s (cloneable), one `Receiver`.
`rx.recv().await` parks the task until someone sends — that's what removes the
timer. The payload is `()`: a doorbell, not a letter. We don't send *which*
instrument, because the stream re-reads the held set from the DB anyway — sending
data would mean trusting the message; sending a nudge means trusting Postgres.

Known limitation: this only fires for fills **this process** handled. A position
opened by another node or by hand-written SQL won't nudge, and stays unmarked
until the next reconnect. (Fix later, if it matters, with a Postgres
`LISTEN/NOTIFY` trigger on `position` feeding the same channel — 6c doesn't change.)

### 6a. Ring the doorbell on a fill

`src/execution.rs` — one extra param, so no caller can forget it:

```rust
use tokio::sync::mpsc;

pub async fn process_execution_report(
    pool: &PgPool,
    kafka: &Option<KafkaClient>,
    order_id: Uuid,
    report: ExecutionReport,
    actor: &str,
    marks_nudge: Option<&mpsc::Sender<()>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    const MAX_ATTEMPTS: u32 = 5;
    for attempt in 1..=MAX_ATTEMPTS {
        match apply_once(pool, kafka, order_id, report.clone(), actor).await {
            Ok(()) => {
                // A fill moved the position — tell the marks feed to re-read the
                // held set. After apply_once, so the row is committed before the
                // stream can query it. try_send never blocks the fill path.
                if matches!(report, ExecutionReport::Fill { .. }) {
                    if let Some(tx) = marks_nudge {
                        let _ = tx.try_send(());
                    }
                }
                return Ok(());
            }
            /* … unchanged … */
        }
    }
    unreachable!()
}
```

Ordering matters: nudge **after** `apply_once` returns `Ok`, never before. It
commits the transaction, so a nudge sent earlier could wake the stream to a query
that can't see the new position yet.

### 6b. Thread the sender to the fill streams

`Sender` is `Clone`, so each stream gets its own. Four call sites, two per broker
(the live stream + the startup `reconcile_routed_orders` catch-up):

- `src/alpaca_stream.rs` — `run` → `connect_and_run` → `handle_trade_update`
  (`:288`), and `reconcile_routed_orders` (`:127`).
- `src/binance_stream.rs` — `run` → `connect_and_run` → `handle_execution_report`
  (`:194`), and `reconcile_routed_orders` (`:264`).

Each `run()` and the helpers take `marks_nudge: Option<&mpsc::Sender<()>>` (or an
owned `Option<mpsc::Sender<()>>` on `run`) and pass it straight through.
`Option` so a broker that runs without the marks feed configured just passes `None`.

`src/main.rs` — create the channel and wire both ends. `AppState` doesn't need it:
handlers submit orders, they don't apply fills.

```rust
// bounded(1): a queued nudge already means "reload", so extras are redundant.
let (marks_tx, marks_rx) = tokio::sync::mpsc::channel::<()>(1);

// … each fill-stream spawn gets `Some(marks_tx.clone())` …

if env::var("DATABENTO_API_KEY").map(|k| !k.is_empty()).unwrap_or(false) {
    let health = state.stream_health().handle("DATABENTO", "OPRA");
    tokio::spawn(opra_stream::run(state.pool().clone(), state.marks().clone(), health, marks_rx));
}
```

### 6c. Race the doorbell against the quote stream

The decode loop becomes a flat `loop` + `select!`. Two rules drive the shape:

- **`subscribe` is NOT cancel-safe** — the crate warns a dropped partial send makes
  the gateway reject it and close the connection. So it must never be a `select!`
  branch. Queue the symbols, subscribe at the top of the loop where nothing else
  borrows `client`. (This also settles the borrow checker: only `next_record`
  takes `&mut client`.)
- **`dbn_to_id` must live in `connect_and_run`**, not a helper fn. If it lived
  inside a future that `select!` can drop, losing that branch would take the
  session's id map with it.

```rust
// `rx` is owned by run() and borrowed into each session: the doorbell outlives
// reconnects, since it has nothing to do with the Databento session.
pub async fn run(pool: PgPool, marks: MarkStore, health: StreamHandle, mut rx: mpsc::Receiver<()>) {
    let mut backoff_secs: u64 = 1;
    loop {
        health.set_connecting();
        match connect_and_run(&pool, &marks, &health, &mut rx).await {
            /* … unchanged … */
        }
    }
}
```

```rust
    let mut sym_to_id = load_held_option_symbols(pool).await?;   // now `mut`
    // … build client, subscribe the initial set, start(), set_live() …

    let mut dbn_to_id: HashMap<u32, i64> = HashMap::new();
    let mut pending: Vec<String> = Vec::new();

    loop {
        // Outside the select: `subscribe` is not cancel-safe.
        if !pending.is_empty() {
            info!(count = pending.len(), "OPRA stream: subscribing newly-held options");
            client
                .subscribe(
                    Subscription::builder()
                        .symbols(std::mem::take(&mut pending))
                        .schema(Schema::Cmbp1)
                        .stype_in(SType::RawSymbol)
                        .build(),
                )
                .await?;
        }

        tokio::select! {
            // Doorbell rang: re-read the held set and diff it. Note we query the
            // DB rather than trust a payload — that's why the notify carries none.
            Some(()) = rx.recv() => {
                for (sym, id) in load_held_option_symbols(pool).await? {
                    if !sym_to_id.contains_key(&sym) {
                        pending.push(sym.clone());
                        sym_to_id.insert(sym, id);
                    }
                }
            }
            // Cancel-safe, so losing this branch just drops it mid-await.
            rec = client.next_record() => {
                let Some(rec) = rec? else { break };
                health.record_event();
                // … your existing SymbolMappingMsg + Cmbp1Msg branches, unchanged …
            }
        }
    }
    Ok(())
```

The gateway sends a fresh `SymbolMappingMsg` for each added symbol, so the
existing mapping branch picks them up with no changes — which is why step 4c
handles mappings *in* the loop rather than once at startup.

### Notes

- **No unsubscribe** in this API. A closed position keeps streaming until the next
  reconnect rebuilds the session. Harmless (a few wasted quotes), but it means
  `sym_to_id` only ever grows — don't read it as "currently held".
- `pending` survives loop iterations but not reconnects. Correct: a reconnect
  reloads the whole set from scratch anyway.
- Verify end-to-end: with the stream live, buy an option you don't already hold —
  the log should show `subscribing newly-held options`, then a mark appears for it
  in `/positions` without a restart. That restart-free part is the whole point of
  step 6.

---

## Step 7 — Valuation (the payoff)

In `get_portfolio_positions` (`src/handlers.rs`) — already returns `net_qty` +
`avg_cost`. Add `contract_size` to the query, join the mark in Rust:

```rust
let marks = state.marks().all();
for p in &mut rows {
    if let Some(m) = marks.get(&p.instrument_id) {
        let mid = m.mid();
        p.mark = Some(mid);
        p.market_value   = Some(p.net_qty * mid * p.contract_size);
        p.unrealized_pnl = Some((mid - p.avg_cost) * p.net_qty * p.contract_size);
    }
}
```

Signed-correct for shorts (`net_qty < 0`). `realized_pnl` already comes from
`positions.rs`. Add the three optional fields to the response struct + the TS
type, and render a cockpit **Positions** page (qty, avg cost, mark, MV, uPnL,
realized) using the same `useList` pattern as the blotter.

---

## Step 8 — Bonus: market-order risk

Market orders skip notional checks today (`est_price=None`,
`src/risk_engine.rs:101`). Now there's a price: in `orders_submit`, when
`limit_price` is None, look up `marks.get(instrument_id).map(|m| m.mid())` and
pass it as `est_price` into `check_submit`. Enforces notional caps on market
orders too.

---

## Step 9 — Verify

1. Open a small option position (limit order, RTH or resting).
2. Start the OMS with `DATABENTO_API_KEY` set → logs show `OPRA live` +
   `SymbolMappingMsg` then quotes.
3. Poke the store: log `marks.all().len()` or add a tiny `GET /admin/marks`
   debug endpoint.
4. Hit `/portfolios/:id/positions` → confirm `mark`, `market_value`,
   `unrealized_pnl` populate and move with the market.
5. Hand-check one: `(mid − avg_cost) × qty × 100`.

---

## Build order (smallest shippable steps)

1. `marks.rs` + `AppState` wiring (compiles, does nothing).
2. `opra_stream.rs` + spawn → marks populate (verify via debug log/endpoint).
3. Valuation join in `/positions` → uPnL shows.
4. Fill-path doorbell → newly-bought options get marked without a restart.
5. Cockpit Positions page.
6. Market-order est_price (bonus).

Steps 1–5 (marks store, AppState, `live` dep, stream task, spawn) are done;
step 6 above is the next one to build.

## Three things to watch

- Exact **dbn 0.54 record struct** for `bbo-1s` (verify with `cargo doc`).
- **Subscription scoping** — only held symbols; never subscribe all of OPRA.
- **Fixed-point price scale** — `/1e9`, guard `i64::MAX`.

Everything else reuses patterns already in the repo.
