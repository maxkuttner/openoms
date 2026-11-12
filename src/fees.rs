//! Venue fee domain: how a reported fee affects the position, and booking a
//! venue-reported fee against an order (event + `order_fee` row + position).

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use tracing::{info, warn};
use uuid::Uuid;

use crate::domain::orders::events::{OrderDomainEvent, OrderEventPayload, OrderEventType};
use crate::event_bus::{publish_appended, EventBus, OrderLaneEvent};
use crate::event_store::{EventStoreError, NewOrderEvent, OrderEventStore};
use crate::execution::{retry_on_conflict, ConcurrencyRetry};
use crate::handlers::parse_order_status;
use crate::positions;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// One venue-reported fee. `amount` is positive for a charge, negative for a rebate.
#[derive(Debug, Clone, PartialEq)]
pub struct FeeCharge {
    pub fee_id: String,
    pub execution_id: Option<String>,
    pub amount: f64,
    pub currency: String,
}

/// Outcome of [`book_fee`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeeBooked {
    Booked,
    /// This `(order_id, fee_id)` was booked before; nothing written.
    Duplicate,
    /// No such order; nothing written.
    UnknownOrder,
}

/// How a fee in a given currency hits the position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FeeEffect {
    /// Fee in the instrument currency: reduces realized P&L.
    Quote,
    /// Fee in the base asset of a pair: shrinks the held quantity.
    Base,
    /// Any other currency: tracked separately, no position effect.
    Other,
}

/// Classify a fee by its currency relative to the instrument (case-insensitive).
pub fn fee_effect(
    fee_currency: &str,
    symbol: &str,
    asset_class: &str,
    instrument_currency: &str,
) -> FeeEffect {
    let fee = fee_currency.to_uppercase();
    let quote = instrument_currency.to_uppercase();
    if fee == quote {
        return FeeEffect::Quote;
    }
    let class = asset_class.to_uppercase();
    if class == "CRYPTO" || class == "FX" {
        let sym: String = symbol
            .chars()
            .filter(|c| !matches!(c, '-' | '.' | '/' | '_'))
            .collect::<String>()
            .to_uppercase();
        if let Some(rest) = sym.strip_suffix(&quote) {
            if rest == fee {
                return FeeEffect::Base;
            }
        }
    }
    FeeEffect::Other
}

/// Book one venue fee on `order_id`: an `order_fee` row, the position effect and
/// an `order_fee_charged` event, in one transaction. Terminal orders still book
/// (fees often arrive with or after the final fill). Retries version conflicts.
pub async fn book_fee(
    pool: &PgPool,
    bus: Option<&EventBus>,
    order_id: Uuid,
    fee: FeeCharge,
    actor: &str,
) -> Result<FeeBooked, BoxError> {
    retry_on_conflict(order_id, "fee", || book_once(pool, bus, order_id, &fee, actor)).await
}

