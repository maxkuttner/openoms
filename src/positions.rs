//! Position projection (read model): net holding + cost basis + realized P&L per
//! (portfolio, instrument), maintained from fill events. Not part of the
//! event-sourced order aggregate. Unrealized P&L is intentionally absent — it needs
//! a market-data mark (later).

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::fees::FeeAmount;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::domain::orders::events::{OrderDomainEvent, OrderEventPayload};
use crate::domain::orders::state::OrderSide;
use crate::fees::FeeEffect;

/// The signed holding + average cost + realized P&L. `avg_cost` is the average price
/// of the *open* position (>= 0; 0 when flat). Pure value type — no DB.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PositionMath {
    pub net_qty: f64, // signed: + long, - short
    pub avg_cost: f64,
    pub realized_pnl: f64,
}

impl PositionMath {
    pub const FLAT: PositionMath = PositionMath { net_qty: 0.0, avg_cost: 0.0, realized_pnl: 0.0 };

    /// Apply one fill (`qty` > 0) using average-cost accounting.
    pub fn apply_fill(self, side: OrderSide, qty: f64, price: f64) -> PositionMath {
        let signed = match side {
            OrderSide::Buy => qty,
            OrderSide::Sell => -qty,
        };
        let (n, c) = (self.net_qty, self.avg_cost);

        // flat -> open at the fill price
        if n == 0.0 {
            return PositionMath { net_qty: signed, avg_cost: price, ..self };
        }
        // increasing (fill same sign as the position) -> blend the cost
        if (n > 0.0) == (signed > 0.0) {
            let new_abs = n.abs() + qty;
            return PositionMath {
                net_qty: n + signed,
                avg_cost: (n.abs() * c + qty * price) / new_abs,
                ..self
            };
        }
        // reducing / closing / flipping (fill opposite the position)
        let close = qty.min(n.abs());
        let sign_n = if n > 0.0 { 1.0 } else { -1.0 };
        let realized_pnl = self.realized_pnl + close * (price - c) * sign_n;
        if qty <= n.abs() {
            let net_qty = n + signed;
            PositionMath {
                net_qty,
                avg_cost: if net_qty == 0.0 { 0.0 } else { c }, // unchanged until flat
                realized_pnl,
            }
        } else {
            // flips through zero -> remainder opens a new position at the fill price
            let remainder = qty - n.abs();
            PositionMath {
                net_qty: if signed > 0.0 { remainder } else { -remainder },
                avg_cost: price,
                realized_pnl,
            }
        }
    }

    /// Apply a venue fee (charge > 0, rebate < 0). Quote: realized P&L. Base: the
    /// held quantity always drops by the amount, cost and realized unchanged (it may
    /// leave a residual or cross zero). Other: no position effect.
    pub fn apply_fee(self, amount: f64, effect: FeeEffect) -> PositionMath {
        match effect {
            FeeEffect::Quote => PositionMath { realized_pnl: self.realized_pnl - amount, ..self },
            FeeEffect::Base => PositionMath { net_qty: self.net_qty - amount, ..self },
            FeeEffect::Other => self,
        }
    }

    /// Reduce the open position by `qty` toward zero, preserving cost basis and
    /// realizing **no** P&L — the source side of an allocation transfer (a conduit).
    /// Caller guarantees `qty <= |net_qty|`.
    pub fn reduce_at_cost(self, qty: f64) -> PositionMath {
        let sign_n = if self.net_qty >= 0.0 { 1.0 } else { -1.0 };
        let net_qty = self.net_qty - sign_n * qty;
        PositionMath {
            net_qty,
            avg_cost: if net_qty == 0.0 { 0.0 } else { self.avg_cost },
            realized_pnl: self.realized_pnl,
        }
    }
}

