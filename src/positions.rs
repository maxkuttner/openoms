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
}

/// A position row as returned by the API.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct Position {
    pub portfolio_id: Uuid,
    pub instrument_id: String,
    pub net_qty: f64,
    pub avg_cost: f64,
    pub realized_pnl: f64,
    pub updated_at: DateTime<Utc>,
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

    let current = match row {
        Some(r) => PositionMath {
            net_qty: r.get("net_qty"),
            avg_cost: r.get("avg_cost"),
            realized_pnl: r.get("realized_pnl"),
        },
        None => PositionMath::FLAT,
    };
    let next = current.apply_fill(side, fill_qty, fill_price);

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
    .bind(next.net_qty)
    .bind(next.avg_cost)
    .bind(next.realized_pnl)
    .execute(&mut *conn)
    .await?;
    Ok(())
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
}