async fn book_once(
    pool: &PgPool,
    bus: Option<&EventBus>,
    order_id: Uuid,
    fee: &FeeCharge,
    actor: &str,
) -> Result<FeeBooked, BoxError> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT os.portfolio_id, os.instrument_id, os.status, os.version, \
                i.symbol, i.asset_class, i.currency \
         FROM order_state os \
         LEFT JOIN instrument i ON i.id::text = os.instrument_id \
         WHERE os.order_id = $1 \
         FOR UPDATE OF os",
    )
    .bind(order_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        warn!(order_id = %order_id, fee_id = %fee.fee_id, "fee: order not found, not booked");
        return Ok(FeeBooked::UnknownOrder);
    };

    let portfolio_id: Uuid = row.get("portfolio_id");
    let instrument_id: String = row.get("instrument_id");
    let version: i64 = row.get("version");
    let status = parse_order_status(row.get("status")).map_err(|e| e.message.clone())?;
    let effect = match (
        row.get::<Option<String>, _>("symbol"),
        row.get::<Option<String>, _>("asset_class"),
        row.get::<Option<String>, _>("currency"),
    ) {
        (Some(symbol), Some(class), Some(ccy)) => fee_effect(&fee.currency, &symbol, &class, &ccy),
        _ => {
            warn!(order_id = %order_id, instrument_id, "fee: instrument not found, booking without position effect");
            FeeEffect::Other
        }
    };
    let effect_str = match effect {
        FeeEffect::Quote => "quote",
        FeeEffect::Base => "base",
        FeeEffect::Other => "other",
    };

    let inserted = sqlx::query(
        "INSERT INTO order_fee \
            (order_id, fee_id, execution_id, portfolio_id, instrument_id, amount, currency, effect) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         ON CONFLICT DO NOTHING",
    )
    .bind(order_id)
    .bind(&fee.fee_id)
    .bind(&fee.execution_id)
    .bind(portfolio_id)
    .bind(&instrument_id)
    .bind(fee.amount)
    .bind(&fee.currency)
    .bind(effect_str)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted == 0 {
        tx.rollback().await?;
        return Ok(FeeBooked::Duplicate);
    }

    positions::persist_fee(&mut *tx, portfolio_id, &instrument_id, fee.amount, effect).await?;

    let event = OrderDomainEvent {
        event_id: Uuid::new_v4().to_string(),
        event_type: OrderEventType::OrderFeeCharged,
        order_id: order_id.to_string(),
        timestamp: Utc::now(),
        actor: actor.to_string(),
        payload: OrderEventPayload::OrderFeeCharged {
            fee_id: fee.fee_id.clone(),
            execution_id: fee.execution_id.clone(),
            amount: fee.amount,
            currency: fee.currency.clone(),
            effect,
        },
        version: version + 1,
        status_after: status,
    };
    let new_events = vec![NewOrderEvent {
        event_id: event.event_id.clone(),
        event_type: event.event_type.as_str().to_string(),
        actor: event.actor.clone(),
        payload: serde_json::to_value(&event)?,
        correlation_id: None,
        causation_id: None,
        schema_version: 0,
    }];
    let appended = OrderEventStore::new(pool.clone())
        .append_events_in_tx(&mut tx, order_id, version, &new_events)
        .await
        .map_err(|e| -> BoxError {
            match e {
                EventStoreError::Concurrency(c) => {
                    Box::new(ConcurrencyRetry { expected: c.expected, actual: c.actual })
                }
                other => format!("event store error: {other:?}").into(),
            }
        })?;

    sqlx::query("UPDATE order_state SET version = version + 1, updated_at = now() WHERE order_id = $1")
        .bind(order_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    info!(order_id = %order_id, fee_id = %fee.fee_id, amount = fee.amount, currency = %fee.currency, ?effect, "fee booked");

    if let Some(bus) = bus {
        publish_appended(bus, order_id, &new_events, &appended);
        if effect != FeeEffect::Other {
            bus.publish_order(OrderLaneEvent::PositionChanged { portfolio_id, instrument_id });
        }
    }
    Ok(FeeBooked::Booked)
}

/// Sum of the fees booked on `order_id` in `currency` (exact match).
pub async fn booked_fee_total(pool: &PgPool, order_id: Uuid, currency: &str) -> Result<f64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount), 0)::double precision FROM order_fee \
         WHERE order_id = $1 AND currency = $2",
    )
    .bind(order_id)
    .bind(currency)
    .fetch_one(pool)
    .await
}

