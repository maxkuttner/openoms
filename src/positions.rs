//! Position projection (read model): net holding + cost basis + realized P&L per
//! (portfolio, instrument), maintained from fill events. Not part of the
//! event-sourced order aggregate. Unrealized P&L is intentionally absent — it needs
//! a market-data mark (later).

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::domain::orders::events::{OrderDomainEvent, OrderEventPayload};
use crate::domain::orders::state::OrderSide;

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

/// Load → apply one fill → upsert, inside the caller's transaction (so it commits
/// atomically with the order_state update + event append).
pub async fn persist_fill(
    conn: &mut PgConnection,
    portfolio_id: Uuid,
    instrument_id: &str,
    side: OrderSide,
    fill_qty: f64,
    fill_price: f64,
) -> Result<(), sqlx::Error> {
    let next = load(conn, portfolio_id, instrument_id).await?.apply_fill(side, fill_qty, fill_price);
    upsert(conn, portfolio_id, instrument_id, next).await
}

/// Cost-preserving allocation transfer of `qty` (> 0) of `instrument_id` from one
/// portfolio to another at `price`, inside the caller's tx. The source reduces at
/// cost (no P&L — a conduit); the destination books at `price` via the normal fill
/// logic. Caller validates `qty <= |source net_qty|`.
pub async fn persist_transfer(
    conn: &mut PgConnection,
    from_portfolio: Uuid,
    to_portfolio: Uuid,
    instrument_id: &str,
    side: OrderSide,
    qty: f64,
    price: f64,
) -> Result<(), sqlx::Error> {
    let from = load(conn, from_portfolio, instrument_id).await?.reduce_at_cost(qty);
    upsert(conn, from_portfolio, instrument_id, from).await?;
    let to = load(conn, to_portfolio, instrument_id).await?.apply_fill(side, qty, price);
    upsert(conn, to_portfolio, instrument_id, to).await
}

/// Recovery / backfill: rebuild every position by replaying the fill events from the
/// order_event log in order. Idempotent (derived data).
pub async fn rebuild_positions(pool: &PgPool) -> Result<usize, sqlx::Error> {
    // Each fill event carries qty/price; portfolio/instrument/side live on order_state.
    let rows = sqlx::query(
        "SELECT e.payload_json, os.portfolio_id, os.instrument_id, os.side \
         FROM order_event e \
         JOIN order_state os ON os.order_id = e.order_id \
         WHERE e.event_type IN ('order_partially_filled', 'order_filled') \
         ORDER BY e.gloabl_position",
    )
    .fetch_all(pool)
    .await?;

    let mut acc: HashMap<(Uuid, String), PositionMath> = HashMap::new();
    for r in &rows {
        let payload: serde_json::Value = r.get("payload_json");
        let Ok(event) = serde_json::from_value::<OrderDomainEvent>(payload) else {
            continue;
        };
        let (fill_qty, fill_price) = match event.payload {
            OrderEventPayload::OrderPartiallyFilled { fill_qty, fill_price, .. }
            | OrderEventPayload::OrderFilled { fill_qty, fill_price, .. } => (fill_qty, fill_price),
            _ => continue,
        };
        let portfolio_id: Uuid = r.get("portfolio_id");
        let instrument_id: String = r.get("instrument_id");
        let side = match r.get::<String, _>("side").as_str() {
            "buy" => OrderSide::Buy,
            _ => OrderSide::Sell,
        };
        let entry = acc.entry((portfolio_id, instrument_id)).or_insert(PositionMath::FLAT);
        *entry = entry.apply_fill(side, fill_qty, fill_price);
    }

    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM position").execute(&mut *tx).await?;
    for ((portfolio_id, instrument_id), p) in &acc {
        sqlx::query(
            "INSERT INTO position \
                (portfolio_id, instrument_id, net_qty, avg_cost, realized_pnl) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(portfolio_id)
        .bind(instrument_id)
        .bind(p.net_qty)
        .bind(p.avg_cost)
        .bind(p.realized_pnl)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(acc.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "expected {b}, got {a}");
    }

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
}