/// Mark-to-market valuation of an open position.
///
/// Both figures are signed through `net_qty`, so a short (`net_qty < 0`) yields a
/// negative market value and profits when the mark falls below `avg_cost`.
/// `contract_size` scales into money — 100 for an option, 1 for spot.
pub fn value(net_qty: f64, avg_cost: f64, contract_size: f64, mid: f64) -> Valuation {
    Valuation {
        market_value: net_qty * mid * contract_size,
        unrealized_pnl: (mid - avg_cost) * net_qty * contract_size,
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Valuation {
    pub market_value: f64,
    pub unrealized_pnl: f64,
}

/// A position row as returned by the API.
///
/// `realized_pnl` comes from the projection; the mark-dependent fields are joined
/// in from the in-memory [`crate::marks::MarkStore`] at request time. They are
/// `None` when no live mark exists for the instrument — a position we can hold but
/// not value (no market-data source, or the feed is down) is a first-class state,
/// not an error, so it renders as a gap rather than a misleading zero.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct Position {
    pub portfolio_id: Uuid,
    pub instrument_id: String,
    /// Ticker of the instrument; None when the master row is missing.
    pub instrument_symbol: Option<String>,
    /// Display name of the instrument; None when the master row is missing.
    pub instrument_name: Option<String>,
    /// The instrument's own currency; every amount on this row is in it (no FX).
    pub currency: String,
    pub net_qty: f64,
    pub avg_cost: f64,
    pub realized_pnl: f64,
    pub updated_at: DateTime<Utc>,
    /// Mid of the current top-of-book quote.
    pub mark: Option<f64>,
    /// `net_qty * mark * contract_size` — signed, so shorts are negative.
    pub market_value: Option<f64>,
    /// `(mark - avg_cost) * net_qty * contract_size` — signed-correct for shorts.
    pub unrealized_pnl: Option<f64>,
    /// When the mark was received; lets a caller judge staleness.
    pub mark_ts: Option<DateTime<Utc>>,
    /// Fees in currencies that don't touch the position (e.g. BNB on a USDT pair),
    /// summed per currency. Empty when none.
    pub other_fees: Vec<FeeAmount>,
}

/// Read the current position (FOR UPDATE), or FLAT if none.
async fn load(
    conn: &mut PgConnection,
    portfolio_id: Uuid,
    instrument_id: &str,
) -> Result<PositionMath, sqlx::Error> {
    let row = sqlx::query(
        "SELECT net_qty::double precision AS net_qty, \
                avg_cost::double precision AS avg_cost, \
                realized_pnl::double precision AS realized_pnl \
         FROM position WHERE portfolio_id = $1 AND instrument_id = $2 FOR UPDATE",
    )
    .bind(portfolio_id)
    .bind(instrument_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(match row {
        Some(r) => PositionMath {
            net_qty: r.get("net_qty"),
            avg_cost: r.get("avg_cost"),
            realized_pnl: r.get("realized_pnl"),
        },
        None => PositionMath::FLAT,
    })
}

async fn upsert(
    conn: &mut PgConnection,
    portfolio_id: Uuid,
    instrument_id: &str,
    p: PositionMath,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO position \
            (portfolio_id, instrument_id, net_qty, avg_cost, realized_pnl, updated_at) \
         VALUES ($1, $2, $3, $4, $5, now()) \
         ON CONFLICT (portfolio_id, instrument_id) DO UPDATE SET \
            net_qty = EXCLUDED.net_qty, avg_cost = EXCLUDED.avg_cost, \
            realized_pnl = EXCLUDED.realized_pnl, updated_at = now()",
    )
    .bind(portfolio_id)
    .bind(instrument_id)
    .bind(p.net_qty)
    .bind(p.avg_cost)
    .bind(p.realized_pnl)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Canonical replay order of position inputs — the one place it is defined, used by
/// the per-position rebuild and the full rebuild. A fill's time is its trade time
/// (`occurred_at`, else booking time); a fee takes its fill's time; equal times
/// break on execution id (bytewise), a fee after its own fill, then on append order.
const CANONICAL_ORDER: &str = "trade_at, execution_id COLLATE \"C\", is_fee, global_position";

/// Trade time of the fill `exec` (an execution id expression) on order `order`, or
/// NULL when that fill isn't booked.
fn fill_time_sql(order: &str, exec: &str) -> String {
    format!(
        "(SELECT COALESCE(f.occurred_at, f.created_at) FROM order_event f \
          WHERE f.order_id = {order} AND f.event_type IN ('order_partially_filled', 'order_filled') \
            AND f.payload_json->'payload'->>'execution_id' = {exec} \
          ORDER BY f.global_position LIMIT 1)"
    )
}

/// Every input the incremental path applies — fills, fees and allocation
/// transfers — for one portfolio ($1, NULL = all) and instrument ($2, NULL = all),
/// in [`CANONICAL_ORDER`]. Transfers sit at their own time and touch both sides.
/// Reported fills (and their Quote/Base fees) join through their own pinned
/// portfolio (an admin rebook) or else their strategy's, at their trade time; like transfers they carry no global_position,
/// which sorts NULLS LAST and only breaks ties the earlier keys leave. A reported
/// row's execution id is `venue|account_ref|exec_id` (what ingest passes to
/// [`persist_fill`]), so one exec id reused on two venues still sorts one way.
fn replay_sql() -> String {
    let fee_fill_time = fill_time_sql("e.order_id", "e.payload_json->'payload'->>'execution_id'");
    format!(
        "SELECT * FROM ( \
           SELECT os.portfolio_id, os.instrument_id, os.side, e.event_type AS kind, e.payload_json, \
                  NULL::float8 AS qty, NULL::float8 AS price, \
                  CASE WHEN e.event_type = 'order_fee_charged' \
                       THEN COALESCE({fee_fill_time}, e.occurred_at, e.created_at) \
                       ELSE COALESCE(e.occurred_at, e.created_at) END AS trade_at, \
                  e.payload_json->'payload'->>'execution_id' AS execution_id, \
                  (e.event_type = 'order_fee_charged') AS is_fee, e.global_position \
           FROM order_event e JOIN order_state os ON os.order_id = e.order_id \
           WHERE e.event_type IN ('order_partially_filled', 'order_filled', 'order_fee_charged') \
             AND ($1::uuid IS NULL OR os.portfolio_id = $1) AND ($2::text IS NULL OR os.instrument_id = $2) \
           UNION ALL \
           SELECT a.from_portfolio_id, a.instrument_id, os.side, 'transfer_out', NULL, \
                  a.qty::float8, a.price::float8, a.created_at, NULL, false, NULL \
           FROM allocation a JOIN order_state os ON os.order_id = a.order_id \
           WHERE ($1::uuid IS NULL OR a.from_portfolio_id = $1) AND ($2::text IS NULL OR a.instrument_id = $2) \
           UNION ALL \
           SELECT a.to_portfolio_id, a.instrument_id, os.side, 'transfer_in', NULL, \
                  a.qty::float8, a.price::float8, a.created_at, NULL, false, NULL \
           FROM allocation a JOIN order_state os ON os.order_id = a.order_id \
           WHERE ($1::uuid IS NULL OR a.to_portfolio_id = $1) AND ($2::text IS NULL OR a.instrument_id = $2) \
           UNION ALL \
           SELECT COALESCE(rf.portfolio_id, s.portfolio_id), rf.instrument_id::text, lower(rf.side), 'reported_fill', NULL, \
                  rf.qty::float8, rf.price::float8, rf.trade_time, rf.venue || '|' || rf.account_ref || '|' || rf.exec_id, false, NULL \
           FROM reported_fill rf JOIN strategy s ON s.code = rf.strategy_code \
           WHERE ($1::uuid IS NULL OR COALESCE(rf.portfolio_id, s.portfolio_id) = $1) AND ($2::text IS NULL OR rf.instrument_id::text = $2) \
           UNION ALL \
           SELECT COALESCE(rf.portfolio_id, s.portfolio_id), rf.instrument_id::text, lower(rf.side), 'reported_fee', \
                  jsonb_build_object('effect', rf.fee_effect), \
                  rf.fee::float8, NULL, rf.trade_time, rf.venue || '|' || rf.account_ref || '|' || rf.exec_id, true, NULL \
           FROM reported_fill rf JOIN strategy s ON s.code = rf.strategy_code \
           WHERE rf.fee IS NOT NULL AND rf.fee_effect IN ('Quote', 'Base') \
             AND ($1::uuid IS NULL OR COALESCE(rf.portfolio_id, s.portfolio_id) = $1) AND ($2::text IS NULL OR rf.instrument_id::text = $2) \
         ) r ORDER BY {CANONICAL_ORDER}"
    )
}

/// An input's place in [`CANONICAL_ORDER`] short of the append order: a newcomer is
/// appended last, so it sorts before the position's last input exactly when its
/// key is smaller. Derived `Ord` matches the SQL: execution ids compare bytewise
/// (`COLLATE "C"`), a missing one sorts last (NULLS LAST), a fee after its fill.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct OrderKey {
    at: DateTime<Utc>,
    exec: ExecKey,
    is_fee: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ExecKey {
    Id(String),
    Missing,
}

impl OrderKey {
    fn new(at: DateTime<Utc>, exec: Option<&str>, is_fee: bool) -> Self {
        let exec = exec.map_or(ExecKey::Missing, |e| ExecKey::Id(e.to_string()));
        OrderKey { at, exec, is_fee }
    }

    fn exec(&self) -> Option<&str> {
        match &self.exec {
            ExecKey::Id(e) => Some(e),
            ExecKey::Missing => None,
        }
    }
}

/// A replayed position: the math, its last ordered input, and how many fills.
#[derive(Default)]
struct Replayed {
    math: Option<PositionMath>,
    /// Last ordered input (fill, base fee, transfer): what a newcomer must not precede.
    last: Option<OrderKey>,
    fills: usize,
}

/// Note an ordered input; rows come in canonical order, so the latest note is the last.
fn bump(acc: &mut HashMap<(Uuid, String), Replayed>, key: &(Uuid, String), at: OrderKey) {
    acc.entry(key.clone()).or_default().last = Some(at);
}

/// Fold canonical-ordered replay rows into one position per (portfolio, instrument).
fn replay(rows: &[sqlx::postgres::PgRow]) -> HashMap<(Uuid, String), Replayed> {
    let mut acc: HashMap<(Uuid, String), Replayed> = HashMap::new();
    for r in rows {
        let key = (r.get::<Uuid, _>("portfolio_id"), r.get::<String, _>("instrument_id"));
        let side = match r.get::<String, _>("side").as_str() {
            "buy" => OrderSide::Buy,
            _ => OrderSide::Sell,
        };
        let trade_at = OrderKey::new(
            r.get("trade_at"),
            r.get::<Option<String>, _>("execution_id").as_deref(),
            r.get("is_fee"),
        );
        let step: Box<dyn Fn(PositionMath) -> PositionMath> = match r.get::<String, _>("kind").as_str() {
            "transfer_out" => {
                let qty: f64 = r.get("qty");
                bump(&mut acc, &key, trade_at.clone());
                Box::new(move |p| p.reduce_at_cost(qty))
            }
            "reported_fill" => {
                let (qty, price): (f64, f64) = (r.get("qty"), r.get("price"));
                acc.entry(key.clone()).or_default().fills += 1;
                bump(&mut acc, &key, trade_at.clone());
                Box::new(move |p| p.apply_fill(side, qty, price))
            }
            "reported_fee" => {
                let amount: f64 = r.get("qty");
                let payload: serde_json::Value = r.get("payload_json");
                let effect = match payload["effect"].as_str() {
                    Some("Quote") => FeeEffect::Quote,
                    Some("Base") => FeeEffect::Base,
                    _ => continue,
                };
                if effect == FeeEffect::Base {
                    bump(&mut acc, &key, trade_at.clone());
                }
                Box::new(move |p| p.apply_fee(amount, effect))
            }
            "transfer_in" => {
                let (qty, price): (f64, f64) = (r.get("qty"), r.get("price"));
                bump(&mut acc, &key, trade_at.clone());
                Box::new(move |p| p.apply_fill(side, qty, price))
            }
            _ => {
                let payload: serde_json::Value = r.get("payload_json");
                let Ok(event) = serde_json::from_value::<OrderDomainEvent>(payload) else {
                    continue;
                };
                match event.payload {
                    OrderEventPayload::OrderPartiallyFilled { fill_qty, fill_price, .. }
                    | OrderEventPayload::OrderFilled { fill_qty, fill_price, .. } => {
                        acc.entry(key.clone()).or_default().fills += 1;
                        bump(&mut acc, &key, trade_at.clone());
                        Box::new(move |p| p.apply_fill(side, fill_qty, fill_price))
                    }
                    OrderEventPayload::OrderFeeCharged { amount, effect, .. } if effect != FeeEffect::Other => {
                        // a quote fee commutes, so only a base fee holds a place in the order
                        if effect == FeeEffect::Base {
                            bump(&mut acc, &key, trade_at.clone());
                        }
                        Box::new(move |p| p.apply_fee(amount, effect))
                    }
                    _ => continue,
                }
            }
        };
        let entry = acc.entry(key).or_default();
        entry.math = Some(step(entry.math.unwrap_or(PositionMath::FLAT)));
    }
    acc
}

/// Serialize every writer of one (portfolio, instrument) position for the rest of
/// the transaction, also when its row doesn't exist yet (FOR UPDATE can't lock that).
async fn lock_position(conn: &mut PgConnection, portfolio_id: Uuid, instrument_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text || '/' || $2, 0))")
        .bind(portfolio_id)
        .bind(instrument_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Lock every (portfolio, instrument) position in `portfolios` in one global
/// (sorted) order, so transactions touching overlapping sets can't deadlock.
/// Later per-position locks in the same transaction are re-entrant no-ops.
pub async fn lock_positions(conn: &mut PgConnection, portfolios: &[Uuid], instrument_id: &str) -> Result<(), sqlx::Error> {
    let mut sorted = portfolios.to_vec();
    sorted.sort();
    sorted.dedup();
    for portfolio in sorted {
        lock_position(conn, portfolio, instrument_id).await?;
    }
    Ok(())
}

/// Lock the position ([`lock_position`]) and read its last ordered input's key
/// (None when absent or legacy).
async fn lock_last_key(
    conn: &mut PgConnection,
    portfolio_id: Uuid,
    instrument_id: &str,
) -> Result<Option<OrderKey>, sqlx::Error> {
    lock_position(conn, portfolio_id, instrument_id).await?;
    let row: Option<(Option<DateTime<Utc>>, Option<String>, Option<bool>)> = sqlx::query_as(
        "SELECT last_trade_at, last_trade_exec, last_trade_is_fee FROM position \
         WHERE portfolio_id = $1 AND instrument_id = $2 FOR UPDATE",
    )
    .bind(portfolio_id)
    .bind(instrument_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.and_then(|(at, exec, is_fee)| Some(OrderKey::new(at?, exec.as_deref(), is_fee.unwrap_or(false)))))
}

/// Record the position's last ordered input (None clears it).
async fn set_last_key(
    conn: &mut PgConnection,
    portfolio_id: Uuid,
    instrument_id: &str,
    key: Option<&OrderKey>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE position SET last_trade_at = $3, last_trade_exec = $4, last_trade_is_fee = $5 \
         WHERE portfolio_id = $1 AND instrument_id = $2",
    )
    .bind(portfolio_id)
    .bind(instrument_id)
    .bind(key.map(|k| k.at))
    .bind(key.and_then(|k| k.exec()))
    .bind(key.map(|k| k.is_fee))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// True when `key` sorts before the position's last input: it must be replayed.
fn is_late(last: Option<&OrderKey>, key: &OrderKey) -> bool {
    last.is_some_and(|l| key < l)
}

/// Rebuild one (portfolio, instrument) position from its booked inputs in
/// canonical order, inside the caller's transaction. Writes only the position row.
/// Returns how many fills it replayed.
pub async fn rebuild_position(
    conn: &mut PgConnection,
    portfolio_id: Uuid,
    instrument_id: &str,
) -> Result<usize, sqlx::Error> {
    let started = std::time::Instant::now();
    let rows = sqlx::query(&replay_sql())
        .bind(portfolio_id)
        .bind(instrument_id)
        .fetch_all(&mut *conn)
        .await?;
    let replayed = replay(&rows).remove(&(portfolio_id, instrument_id.to_string())).unwrap_or_default();
    upsert(conn, portfolio_id, instrument_id, replayed.math.unwrap_or(PositionMath::FLAT)).await?;
    set_last_key(conn, portfolio_id, instrument_id, replayed.last.as_ref()).await?;
    tracing::info!(
        %portfolio_id,
        instrument_id,
        fills = replayed.fills,
        duration_ms = started.elapsed().as_secs_f64() * 1000.0,
        "position rebuilt in trade-time order"
    );
    Ok(replayed.fills)
}

/// Apply one fill to its position inside the caller's transaction (so it commits
/// atomically with the order_state update + event append). The caller appends the
/// fill's event first. A fill whose canonical key `(trade_time, execution_id)`
/// sorts before the position's last input rebuilds the position in canonical
/// order; one that sorts after it applies incrementally.
/// Returns true when it rebuilt: the rebuild already includes every appended
/// event, so the caller applies no further fills of the same append.
#[allow(clippy::too_many_arguments)]
pub async fn persist_fill(
    conn: &mut PgConnection,
    portfolio_id: Uuid,
    instrument_id: &str,
    side: OrderSide,
    fill_qty: f64,
    fill_price: f64,
    trade_time: DateTime<Utc>,
    execution_id: &str,
) -> Result<bool, sqlx::Error> {
    let key = OrderKey::new(trade_time, Some(execution_id), false);
    if is_late(lock_last_key(conn, portfolio_id, instrument_id).await?.as_ref(), &key) {
        rebuild_position(conn, portfolio_id, instrument_id).await?;
        return Ok(true);
    }
    let next = load(conn, portfolio_id, instrument_id).await?.apply_fill(side, fill_qty, fill_price);
    upsert(conn, portfolio_id, instrument_id, next).await?;
    set_last_key(conn, portfolio_id, instrument_id, Some(&key)).await?;
    Ok(false)
}

/// Apply one venue fee to its position inside the caller's transaction; the caller
/// appends the fee's event first. An `Other` fee has no position effect and writes
/// nothing. A quote fee only moves realized P&L, so its place in the order doesn't
/// matter. A base fee changes quantity: it sits at its fill's key, and one that
/// sorts before the position's last input rebuilds the position. Returns true when
/// it rebuilt.
#[allow(clippy::too_many_arguments)]
pub async fn persist_fee(
    conn: &mut PgConnection,
    portfolio_id: Uuid,
    instrument_id: &str,
    order_id: Uuid,
    execution_id: Option<&str>,
    amount: f64,
    effect: FeeEffect,
) -> Result<bool, sqlx::Error> {
    if effect == FeeEffect::Other {
        return Ok(false);
    }
    let last = lock_last_key(conn, portfolio_id, instrument_id).await?;
    let mut fee_key = None;
    if effect == FeeEffect::Base {
        // its fill's trade time; a fee ahead of its fill sits at its own booking time
        let at: DateTime<Utc> = sqlx::query_scalar(&format!("SELECT COALESCE({}, now())", fill_time_sql("$1", "$2")))
            .bind(order_id)
            .bind(execution_id)
            .fetch_one(&mut *conn)
            .await?;
        let key = OrderKey::new(at, execution_id, true);
        if is_late(last.as_ref(), &key) {
            rebuild_position(conn, portfolio_id, instrument_id).await?;
            return Ok(true);
        }
        fee_key = Some(key);
    }
    let next = load(conn, portfolio_id, instrument_id).await?.apply_fee(amount, effect);
    upsert(conn, portfolio_id, instrument_id, next).await?;
    // a quote fee holds no place in the order: the last key stays
    if let Some(key) = fee_key {
        set_last_key(conn, portfolio_id, instrument_id, Some(&key)).await?;
    }
    Ok(false)
}

/// [`persist_fee`] for a reported fill's fee (no order): the caller inserts the
/// `reported_fill` row first. A base fee sits at its fill's key
/// `(trade_time, exec_id)` and rebuilds when late; a quote fee commutes; `Other`
/// writes nothing. Returns true when it rebuilt.
#[allow(clippy::too_many_arguments)]
pub async fn persist_reported_fee(
    conn: &mut PgConnection,
    portfolio_id: Uuid,
    instrument_id: &str,
    exec_id: &str,
    trade_time: DateTime<Utc>,
    amount: f64,
    effect: FeeEffect,
) -> Result<bool, sqlx::Error> {
    if effect == FeeEffect::Other {
        return Ok(false);
    }
    let last = lock_last_key(conn, portfolio_id, instrument_id).await?;
    let fee_key = (effect == FeeEffect::Base).then(|| OrderKey::new(trade_time, Some(exec_id), true));
    if fee_key.as_ref().is_some_and(|k| is_late(last.as_ref(), k)) {
        rebuild_position(conn, portfolio_id, instrument_id).await?;
        return Ok(true);
    }
    let next = load(conn, portfolio_id, instrument_id).await?.apply_fee(amount, effect);
    upsert(conn, portfolio_id, instrument_id, next).await?;
    if let Some(key) = fee_key {
        set_last_key(conn, portfolio_id, instrument_id, Some(&key)).await?;
    }
    Ok(false)
}

/// Cost-preserving allocation transfer of `qty` (> 0) of `instrument_id` from one
/// portfolio to another at `price`, inside the caller's tx. The source reduces at
/// cost (no P&L — a conduit); the destination books at `price` via the normal fill
/// logic. Caller validates `qty <= |source net_qty|` and inserts the `allocation`
/// row first: the transfer sits at its time (`now()`, the row's `created_at`), and
/// a side whose latest input isn't older rebuilds from the log instead.
pub async fn persist_transfer(
    conn: &mut PgConnection,
    from_portfolio: Uuid,
    to_portfolio: Uuid,
    instrument_id: &str,
    side: OrderSide,
    qty: f64,
    price: f64,
) -> Result<(), sqlx::Error> {
    // both sides locked in one order (a caller with several transfers in one tx
    // locks them all up front with lock_positions; these then re-enter)
    lock_positions(conn, &[from_portfolio, to_portfolio], instrument_id).await?;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT now()").fetch_one(&mut *conn).await?;
    let key = OrderKey::new(now, None, false);
    for (portfolio, out) in [(from_portfolio, true), (to_portfolio, false)] {
        if is_late(lock_last_key(conn, portfolio, instrument_id).await?.as_ref(), &key) {
            rebuild_position(conn, portfolio, instrument_id).await?;
            continue;
        }
        let p = load(conn, portfolio, instrument_id).await?;
        let next = if out { p.reduce_at_cost(qty) } else { p.apply_fill(side, qty, price) };
        upsert(conn, portfolio, instrument_id, next).await?;
        set_last_key(conn, portfolio, instrument_id, Some(&key)).await?;
    }
    Ok(())
}

/// Recovery / backfill: rebuild every position by replaying fills, fees and
/// allocation transfers in canonical (trade-time) order — the same order and inputs
/// as the late-fill rebuild. Idempotent (derived data).
pub async fn rebuild_positions(pool: &PgPool) -> Result<usize, sqlx::Error> {
    rebuild_positions_scoped(pool, None).await
}

/// Boot backfill: when the position projection is empty, rebuild it from the event
/// log (Some(rows written)); otherwise do nothing (None). Boot calls it before
/// anything that can book a fill starts. Migration 0067 empties the table so every
/// row gets its trade-time key here.
pub async fn rebuild_if_empty(pool: &PgPool) -> Result<Option<usize>, sqlx::Error> {
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM position").fetch_one(pool).await?;
    if n > 0 {
        return Ok(None);
    }
    rebuild_positions(pool).await.map(Some)
}

/// [`rebuild_positions`] limited to one portfolio (`Some`), leaving every other
/// portfolio's positions untouched; `None` rebuilds everything.
///
/// Safe against concurrent booking: it takes the incremental path's per-position
/// advisory lock for every position it rewrites (existing rows plus every key with
/// inputs), in one (instrument, portfolio) order, then reads the inputs inside the
/// transaction. A position that first appears after the key list was taken is
/// neither locked nor touched: its own writer maintains it.
pub async fn rebuild_positions_scoped(pool: &PgPool, portfolio: Option<Uuid>) -> Result<usize, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let mut keys: Vec<(String, Uuid)> = sqlx::query_as(
        "SELECT instrument_id, portfolio_id FROM position WHERE ($1::uuid IS NULL OR portfolio_id = $1) \
         UNION SELECT os.instrument_id, os.portfolio_id FROM order_state os \
           WHERE ($1::uuid IS NULL OR os.portfolio_id = $1) \
             AND EXISTS (SELECT 1 FROM order_event e WHERE e.order_id = os.order_id \
                         AND e.event_type IN ('order_partially_filled', 'order_filled', 'order_fee_charged')) \
         UNION SELECT instrument_id, from_portfolio_id FROM allocation WHERE ($1::uuid IS NULL OR from_portfolio_id = $1) \
         UNION SELECT instrument_id, to_portfolio_id FROM allocation WHERE ($1::uuid IS NULL OR to_portfolio_id = $1) \
         UNION SELECT rf.instrument_id::text, COALESCE(rf.portfolio_id, s.portfolio_id) FROM reported_fill rf JOIN strategy s ON s.code = rf.strategy_code \
           WHERE ($1::uuid IS NULL OR COALESCE(rf.portfolio_id, s.portfolio_id) = $1)",
    )
    .bind(portfolio)
    .fetch_all(&mut *tx)
    .await?;
    keys.sort();
    for (instrument_id, portfolio_id) in &keys {
        lock_position(&mut tx, *portfolio_id, instrument_id).await?;
    }
    let rows = sqlx::query(&replay_sql())
        .bind(portfolio)
        .bind(None::<String>)
        .fetch_all(&mut *tx)
        .await?;
    let acc = replay(&rows);

    let (instruments, portfolios): (Vec<String>, Vec<Uuid>) = keys.iter().cloned().unzip();
    sqlx::query(
        "DELETE FROM position p USING unnest($1::text[], $2::uuid[]) k(instrument_id, portfolio_id) \
         WHERE p.instrument_id = k.instrument_id AND p.portfolio_id = k.portfolio_id",
    )
    .bind(&instruments)
    .bind(&portfolios)
    .execute(&mut *tx)
    .await?;
    let locked: std::collections::HashSet<(Uuid, &str)> = keys.iter().map(|(i, p)| (*p, i.as_str())).collect();
    let mut written = 0;
    for ((portfolio_id, instrument_id), r) in &acc {
        if !locked.contains(&(*portfolio_id, instrument_id.as_str())) {
            continue;
        }
        let Some(p) = r.math else { continue };
        sqlx::query(
            "INSERT INTO position \
                (portfolio_id, instrument_id, net_qty, avg_cost, realized_pnl, \
                 last_trade_at, last_trade_exec, last_trade_is_fee) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(portfolio_id)
        .bind(instrument_id)
        .bind(p.net_qty)
        .bind(p.avg_cost)
        .bind(p.realized_pnl)
        .bind(r.last.as_ref().map(|k| k.at))
        .bind(r.last.as_ref().and_then(|k| k.exec()))
        .bind(r.last.as_ref().map(|k| k.is_fee))
        .execute(&mut *tx)
        .await?;
        written += 1;
    }
    tx.commit().await?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "expected {b}, got {a}");
    }

    #[test]
    fn position_serializes_instrument_currency() {
        let p = Position {
            portfolio_id: Uuid::nil(),
            instrument_id: "42".into(),
            instrument_symbol: None,
            instrument_name: None,
            currency: "EUR".into(),
            net_qty: 1.0,
            avg_cost: 2.0,
            realized_pnl: 0.0,
            updated_at: Utc::now(),
            mark: None,
            market_value: None,
            unrealized_pnl: None,
            mark_ts: None,
            other_fees: vec![],
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["currency"], "EUR");
    }

    #[test] fn quote_fee_reduces_realized() { let p = PositionMath { net_qty: 1.0, avg_cost: 100.0, realized_pnl: 5.0 }.apply_fee(1.5, FeeEffect::Quote); approx(p.realized_pnl, 3.5); approx(p.net_qty, 1.0); }
    #[test] fn rebate_increases_realized() { approx(PositionMath::FLAT.apply_fee(-0.2, FeeEffect::Quote).realized_pnl, 0.2); }
    #[test] fn base_fee_shrinks_long_at_cost() { let p = PositionMath { net_qty: 1.0, avg_cost: 64000.0, realized_pnl: 0.0 }.apply_fee(0.001, FeeEffect::Base); approx(p.net_qty, 0.999); approx(p.avg_cost, 64000.0); approx(p.realized_pnl, 0.0); }
    #[test] fn base_fee_on_short_grows_the_short() { approx(PositionMath { net_qty: -2.0, avg_cost: 10.0, realized_pnl: 0.0 }.apply_fee(0.5, FeeEffect::Base).net_qty, -2.5); }
    #[test] fn base_fee_when_flat_leaves_a_residual() { let p = PositionMath::FLAT.apply_fee(0.001, FeeEffect::Base); approx(p.net_qty, -0.001); approx(p.avg_cost, 0.0); }
    #[test] fn base_rebate_increases_quantity() { approx(PositionMath { net_qty: 1.0, avg_cost: 10.0, realized_pnl: 0.0 }.apply_fee(-0.001, FeeEffect::Base).net_qty, 1.001); }
    #[test] fn base_fee_may_cross_zero() { let p = PositionMath { net_qty: 0.0005, avg_cost: 10.0, realized_pnl: 0.0 }.apply_fee(0.001, FeeEffect::Base); approx(p.net_qty, -0.0005); approx(p.avg_cost, 10.0); }
    #[test] fn other_fee_changes_nothing() { let p = PositionMath { net_qty: 1.0, avg_cost: 1.0, realized_pnl: 1.0 }; assert_eq!(p.apply_fee(3.0, FeeEffect::Other), p); }

    #[test]
    fn open_long() {
        let p = PositionMath::FLAT.apply_fill(OrderSide::Buy, 100.0, 10.0);
        approx(p.net_qty, 100.0);
        approx(p.avg_cost, 10.0);
        approx(p.realized_pnl, 0.0);
    }

    #[test]
    fn value_long_option_scales_by_contract_size() {
        // 1 contract, paid 2.00, now marked 3.28 -> +1.28 x 100.
        let v = value(1.0, 2.00, 100.0, 3.28);
        approx(v.market_value, 328.0);
        approx(v.unrealized_pnl, 128.0);
    }

    #[test]
    fn value_short_is_signed_correct() {
        // Short 1 contract at 3.00; mark falls to 2.00 -> the short PROFITS.
        let v = value(-1.0, 3.00, 100.0, 2.00);
        approx(v.market_value, -200.0); // liability
        approx(v.unrealized_pnl, 100.0); // gain
    }

    #[test]
    fn value_short_loses_when_mark_rises() {
        let v = value(-1.0, 3.00, 100.0, 4.00);
        approx(v.unrealized_pnl, -100.0);
    }

    #[test]
    fn value_spot_uses_unit_contract_size() {
        // 0.1 SOL bought at 100, marked 150.
        let v = value(0.1, 100.0, 1.0, 150.0);
        approx(v.market_value, 15.0);
        approx(v.unrealized_pnl, 5.0);
    }

    #[test]
    fn value_at_cost_is_flat() {
        let v = value(10.0, 42.0, 1.0, 42.0);
        approx(v.unrealized_pnl, 0.0);
    }

    #[test]
    fn add_to_long_blends_cost() {
        let p = PositionMath::FLAT
            .apply_fill(OrderSide::Buy, 100.0, 10.0)
            .apply_fill(OrderSide::Buy, 100.0, 12.0);
        approx(p.net_qty, 200.0);
        approx(p.avg_cost, 11.0); // (100*10 + 100*12)/200
        approx(p.realized_pnl, 0.0);
    }

    #[test]
    fn partial_close_realizes_pnl_keeps_cost() {
        let p = PositionMath::FLAT
            .apply_fill(OrderSide::Buy, 100.0, 10.0)
            .apply_fill(OrderSide::Sell, 40.0, 15.0);
        approx(p.net_qty, 60.0);
        approx(p.avg_cost, 10.0); // unchanged on reduce
        approx(p.realized_pnl, 200.0); // 40 * (15 - 10)
    }

    #[test]
    fn full_close_goes_flat() {
        let p = PositionMath::FLAT
            .apply_fill(OrderSide::Buy, 100.0, 10.0)
            .apply_fill(OrderSide::Sell, 100.0, 9.0);
        approx(p.net_qty, 0.0);
        approx(p.avg_cost, 0.0);
        approx(p.realized_pnl, -100.0); // 100 * (9 - 10)
    }

    #[test]
    fn flip_long_to_short_opens_remainder_at_fill() {
        let p = PositionMath::FLAT
            .apply_fill(OrderSide::Buy, 100.0, 10.0)
            .apply_fill(OrderSide::Sell, 150.0, 12.0);
        approx(p.net_qty, -50.0); // flipped short
        approx(p.avg_cost, 12.0); // remainder at fill price
        approx(p.realized_pnl, 200.0); // closed 100 * (12 - 10)
    }

    #[test]
    fn open_short_then_cover_for_profit() {
        let p = PositionMath::FLAT
            .apply_fill(OrderSide::Sell, 100.0, 20.0)
            .apply_fill(OrderSide::Buy, 100.0, 18.0);
        approx(p.net_qty, 0.0);
        approx(p.avg_cost, 0.0);
        approx(p.realized_pnl, 200.0); // short: 100 * (18 - 20) * sign(-1) = +200
    }

    #[test]
    fn reduce_at_cost_preserves_cost_and_pnl() {
        let long = PositionMath { net_qty: 100.0, avg_cost: 10.0, realized_pnl: 5.0 };
        let r = long.reduce_at_cost(40.0);
        approx(r.net_qty, 60.0);
        approx(r.avg_cost, 10.0); // unchanged
        approx(r.realized_pnl, 5.0); // unchanged — conduit, no P&L
    }

    #[test]
    fn reduce_short_and_to_flat() {
        let short = PositionMath { net_qty: -100.0, avg_cost: 20.0, realized_pnl: 0.0 };
        approx(short.reduce_at_cost(30.0).net_qty, -70.0);
        let flat = short.reduce_at_cost(100.0);
        approx(flat.net_qty, 0.0);
        approx(flat.avg_cost, 0.0); // cost cleared when flat
    }

    #[test]
    fn transfer_round_trip_buy_block() {
        // Staging long 1000 @ 10; allocate 600 to a flat fund at the block price.
        let staging = PositionMath { net_qty: 1000.0, avg_cost: 10.0, realized_pnl: 0.0 };
        let fund = PositionMath::FLAT;
        let staging_after = staging.reduce_at_cost(600.0);
        let fund_after = fund.apply_fill(OrderSide::Buy, 600.0, 10.0);
        approx(staging_after.net_qty, 400.0);
        approx(staging_after.realized_pnl, 0.0); // no P&L on the conduit
        approx(fund_after.net_qty, 600.0);
        approx(fund_after.avg_cost, 10.0); // booked at the block price
        approx(fund_after.realized_pnl, 0.0); // adding -> no P&L
    }

    /// Trade-time ordering against a live DB. Each test seeds its own portfolio.
    mod db {
        use super::super::*;
        use chrono::Duration;
        use crate::domain::orders::events::OrderEventType;
        use crate::domain::orders::state::OrderStatus;
        use crate::event_store::{NewOrderEvent, OrderEventStore};
        use crate::execution::tests::{remove_seeded, seeded_order};
        use crate::handlers::tests::test_pool;

        const EPS: f64 = 1e-9;

        /// One portfolio/instrument with a buy order and a sell order on it.
        struct Book { buy: Uuid, sell: Uuid, portfolio: Uuid, instrument: String }

        async fn book(pool: &PgPool) -> Book {
            let (buy, portfolio, instrument) = seeded_order(pool, 0).await;
            sqlx::query("UPDATE order_state SET side = 'buy' WHERE order_id = $1").bind(buy).execute(pool).await.unwrap();
            let sell = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO order_state SELECT (jsonb_populate_record(NULL::order_state, to_jsonb(os) || \
                 jsonb_build_object('order_id', $2::uuid, 'side', 'sell', 'client_order_id', $2::text))).* \
                 FROM order_state os WHERE order_id = $1",
            )
            .bind(buy).bind(sell).execute(pool).await.expect("seed sell order");
            Book { buy, sell, portfolio, instrument }
        }

        async fn drop_book(pool: &PgPool, b: &Book) {
            for sql in [
                "DELETE FROM allocation WHERE order_id = ANY($1)",
                "DELETE FROM kafka_outbox WHERE global_position IN (SELECT global_position FROM order_event WHERE order_id = ANY($1))",
            ] {
                sqlx::query(sql).bind(vec![b.buy, b.sell]).execute(pool).await.expect(sql);
            }
            sqlx::query("DELETE FROM order_state WHERE order_id = $1").bind(b.sell).execute(pool).await.unwrap();
            remove_seeded(pool, b.buy).await;
        }

        async fn append(
            pool: &PgPool,
            tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
            order_id: Uuid,
            event_type: OrderEventType,
            payload: OrderEventPayload,
            at: Option<DateTime<Utc>>,
        ) {
            let version: i64 = sqlx::query_scalar("SELECT version FROM order_state WHERE order_id = $1 FOR UPDATE")
                .bind(order_id).fetch_one(&mut **tx).await.unwrap();
            let event = OrderDomainEvent {
                event_id: Uuid::new_v4().to_string(),
                event_type,
                order_id: order_id.to_string(),
                timestamp: Utc::now(),
                actor: "test".into(),
                occurred_at: at,
                payload,
                version: version + 1,
                status_after: OrderStatus::PartiallyFilled,
            };
            let new = NewOrderEvent {
                event_id: event.event_id.clone(),
                event_type: event.event_type.as_str().to_string(),
                actor: "test".into(),
                payload: serde_json::to_value(&event).unwrap(),
                correlation_id: None,
                causation_id: None,
                schema_version: 0,
                occurred_at: at,
            };
            OrderEventStore::new(pool.clone()).append_events_in_tx(tx, order_id, version, &[new]).await.expect("append");
            sqlx::query("UPDATE order_state SET version = version + 1 WHERE order_id = $1")
                .bind(order_id).execute(&mut **tx).await.unwrap();
        }

        /// Book one fill the way the callers do: append the event, then the position.
        async fn fill(pool: &PgPool, b: &Book, side: OrderSide, exec: &str, qty: f64, price: f64, at: DateTime<Utc>) -> bool {
            let order = if side == OrderSide::Buy { b.buy } else { b.sell };
            let mut tx = pool.begin().await.unwrap();
            let payload = OrderEventPayload::OrderPartiallyFilled {
                execution_id: exec.into(), fill_qty: qty, fill_price: price, cum_qty: qty, leaves_qty: 0.0, avg_px: price, venue: "TEST".into(),
            };
            append(pool, &mut tx, order, OrderEventType::OrderPartiallyFilled, payload, Some(at)).await;
            let rebuilt = persist_fill(&mut tx, b.portfolio, &b.instrument, side, qty, price, at, exec).await.expect("persist fill");
            tx.commit().await.unwrap();
            rebuilt
        }

        /// Book one fee on `exec`'s fill (event first, then the position).
        async fn fee(pool: &PgPool, b: &Book, side: OrderSide, exec: &str, amount: f64, effect: FeeEffect) -> bool {
            let order = if side == OrderSide::Buy { b.buy } else { b.sell };
            let mut tx = pool.begin().await.unwrap();
            let payload = OrderEventPayload::OrderFeeCharged {
                fee_id: Uuid::new_v4().to_string(), execution_id: Some(exec.into()), amount, currency: "X".into(), effect,
            };
            append(pool, &mut tx, order, OrderEventType::OrderFeeCharged, payload, None).await;
            let rebuilt = persist_fee(&mut tx, b.portfolio, &b.instrument, order, Some(exec), amount, effect).await.expect("persist fee");
            tx.commit().await.unwrap();
            rebuilt
        }

        async fn pos(pool: &PgPool, portfolio: Uuid, instrument: &str) -> (f64, f64, f64) {
            sqlx::query_as(
                "SELECT net_qty::float8, avg_cost::float8, realized_pnl::float8 FROM position \
                 WHERE portfolio_id = $1 AND instrument_id = $2")
                .bind(portfolio).bind(instrument).fetch_optional(pool).await.unwrap().unwrap_or((0.0, 0.0, 0.0))
        }

        fn close(got: (f64, f64, f64), want: PositionMath, ctx: &str) {
            let ok = (got.0 - want.net_qty).abs() < EPS
                && (got.1 - want.avg_cost).abs() < EPS
                && (got.2 - want.realized_pnl).abs() < EPS;
            assert!(ok, "{ctx}: got {got:?}, want {want:?}");
        }

        fn t(secs: i64) -> DateTime<Utc> {
            DateTime::parse_from_rfc3339("2026-11-01T00:00:00Z").unwrap().with_timezone(&Utc) + Duration::seconds(secs)
        }

        fn permutations(n: usize) -> Vec<Vec<usize>> {
            if n == 0 {
                return vec![vec![]];
            }
            let mut out = Vec::new();
            for p in permutations(n - 1) {
                for i in 0..=p.len() {
                    let mut q = p.clone();
                    q.insert(i, n - 1);
                    out.push(q);
                }
            }
            out
        }

        #[derive(Clone, Copy, Debug)]
        enum Input {
            Fill(usize),
            Fee(usize),
            Transfer,
        }

        /// Record an allocation of `qty` from the block to `fund` at `price` and
        /// transfer it (row first, then the positions), as create_allocations does.
        async fn allocate(pool: &PgPool, b: &Book, fund: Uuid, qty: f64, price: f64) {
            let mut tx = pool.begin().await.unwrap();
            sqlx::query(
                "INSERT INTO allocation (order_id, from_portfolio_id, to_portfolio_id, instrument_id, qty, price) \
                 VALUES ($1, $2, $3, $4, $5, $6)")
                .bind(b.buy).bind(b.portfolio).bind(fund).bind(&b.instrument).bind(qty).bind(price)
                .execute(&mut *tx).await.unwrap();
            persist_transfer(&mut tx, b.portfolio, fund, &b.instrument, OrderSide::Buy, qty, price).await.unwrap();
            tx.commit().await.unwrap();
        }

        async fn new_fund(pool: &PgPool) -> Uuid {
            new_fund_with(pool, Uuid::new_v4()).await
        }

        async fn new_fund_with(pool: &PgPool, fund: Uuid) -> Uuid {
            sqlx::query("INSERT INTO portfolio (id, code, name, status) VALUES ($1, $2, $2, 'ACTIVE')")
                .bind(fund).bind(format!("ZZFUND{}", fund.simple())).execute(pool).await.unwrap();
            fund
        }

        /// Two allocations in flight at once, P → {F1, F2} and F2 → P, the way
        /// create_allocations runs them (all positions locked up front, then one
        /// transfer per split). F2 sorts below P, so per-pair locking alone would
        /// deadlock: A holds P waiting for F2 while B holds F2 waiting for P.
        #[tokio::test]
        #[ignore = "needs a live Postgres; run with --ignored"]
        async fn concurrent_crossing_allocations_do_not_deadlock() {
            let pool = test_pool().await;
            let b = book(&pool).await;
            fill(&pool, &b, OrderSide::Buy, "b0", 4.0, 100.0, t(1)).await;
            let f1 = new_fund(&pool).await;
            let f2 = new_fund_with(&pool, Uuid::from_u128(rand_low())).await;
            assert!(f2 < b.portfolio);
            let p = b.portfolio;
            let inst = b.instrument.clone();
            let a = {
                let (pool, inst) = (pool.clone(), inst.clone());
                async move {
                    let mut tx = pool.begin().await?;
                    lock_positions(&mut tx, &[f1, f2, p], &inst).await?;
                    persist_transfer(&mut tx, p, f1, &inst, OrderSide::Buy, 1.0, 100.0).await?;
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    persist_transfer(&mut tx, p, f2, &inst, OrderSide::Buy, 1.0, 100.0).await?;
                    tx.commit().await
                }
            };
            let bb = {
                let (pool, inst) = (pool.clone(), inst.clone());
                async move {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    let mut tx = pool.begin().await?;
                    lock_positions(&mut tx, &[p, f2], &inst).await?;
                    persist_transfer(&mut tx, f2, p, &inst, OrderSide::Buy, 1.0, 100.0).await?;
                    tx.commit().await
                }
            };
            let (ra, rb): (Result<(), sqlx::Error>, Result<(), sqlx::Error>) = tokio::join!(a, bb);
            for f in [f1, f2] {
                sqlx::query("DELETE FROM position WHERE portfolio_id = $1").bind(f).execute(&pool).await.unwrap();
                sqlx::query("DELETE FROM kafka_position_outbox WHERE portfolio_id = $1").bind(f).execute(&pool).await.unwrap();
                sqlx::query("DELETE FROM portfolio WHERE id = $1").bind(f).execute(&pool).await.unwrap();
            }
            drop_book(&pool, &b).await;
            ra.expect("allocation P → F1, F2");
            rb.expect("allocation F2 → P");
        }

        fn rand_low() -> u128 {
            (Uuid::new_v4().as_u128() >> 8) & 0x0000_ffff_ffff_ffff_ffff_ffff_ffff_ffff
        }

        #[tokio::test]
        #[ignore = "needs a live Postgres; run with --ignored"]
        async fn base_fee_on_newest_fill_does_not_rebuild() {
            let pool = test_pool().await;
            let b = book(&pool).await;
            fill(&pool, &b, OrderSide::Buy, "e1", 1.0, 100.0, t(1)).await;
            fill(&pool, &b, OrderSide::Buy, "e2", 1.0, 110.0, t(2)).await;
            let newest = fee(&pool, &b, OrderSide::Buy, "e2", 0.01, FeeEffect::Base).await;
            let older = fee(&pool, &b, OrderSide::Buy, "e1", 0.01, FeeEffect::Base).await;
            let got = pos(&pool, b.portfolio, &b.instrument).await;
            drop_book(&pool, &b).await;
            assert!(!newest, "a base fee on the newest fill applies incrementally");
            assert!(older, "a base fee on an older fill rebuilds");
            let want = PositionMath::FLAT
                .apply_fill(OrderSide::Buy, 1.0, 100.0)
                .apply_fee(0.01, FeeEffect::Base)
                .apply_fill(OrderSide::Buy, 1.0, 110.0)
                .apply_fee(0.01, FeeEffect::Base);
            close(got, want, "fees in trade-time order");
        }

        #[tokio::test]
        #[ignore = "needs a live Postgres; run with --ignored"]
        async fn equal_time_fill_with_larger_exec_id_stays_incremental() {
            let pool = test_pool().await;
            let b = book(&pool).await;
            fill(&pool, &b, OrderSide::Buy, "x-a", 1.0, 100.0, t(2)).await;
            let larger = fill(&pool, &b, OrderSide::Buy, "x-b", 1.0, 100.0, t(2)).await;
            let smaller = fill(&pool, &b, OrderSide::Buy, "x-0", 1.0, 100.0, t(2)).await;
            drop_book(&pool, &b).await;
            assert!(!larger, "sorts after the last input: incremental");
            assert!(smaller, "sorts before the last input: rebuild");
        }

        async fn drop_fund(pool: &PgPool, b: &Book, fund: Uuid) {
            sqlx::query("DELETE FROM allocation WHERE order_id = $1").bind(b.buy).execute(pool).await.unwrap();
            sqlx::query("DELETE FROM position WHERE portfolio_id = $1").bind(fund).execute(pool).await.unwrap();
            sqlx::query("DELETE FROM kafka_position_outbox WHERE portfolio_id = $1").bind(fund).execute(pool).await.unwrap();
            sqlx::query("DELETE FROM portfolio WHERE id = $1").bind(fund).execute(pool).await.unwrap();
        }

        /// Every arrival order of the fills, with the fees and one allocation transfer
        /// as independent arrivals placed at varying positions (a fee ahead of its fill,
        /// a fee after later fills, the transfer anywhere), ends at the position the
        /// trade-time order gives.
        #[tokio::test]
        #[ignore = "needs a live Postgres; run with --ignored"]
        async fn any_arrival_order_gives_the_same_position() {
            use OrderSide::{Buy, Sell};
            let pool = test_pool().await;
            // in trade-time order: open long, add, flip short through zero, then an
            // equal-time pair (e4 sorts before e5) that flips back long and trims
            let fills = [
                (Buy, "e1", 2.0, 100.0, t(1)),
                (Buy, "e2", 1.0, 110.0, t(2)),
                (Sell, "e3", 4.0, 120.0, t(3)),
                (Buy, "e4", 3.0, 90.0, t(4)),
                (Sell, "e5", 1.0, 95.0, t(4)),
            ];
            // fees on fills e2 (base), e3 (quote), e4 (base)
            let fees = [(1, 0.01, FeeEffect::Base), (2, 0.5, FeeEffect::Quote), (3, 0.02, FeeEffect::Base)];
            let (xfer_qty, xfer_px) = (0.5, 100.0); // block → fund, booked now: after every fill
            let mut want = PositionMath::FLAT;
            for (i, (side, _, qty, price, _)) in fills.iter().enumerate() {
                want = want.apply_fill(*side, *qty, *price);
                for (on, amount, effect) in fees {
                    if on == i {
                        want = want.apply_fee(amount, effect);
                    }
                }
            }
            want = want.reduce_at_cost(xfer_qty);
            let fund_want = PositionMath::FLAT.apply_fill(Buy, xfer_qty, xfer_px);

            let mut failures = Vec::new();
            for (k, perm) in permutations(fills.len()).into_iter().enumerate() {
                let mut arrivals: Vec<Input> = perm.iter().map(|&i| Input::Fill(i)).collect();
                // spread the other inputs over every slot as k varies
                for (j, _) in fees.iter().enumerate() {
                    let slot = (k / (j + 1) + j * 2) % (arrivals.len() + 1);
                    arrivals.insert(slot, Input::Fee(j));
                }
                let slot = (k * 7) % (arrivals.len() + 1);
                arrivals.insert(slot, Input::Transfer);

                let b = book(&pool).await;
                let fund = new_fund(&pool).await;
                for input in &arrivals {
                    match *input {
                        Input::Fill(i) => {
                            let (side, exec, qty, price, at) = fills[i];
                            fill(&pool, &b, side, exec, qty, price, at).await;
                        }
                        Input::Fee(j) => {
                            let (on, amount, effect) = fees[j];
                            let (side, exec, ..) = fills[on];
                            fee(&pool, &b, side, exec, amount, effect).await;
                        }
                        Input::Transfer => allocate(&pool, &b, fund, xfer_qty, xfer_px).await,
                    }
                }
                let got = pos(&pool, b.portfolio, &b.instrument).await;
                let fund_got = pos(&pool, fund, &b.instrument).await;
                drop_fund(&pool, &b, fund).await;
                drop_book(&pool, &b).await;
                let near = |g: (f64, f64, f64), w: PositionMath| {
                    (g.0 - w.net_qty).abs() < EPS && (g.1 - w.avg_cost).abs() < EPS && (g.2 - w.realized_pnl).abs() < EPS
                };
                if !near(got, want) || !near(fund_got, fund_want) {
                    failures.push(format!("{arrivals:?}: block {got:?} fund {fund_got:?}"));
                }
            }
            assert!(failures.is_empty(), "want {want:?}; {} of 120 differ, e.g. {:?}", failures.len(), failures.first());
        }

        #[tokio::test]
        #[ignore = "needs a live Postgres; run with --ignored"]
        async fn fill_older_than_allocation_triggers_rebuild() {
            let pool = test_pool().await;
            let b = book(&pool).await;
            let fund = new_fund(&pool).await;
            let now = Utc::now();
            fill(&pool, &b, OrderSide::Buy, "b0", 2.0, 80.0, now - Duration::seconds(10)).await;
            allocate(&pool, &b, fund, 1.0, 80.0).await;
            // traded before the allocation, after the last fill: lands before the transfer
            fill(&pool, &b, OrderSide::Buy, "b1", 1.0, 100.0, now - Duration::seconds(5)).await;
            let got = pos(&pool, b.portfolio, &b.instrument).await;
            drop_fund(&pool, &b, fund).await;
            drop_book(&pool, &b).await;
            // 2@80 + 1@100 → 3 @ 86.67; 1 out at cost → 2 @ 86.67
            close(got, PositionMath { net_qty: 2.0, avg_cost: 260.0 / 3.0, realized_pnl: 0.0 }, "block");
        }

        #[tokio::test]
        #[ignore = "needs a live Postgres; run with --ignored"]
        async fn concurrent_first_fills_on_new_position_both_apply() {
            let pool = test_pool().await;
            let mut lost = 0;
            for _ in 0..20 {
                let b = book(&pool).await;
                tokio::join!(
                    fill(&pool, &b, OrderSide::Buy, "c1", 2.0, 100.0, t(1)),
                    fill(&pool, &b, OrderSide::Sell, "c2", 1.0, 110.0, t(2)),
                );
                let got = pos(&pool, b.portfolio, &b.instrument).await;
                drop_book(&pool, &b).await;
                if (got.0 - 1.0).abs() > EPS || (got.2 - 10.0).abs() > EPS {
                    lost += 1;
                }
            }
            assert_eq!(lost, 0, "a concurrent first fill was lost or misordered");
        }

        #[tokio::test]
        #[ignore = "needs a live Postgres; run with --ignored"]
        async fn oms_fill_stores_its_trade_time() {
            use crate::domain::orders::commands::ExecutionReport;
            let pool = test_pool().await;
            let (order_id, portfolio_id, instrument_id) = seeded_order(&pool, 0).await;
            let report = ExecutionReport::Fill {
                execution_id: Uuid::new_v4().to_string(), fill_qty: 1.0, fill_price: 100.0, venue: "TEST".into(),
            };
            let applied = crate::execution::process_execution_report(&pool, order_id, report, "test", None).await;
            let row: Option<(Option<DateTime<Utc>>, Option<DateTime<Utc>>)> = sqlx::query_as(
                "SELECT e.occurred_at, p.last_trade_at FROM order_event e, position p \
                 WHERE e.order_id = $1 AND e.event_type IN ('order_partially_filled', 'order_filled') \
                   AND p.portfolio_id = $2 AND p.instrument_id = $3")
                .bind(order_id).bind(portfolio_id).bind(&instrument_id).fetch_optional(&pool).await.unwrap();
            remove_seeded(&pool, order_id).await;
            applied.unwrap();
            let (occurred, last) = row.expect("fill event and position");
            assert!(occurred.is_some(), "the fill's trade time is stored as occurred_at");
            assert_eq!(occurred, last, "the compared key is the stored key");
        }

        #[tokio::test]
        #[ignore = "needs a live Postgres; run with --ignored"]
        async fn late_fill_triggers_rebuild_in_trade_time_order() {
            let pool = test_pool().await;
            let b = book(&pool).await;
            fill(&pool, &b, OrderSide::Buy, "b2", 1.0, 100.0, t(2)).await;
            fill(&pool, &b, OrderSide::Sell, "s3", 1.0, 110.0, t(3)).await;
            fill(&pool, &b, OrderSide::Buy, "b1", 1.0, 90.0, t(1)).await; // late
            let got = pos(&pool, b.portfolio, &b.instrument).await;
            let last: Option<DateTime<Utc>> = sqlx::query_scalar(
                "SELECT last_trade_at FROM position WHERE portfolio_id = $1 AND instrument_id = $2")
                .bind(b.portfolio).bind(&b.instrument).fetch_one(&pool).await.unwrap();
            drop_book(&pool, &b).await;
            // buy@90, buy@100 (avg 95), sell@110 → realized 15, long 1 @ 95
            close(got, PositionMath { net_qty: 1.0, avg_cost: 95.0, realized_pnl: 15.0 }, "late fill");
            assert_eq!(last, Some(t(3)));
        }

        #[tokio::test]
        #[ignore = "needs a live Postgres; run with --ignored"]
        async fn rebuild_reapplies_fees_once() {
            let pool = test_pool().await;
            let b = book(&pool).await;
            fill(&pool, &b, OrderSide::Buy, "b0", 1.0, 80.0, t(0)).await;
            fill(&pool, &b, OrderSide::Buy, "b2", 2.0, 100.0, t(2)).await;
            fee(&pool, &b, OrderSide::Buy, "b2", 0.1, FeeEffect::Base).await;
            fee(&pool, &b, OrderSide::Buy, "b2", 0.5, FeeEffect::Quote).await;
            fill(&pool, &b, OrderSide::Sell, "s1", 1.0, 120.0, t(1)).await; // late
            let got = pos(&pool, b.portfolio, &b.instrument).await;
            drop_book(&pool, &b).await;
            // long 1@80, sell@120 → +40 flat, buy 2@100; base fee → 1.9; quote fee → 39.5
            close(got, PositionMath { net_qty: 1.9, avg_cost: 100.0, realized_pnl: 39.5 }, "fees once");
        }

        #[tokio::test]
        #[ignore = "needs a live Postgres; run with --ignored"]
        async fn rebuild_includes_colo_transfers() {
            let pool = test_pool().await;
            let b = book(&pool).await;
            let fund = Uuid::new_v4();
            sqlx::query("INSERT INTO portfolio (id, code, name, status) VALUES ($1, $2, $2, 'ACTIVE')")
                .bind(fund).bind(format!("ZZFUND{}", &fund.simple().to_string()[..8])).execute(&pool).await.unwrap();
            fill(&pool, &b, OrderSide::Buy, "b0", 1.0, 80.0, t(0)).await;
            fill(&pool, &b, OrderSide::Buy, "b2", 2.0, 100.0, t(2)).await;
            let mut tx = pool.begin().await.unwrap();
            sqlx::query(
                "INSERT INTO allocation (order_id, from_portfolio_id, to_portfolio_id, instrument_id, qty, price) \
                 VALUES ($1, $2, $3, $4, 1, 100)")
                .bind(b.buy).bind(b.portfolio).bind(fund).bind(&b.instrument).execute(&mut *tx).await.unwrap();
            persist_transfer(&mut tx, b.portfolio, fund, &b.instrument, OrderSide::Buy, 1.0, 100.0).await.unwrap();
            tx.commit().await.unwrap();
            fill(&pool, &b, OrderSide::Sell, "s1", 1.0, 120.0, t(1)).await; // late, before the block fill
            let got = pos(&pool, b.portfolio, &b.instrument).await;
            let fund_pos = pos(&pool, fund, &b.instrument).await;
            sqlx::query("DELETE FROM allocation WHERE order_id = $1").bind(b.buy).execute(&pool).await.unwrap();
            sqlx::query("DELETE FROM position WHERE portfolio_id = $1").bind(fund).execute(&pool).await.unwrap();
            sqlx::query("DELETE FROM kafka_position_outbox WHERE portfolio_id = $1").bind(fund).execute(&pool).await.unwrap();
            sqlx::query("DELETE FROM portfolio WHERE id = $1").bind(fund).execute(&pool).await.unwrap();
            drop_book(&pool, &b).await;
            // long 1@80, sell@120 → +40 flat, buy 2@100; 1 transferred out at cost → long 1@100
            close(got, PositionMath { net_qty: 1.0, avg_cost: 100.0, realized_pnl: 40.0 }, "block");
            close(fund_pos, PositionMath { net_qty: 1.0, avg_cost: 100.0, realized_pnl: 0.0 }, "fund");
        }

        #[tokio::test]
        #[ignore = "needs a live Postgres; run with --ignored"]
        async fn equal_trade_times_order_by_execution_id() {
            let pool = test_pool().await;
            let b = book(&pool).await;
            fill(&pool, &b, OrderSide::Buy, "a0", 1.0, 90.0, t(1)).await;
            // same trade time: "x-b" arrives first, "x-a" sorts first
            fill(&pool, &b, OrderSide::Buy, "x-b", 1.0, 100.0, t(2)).await;
            fill(&pool, &b, OrderSide::Sell, "x-a", 1.0, 110.0, t(2)).await;
            let got = pos(&pool, b.portfolio, &b.instrument).await;
            drop_book(&pool, &b).await;
            // long 1@90; x-a sells it → +20, flat; x-b buys → long 1@100
            close(got, PositionMath { net_qty: 1.0, avg_cost: 100.0, realized_pnl: 20.0 }, "equal times");
        }

        #[tokio::test]
        #[ignore = "needs a live Postgres; run with --ignored"]
        async fn full_rebuild_matches_late_fill_rebuild() {
            let pool = test_pool().await;
            let b = book(&pool).await;
            fill(&pool, &b, OrderSide::Buy, "b0", 1.0, 80.0, t(0)).await;
            fill(&pool, &b, OrderSide::Buy, "b2", 2.0, 100.0, t(2)).await;
            fee(&pool, &b, OrderSide::Buy, "b2", 0.1, FeeEffect::Base).await;
            fill(&pool, &b, OrderSide::Sell, "s1", 1.0, 120.0, t(1)).await; // late
            let live = pos(&pool, b.portfolio, &b.instrument).await;
            let rebuilt = rebuild_positions_scoped(&pool, Some(b.portfolio)).await;
            let after = pos(&pool, b.portfolio, &b.instrument).await;
            drop_book(&pool, &b).await;
            rebuilt.unwrap();
            close(live, PositionMath { net_qty: 1.9, avg_cost: 100.0, realized_pnl: 40.0 }, "live");
            assert_eq!(after, live);
        }

        /// A rebuild waits for a writer holding the position lock, then sees its
        /// fill: it neither overwrites an in-flight booking nor misses it.
        #[tokio::test]
        #[ignore = "needs a live Postgres; run with --ignored"]
        async fn rebuild_waits_for_in_flight_fill() {
            let pool = test_pool().await;
            let b = book(&pool).await;
            fill(&pool, &b, OrderSide::Buy, "w0", 1.0, 100.0, t(0)).await;
            // a writer mid-booking: lock held, fill appended and applied, not committed
            let mut tx = pool.begin().await.unwrap();
            let payload = OrderEventPayload::OrderPartiallyFilled {
                execution_id: "w1".into(), fill_qty: 1.0, fill_price: 200.0, cum_qty: 1.0, leaves_qty: 0.0, avg_px: 200.0, venue: "TEST".into(),
            };
            append(&pool, &mut tx, b.buy, OrderEventType::OrderPartiallyFilled, payload, Some(t(1))).await;
            persist_fill(&mut tx, b.portfolio, &b.instrument, OrderSide::Buy, 1.0, 200.0, t(1), "w1").await.unwrap();
            let rebuild = tokio::spawn({
                let pool = pool.clone();
                let portfolio = b.portfolio;
                async move { rebuild_positions_scoped(&pool, Some(portfolio)).await }
            });
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let waited = !rebuild.is_finished();
            tx.commit().await.unwrap();
            let rebuilt = rebuild.await.unwrap();
            let after = pos(&pool, b.portfolio, &b.instrument).await;
            drop_book(&pool, &b).await;
            rebuilt.unwrap();
            assert!(waited, "rebuild did not wait for the position lock");
            close(after, PositionMath { net_qty: 2.0, avg_cost: 150.0, realized_pnl: 0.0 }, "after");
        }

        /// A reported strategy (own portfolio) on the book's instrument, with one batch.
        struct Reported { code: String, portfolio: Uuid, owner: Uuid }

        async fn reported(pool: &PgPool) -> Reported {
            let portfolio = new_fund(pool).await;
            let owner = Uuid::new_v4();
            let code = format!("ZZRPT{}", owner.simple());
            sqlx::query("INSERT INTO principal (id, code, principal_type, status) VALUES ($1, $2, 'HUMAN', 'ACTIVE')")
                .bind(owner).bind(&code).execute(pool).await.unwrap();
            sqlx::query("INSERT INTO strategy (code, name, portfolio_id, owner_principal_id, mode) VALUES ($1, $1, $2, $3, 'REPORTED')")
                .bind(&code).bind(portfolio).bind(owner).execute(pool).await.unwrap();
            sqlx::query("INSERT INTO reported_batch (strategy_code, batch_id, response_json, payload_json) VALUES ($1, 'b', '{}', '{}')")
                .bind(&code).execute(pool).await.unwrap();
            Reported { code, portfolio, owner }
        }

        async fn drop_reported(pool: &PgPool, r: &Reported) {
            for (sql, bind) in [
                ("DELETE FROM strategy WHERE code = $1", r.code.clone()),
                ("DELETE FROM principal WHERE id = $1::uuid", r.owner.to_string()),
                ("DELETE FROM position WHERE portfolio_id = $1::uuid", r.portfolio.to_string()),
                ("DELETE FROM kafka_position_outbox WHERE portfolio_id = $1::uuid", r.portfolio.to_string()),
                ("DELETE FROM portfolio WHERE id = $1::uuid", r.portfolio.to_string()),
            ] {
                sqlx::query(sql).bind(bind).execute(pool).await.expect(sql);
            }
        }

        /// Insert one reported fill row (no position effect by itself).
        #[allow(clippy::too_many_arguments)]
        async fn report(
            conn: &mut PgConnection, r: &Reported, instrument: &str, exec: &str, side: &str,
            qty: f64, price: f64, at: DateTime<Utc>, fee: Option<(f64, &str)>,
        ) {
            sqlx::query(
                "INSERT INTO reported_fill (id, strategy_code, batch_id, venue, account_ref, exec_id, symbol, \
                   instrument_id, side, qty, price, fee, fee_asset, fee_effect, trade_time, raw_json) \
                 VALUES ($1, $2, 'b', 'TEST', 'acct', $3, 'SYM', $4::bigint, $5, $6, $7, $8, 'X', $9, $10, '{}')")
                .bind(Uuid::new_v4()).bind(&r.code).bind(exec).bind(instrument).bind(side)
                .bind(qty).bind(price).bind(fee.map(|f| f.0)).bind(fee.map(|f| f.1)).bind(at)
                .execute(conn).await.expect("insert reported fill");
        }

        #[tokio::test]
        #[ignore = "needs a live Postgres; run with --ignored"]
        async fn rebuild_keeps_reported_fills() {
            let pool = test_pool().await;
            let b = book(&pool).await;
            let r = reported(&pool).await;
            let mut conn = pool.acquire().await.unwrap();
            report(&mut conn, &r, &b.instrument, "r1", "BUY", 1.0, 100.0, t(1), None).await;
            report(&mut conn, &r, &b.instrument, "r2", "SELL", 1.0, 110.0, t(2), None).await;
            drop(conn);
            let res = rebuild_positions_scoped(&pool, Some(r.portfolio)).await;
            let got = pos(&pool, r.portfolio, &b.instrument).await;
            drop_reported(&pool, &r).await;
            drop_book(&pool, &b).await;
            res.unwrap();
            close(got, PositionMath { net_qty: 0.0, avg_cost: 0.0, realized_pnl: 10.0 }, "reported rebuild");
        }

        #[tokio::test]
        #[ignore = "needs a live Postgres; run with --ignored"]
        async fn late_reported_fill_rebuilds_in_trade_time_order() {
            let pool = test_pool().await;
            let b = book(&pool).await;
            let r = reported(&pool).await;
            for (exec, side, os, price, at) in [("r2", "BUY", OrderSide::Buy, 100.0, t(2)), ("r3", "SELL", OrderSide::Sell, 110.0, t(3)), ("r1", "BUY", OrderSide::Buy, 90.0, t(1))] {
                let mut tx = pool.begin().await.unwrap();
                report(&mut tx, &r, &b.instrument, exec, side, 1.0, price, at, None).await;
                persist_fill(&mut tx, r.portfolio, &b.instrument, os, 1.0, price, at, &format!("TEST|acct|{exec}")).await.unwrap();
                tx.commit().await.unwrap();
            }
            let incremental = pos(&pool, r.portfolio, &b.instrument).await;
            let res = rebuild_positions_scoped(&pool, Some(r.portfolio)).await;
            let rebuilt = pos(&pool, r.portfolio, &b.instrument).await;
            drop_reported(&pool, &r).await;
            drop_book(&pool, &b).await;
            res.unwrap();
            let want = PositionMath { net_qty: 1.0, avg_cost: 95.0, realized_pnl: 15.0 };
            close(incremental, want, "late reported fill");
            close(rebuilt, want, "rebuild after late reported fill");
        }

        #[tokio::test]
        #[ignore = "needs a live Postgres; run with --ignored"]
        async fn reported_quote_fee_reduces_realized() {
            let pool = test_pool().await;
            let b = book(&pool).await;
            let r = reported(&pool).await;
            let mut tx = pool.begin().await.unwrap();
            report(&mut tx, &r, &b.instrument, "r1", "BUY", 1.0, 100.0, t(1), Some((0.5, "Quote"))).await;
            persist_fill(&mut tx, r.portfolio, &b.instrument, OrderSide::Buy, 1.0, 100.0, t(1), "TEST|acct|r1").await.unwrap();
            persist_reported_fee(&mut tx, r.portfolio, &b.instrument, "TEST|acct|r1", t(1), 0.5, FeeEffect::Quote).await.unwrap();
            report(&mut tx, &r, &b.instrument, "r2", "SELL", 1.0, 110.0, t(2), Some((0.001, "Other"))).await;
            persist_fill(&mut tx, r.portfolio, &b.instrument, OrderSide::Sell, 1.0, 110.0, t(2), "TEST|acct|r2").await.unwrap();
            tx.commit().await.unwrap();
            let incremental = pos(&pool, r.portfolio, &b.instrument).await;
            let res = rebuild_positions_scoped(&pool, Some(r.portfolio)).await;
            let rebuilt = pos(&pool, r.portfolio, &b.instrument).await;
            drop_reported(&pool, &r).await;
            drop_book(&pool, &b).await;
            res.unwrap();
            let want = PositionMath { net_qty: 0.0, avg_cost: 0.0, realized_pnl: 9.5 };
            close(incremental, want, "incremental quote fee");
            close(rebuilt, want, "rebuilt quote fee");
        }
    }
}