/// The fee to book to bring `booked` up to a venue-reported running `total`, or
/// `None` when they already agree. The id names the total reached, so a repeated
/// total dedupes and a corrected one books its own difference.
pub fn fee_delta(order_id: Uuid, venue: &str, booked: f64, total: f64, currency: &str) -> Option<FeeCharge> {
    if (total - booked).abs() < 1e-12 {
        return None;
    }
    Some(FeeCharge {
        fee_id: format!("{venue}-{order_id}-{currency}-{total}"),
        execution_id: None,
        amount: total - booked,
        currency: currency.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) { assert!((a - b).abs() < 1e-9, "{a} != {b}"); }
    #[test] fn fee_delta_books_the_increase() {
        let f = fee_delta(Uuid::nil(), "COINBASE", 0.5, 0.8, "USD").unwrap();
        approx(f.amount, 0.3);
        assert_eq!(f.currency, "USD");
    }
    #[test] fn a_repeated_total_books_nothing() { assert!(fee_delta(Uuid::nil(), "COINBASE", 0.8, 0.8, "USD").is_none()); }
    #[test] fn a_lower_total_books_a_negative_correction() {
        approx(fee_delta(Uuid::nil(), "OKX", 0.8, 0.6, "USDT").unwrap().amount, -0.2);
    }
    #[test] fn the_fee_id_names_the_total_reached() {
        let id = Uuid::nil();
        assert_eq!(fee_delta(id, "OKX", 0.0, 0.6, "USDT").unwrap().fee_id, format!("OKX-{id}-USDT-0.6"));
    }

    #[test] fn usdt_fee_on_btcusdt_is_quote() { assert_eq!(fee_effect("USDT", "BTCUSDT", "CRYPTO", "USDT"), FeeEffect::Quote); }
    #[test] fn btc_fee_on_btcusdt_is_base() { assert_eq!(fee_effect("btc", "BTCUSDT", "CRYPTO", "USDT"), FeeEffect::Base); }
    #[test] fn base_fee_with_separator_symbol() {
        assert_eq!(fee_effect("BTC", "BTC-USD", "CRYPTO", "USD"), FeeEffect::Base);
        assert_eq!(fee_effect("EUR", "EUR.USD", "FX", "USD"), FeeEffect::Base);
    }
    #[test] fn bnb_fee_on_btcusdt_is_other() { assert_eq!(fee_effect("BNB", "BTCUSDT", "CRYPTO", "USDT"), FeeEffect::Other); }
    #[test] fn usd_commission_on_cad_stock_is_other() { assert_eq!(fee_effect("USD", "SHOP", "EQUITY", "CAD"), FeeEffect::Other); }
    #[test] fn equity_never_has_a_base() { assert_eq!(fee_effect("AAPL", "AAPL", "EQUITY", "USD"), FeeEffect::Other); }
    #[test] fn serde_names_are_lowercase() {
        assert_eq!(serde_json::to_string(&FeeEffect::Quote).unwrap(), "\"quote\"");
        assert_eq!(serde_json::from_str::<FeeEffect>("\"base\"").unwrap(), FeeEffect::Base);
    }

    mod db {
        use super::super::*;
        use crate::domain::orders::commands::ExecutionReport;
        use crate::execution::process_execution_report;
        use crate::execution::tests::{remove_seeded, seeded_order};
        use crate::handlers::tests::test_pool;

        fn charge(fee_id: &str, amount: f64, currency: &str) -> FeeCharge {
            FeeCharge { fee_id: fee_id.into(), execution_id: Some("x1".into()), amount, currency: currency.into() }
        }

        async fn quote_ccy(pool: &PgPool, instrument_id: &str) -> String {
            sqlx::query_scalar("SELECT currency FROM instrument WHERE id::text = $1")
                .bind(instrument_id).fetch_one(pool).await.expect("instrument currency")
        }

        /// Turn the seeded equity into a crypto pair `<BASE><QUOTE>`; returns the base.
        async fn make_pair(pool: &PgPool, instrument_id: &str) -> String {
            let quote = quote_ccy(pool, instrument_id).await;
            let base = format!("ZB{}", &Uuid::new_v4().simple().to_string()[..8]).to_uppercase();
            sqlx::query("UPDATE instrument SET asset_class = 'CRYPTO', symbol = $2 WHERE id::text = $1")
                .bind(instrument_id).bind(format!("{base}{quote}")).execute(pool).await.expect("make pair");
            base
        }

        async fn counts(pool: &PgPool, order_id: Uuid) -> (i64, i64) {
            let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM order_fee WHERE order_id = $1")
                .bind(order_id).fetch_one(pool).await.unwrap();
            let events: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM order_event WHERE order_id = $1 AND event_type = 'order_fee_charged'")
                .bind(order_id).fetch_one(pool).await.unwrap();
            (rows, events)
        }

        async fn position(pool: &PgPool, portfolio_id: Uuid, instrument_id: &str) -> Option<(f64, f64, f64)> {
            sqlx::query_as(
                "SELECT net_qty::float8, avg_cost::float8, realized_pnl::float8 FROM position \
                 WHERE portfolio_id = $1 AND instrument_id = $2")
                .bind(portfolio_id).bind(instrument_id).fetch_optional(pool).await.unwrap()
        }

        fn fill() -> ExecutionReport {
            ExecutionReport::Fill { execution_id: Uuid::new_v4().to_string(), fill_qty: 1.0, fill_price: 100.0, venue: "TEST".into() }
        }

        #[tokio::test]
        #[ignore = "needs a live Postgres; run with --ignored"]
        async fn a_quote_fee_books_event_row_and_pnl() {
            let pool = test_pool().await;
            let (order_id, portfolio_id, instrument_id) = seeded_order(&pool, 0).await;
            let ccy = quote_ccy(&pool, &instrument_id).await;
            let booked = book_fee(&pool, None, order_id, charge("f1", 1.25, &ccy), "test").await;
            let c = counts(&pool, order_id).await;
            let pos = position(&pool, portfolio_id, &instrument_id).await;
            remove_seeded(&pool, order_id).await;
            assert_eq!(booked.unwrap(), FeeBooked::Booked);
            assert_eq!(c, (1, 1));
            assert_eq!(pos.unwrap().2, -1.25);
        }

        #[tokio::test]
        #[ignore = "needs a live Postgres; run with --ignored"]
        async fn the_same_fee_id_books_once() {
            let pool = test_pool().await;
            let (order_id, _, instrument_id) = seeded_order(&pool, 0).await;
            let ccy = quote_ccy(&pool, &instrument_id).await;
            let first = book_fee(&pool, None, order_id, charge("f1", 1.0, &ccy), "test").await;
            let second = book_fee(&pool, None, order_id, charge("f1", 1.0, &ccy), "test").await;
            let c = counts(&pool, order_id).await;
            remove_seeded(&pool, order_id).await;
            assert_eq!(first.unwrap(), FeeBooked::Booked);
            assert_eq!(second.unwrap(), FeeBooked::Duplicate);
            assert_eq!(c, (1, 1));
        }

        #[tokio::test]
        #[ignore = "needs a live Postgres; run with --ignored"]
        async fn a_fee_on_a_filled_order_still_books() {
            let pool = test_pool().await;
            let (order_id, _, instrument_id) = seeded_order(&pool, 0).await;
            let filled = process_execution_report(&pool, order_id, fill(), "test", None).await;
            let status: String = sqlx::query_scalar("SELECT status FROM order_state WHERE order_id = $1")
                .bind(order_id).fetch_one(&pool).await.unwrap();
            let ccy = quote_ccy(&pool, &instrument_id).await;
            let booked = book_fee(&pool, None, order_id, charge("f1", 0.5, &ccy), "test").await;
            remove_seeded(&pool, order_id).await;
            filled.unwrap();
            assert_eq!(status, "filled");
            assert_eq!(booked.unwrap(), FeeBooked::Booked);
        }

        #[tokio::test]
        #[ignore = "needs a live Postgres; run with --ignored"]
        async fn a_fee_for_an_unknown_order_is_reported() {
            let pool = test_pool().await;
            let order_id = Uuid::new_v4();
            let booked = book_fee(&pool, None, order_id, charge("f1", 1.0, "USD"), "test").await;
            assert_eq!(booked.unwrap(), FeeBooked::UnknownOrder);
            assert_eq!(counts(&pool, order_id).await, (0, 0));
        }

        #[tokio::test]
        #[ignore = "needs a live Postgres; run with --ignored"]
        async fn rebuild_matches_incremental_with_fees() {
            let pool = test_pool().await;
            let (order_id, portfolio_id, instrument_id) = seeded_order(&pool, 0).await;
            let base = make_pair(&pool, &instrument_id).await;
            let quote = quote_ccy(&pool, &instrument_id).await;
            book_fee(&pool, None, order_id, charge("q1", 0.3, &quote), "test").await.unwrap();
            process_execution_report(&pool, order_id, fill(), "test", None).await.unwrap();
            book_fee(&pool, None, order_id, charge("b1", 0.01, &base), "test").await.unwrap();
            let incremental = position(&pool, portfolio_id, &instrument_id).await;
            let rebuilt = positions::rebuild_positions(&pool).await;
            let after = position(&pool, portfolio_id, &instrument_id).await;
            remove_seeded(&pool, order_id).await;
            rebuilt.unwrap();
            let (q, c, p) = incremental.expect("incremental position");
            assert!((q - 0.99).abs() < 1e-9 && (c - 100.0).abs() < 1e-9 && (p + 0.3).abs() < 1e-9, "{q} {c} {p}");
            assert_eq!(after, incremental);
        }

        #[tokio::test]
        #[ignore = "needs a live Postgres; run with --ignored"]
        async fn booked_fee_total_sums_one_currency() {
            let pool = test_pool().await;
            let (order_id, _, _) = seeded_order(&pool, 0).await;
            for f in [charge("u1", 1.5, "USD"), charge("u2", 2.25, "USD"), charge("n1", 9.0, "BNB")] {
                book_fee(&pool, None, order_id, f, "test").await.unwrap();
            }
            let total = booked_fee_total(&pool, order_id, "USD").await;
            remove_seeded(&pool, order_id).await;
            assert_eq!(total.unwrap(), 3.75);
        }
    }
}
