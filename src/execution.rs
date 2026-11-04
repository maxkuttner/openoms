//! Broker-agnostic execution-report apply path.
//!
//! Both broker fill streams (Alpaca, Binance, …) normalize their venue-native
//! trade updates into an [`ExecutionReport`] keyed by our order UUID, then call
//! [`process_execution_report`]. It rehydrates the order aggregate, applies the
//! report through the state machine, and — in a single transaction — updates
//! `order_state`, maintains the position projection, and appends the resulting
//! events. On commit it publishes to the bus's order lane and to Kafka.

use chrono::Utc;
use sqlx::{PgPool, Row};
use tracing::{info, warn};
use uuid::Uuid;

use crate::domain::orders::aggregate::{EventMetadata, OrderAggregate};
use crate::domain::orders::commands::{ExecutionReport, OrderCommand, ReceiveExecutionReport};
use crate::domain::orders::events::OrderEventPayload;
use crate::domain::orders::state::OrderAggregateState;
use crate::event_bus::{publish_appended, EventBus, OrderLaneEvent};
use crate::event_store::{NewOrderEvent, OrderEventStore};
use crate::handlers::{parse_order_side, parse_order_status, parse_order_type, parse_time_in_force};
use crate::kafka::{publish_events, KafkaClient};
use crate::positions;

/// Signals an optimistic-concurrency version conflict — the caller retries.
#[derive(Debug)]
struct ConcurrencyRetry {
    expected: i64,
    actual: i64,
}
impl std::fmt::Display for ConcurrencyRetry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "version conflict (expected {}, actual {})", self.expected, self.actual)
    }
}
impl std::error::Error for ConcurrencyRetry {}

/// Apply one execution report to `order_id`, retrying on version conflicts. A
/// fast broker fill (Binance market orders fill instantly) can race the
/// OrderRouted version bump; re-read and re-apply rather than drop the fill.
pub async fn process_execution_report(
    pool: &PgPool,
    kafka: &Option<KafkaClient>,
    order_id: Uuid,
    report: ExecutionReport,
    actor: &str,
    bus: Option<&EventBus>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    const MAX_ATTEMPTS: u32 = 5;
    for attempt in 1..=MAX_ATTEMPTS {
        match apply_once(pool, kafka, order_id, report.clone(), actor, bus).await {
            Ok(()) => return Ok(()),
            Err(e) if e.downcast_ref::<ConcurrencyRetry>().is_some() => {
                if attempt == MAX_ATTEMPTS {
                    return Err(e);
                }
                warn!(order_id = %order_id, attempt, "execution: {e}, retrying");
                tokio::time::sleep(std::time::Duration::from_millis(50 * attempt as u64)).await;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

async fn apply_once(
    pool: &PgPool,
    kafka: &Option<KafkaClient>,
    order_id: Uuid,
    report: ExecutionReport,
    actor: &str,
    bus: Option<&EventBus>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let event_store = OrderEventStore::new(pool.clone());
    let mut tx = pool.begin().await?;

    let row = sqlx::query(
        r#"
        SELECT
            order_id, client_order_id, portfolio_id, account_id, instrument_id,
            side, order_type, time_in_force,
            limit_price::double precision AS limit_price,
            original_qty::double precision AS original_qty,
            leaves_qty::double precision AS leaves_qty,
            cum_qty::double precision AS cum_qty,
            avg_px::double precision AS avg_px,
            status, resume_to_status, version
        FROM order_state
        WHERE order_id = $1
        "#,
    )
    .bind(order_id)
    .fetch_optional(&mut *tx)
    .await?;

    let row = match row {
        Some(r) => r,
        // TODO: If the OMS is supposed to act as a projection for colocated quoting engines,
        //  trading algorithms, etc., we should be able to handle the case where the order is
        //  not found in the DB.
        None => {
            warn!(order_id = %order_id, "execution: order not found in DB, skipping");
            return Ok(());
        }
    };

    let current_status: &str = row.get("status");
    if matches!(current_status, "filled" | "rejected" | "canceled" | "expired") {
        warn!(order_id = %order_id, status = current_status, "execution: order already in terminal state, skipping duplicate event");
        return Ok(());
    }

    // Reuse parse helpers from handlers — map errors to strings for the boxed error type
    let side = parse_order_side(row.get("side")).map_err(|e| e.message.clone())?;
    let order_type = parse_order_type(row.get("order_type")).map_err(|e| e.message.clone())?;
    let time_in_force = parse_time_in_force(row.get("time_in_force")).map_err(|e| e.message.clone())?;
    let status = parse_order_status(row.get("status")).map_err(|e| e.message.clone())?;
    let resume_to_status = row
        .get::<Option<String>, _>("resume_to_status")
        .map(parse_order_status)
        .transpose()
        .map_err(|e| e.message.clone())?;

    let state = OrderAggregateState {
        order_id: row.get::<Uuid, _>("order_id").to_string(),
        client_order_id: row.get("client_order_id"),
        portfolio_id: row.get::<Uuid, _>("portfolio_id").to_string(),
        account_id: row.get::<Uuid, _>("account_id").to_string(),
        instrument_id: row.get("instrument_id"),
        side,
        order_type,
        time_in_force,
        limit_price: row.get("limit_price"),
        original_qty: row.get("original_qty"),
        leaves_qty: row.get("leaves_qty"),
        cum_qty: row.get("cum_qty"),
        avg_px: row.get("avg_px"),
        status,
        resume_to_status,
        version: row.get("version"),
    };

    let expected_version = state.version;
    let aggregate = OrderAggregate::from_state(state);
    let metadata = EventMetadata {
        event_id: Uuid::new_v4().to_string(),
        timestamp: Utc::now(),
        actor: actor.to_string(),
    };

    let cmd = ReceiveExecutionReport {
        order_id: order_id.to_string(),
        report,
    };

    let events = match aggregate.decide(OrderCommand::ReceiveExecutionReport(cmd), metadata) {
        Ok(evts) => evts,
        Err(rejection) => {
            warn!(order_id = %order_id, reason = %rejection.message, "execution: state machine rejected execution report, skipping");
            return Ok(());
        }
    };

    let mut applied = aggregate;
    for event in &events {
        applied.apply(event).map_err(|e| format!("apply failed: {e:?}"))?;
    }

    let new_state = applied.state.ok_or("missing aggregate state after apply")?;

    sqlx::query(
        r#"
        UPDATE order_state
        SET
            status          = $2,
            leaves_qty      = $3,
            cum_qty         = $4,
            avg_px          = $5,
            version         = $6,
            updated_at      = now()
        WHERE order_id = $1 AND version = $7
        "#,
    )
    .bind(order_id)
    .bind(new_state.status.as_str())
    .bind(new_state.leaves_qty)
    .bind(new_state.cum_qty)
    .bind(new_state.avg_px)
    .bind(new_state.version)
    .bind(expected_version)
    .execute(&mut *tx)
    .await?;

    // Maintain the position projection from any fill events, in the same tx so it
    // commits atomically with the order_state update + event append below.
    let portfolio_uuid = Uuid::parse_str(&new_state.portfolio_id)?;
    let mut any_fill = false;
    for event in &events {
        let (fill_qty, fill_price) = match &event.payload {
            OrderEventPayload::OrderPartiallyFilled { fill_qty, fill_price, .. }
            | OrderEventPayload::OrderFilled { fill_qty, fill_price, .. } => (*fill_qty, *fill_price),
            _ => continue,
        };
        any_fill = true;
        positions::persist_fill(
            &mut *tx,
            portfolio_uuid,
            &new_state.instrument_id,
            new_state.side,
            fill_qty,
            fill_price,
        )
        .await?;
    }

    let new_events: Vec<NewOrderEvent> = events
        .iter()
        .map(|e| {
            let payload = serde_json::to_value(e).unwrap_or_default();
            NewOrderEvent {
                event_id: e.event_id.clone(),
                event_type: e.event_type.as_str().to_string(),
                actor: e.actor.clone(),
                payload,
                correlation_id: None,
                causation_id: None,
                schema_version: 0,
            }
        })
        .collect();

    let appended = event_store
        .append_events_in_tx(&mut tx, order_id, expected_version, &new_events)
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            match e {
                crate::event_store::EventStoreError::Concurrency(c) => {
                    Box::new(ConcurrencyRetry { expected: c.expected, actual: c.actual })
                }
                other => format!("event store error: {other:?}").into(),
            }
        })?;

    tx.commit().await?;

    info!(order_id = %order_id, status = new_state.status.as_str(), "execution report applied");

    if let Some(bus) = bus {
        publish_appended(bus, order_id, &new_events, &appended);
        // quote feeds re-read the held set on this, to price newly held instruments
        if any_fill {
            bus.publish_order(OrderLaneEvent::PositionChanged {
                portfolio_id: portfolio_uuid,
                instrument_id: new_state.instrument_id.clone(),
            });
        }
    }

    publish_events(kafka.as_ref(), &order_id.to_string(), &events).await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::orders::commands::ExecutionReport;
    use crate::event_bus::{EventBus, OrderLaneEvent};
    use crate::handlers::tests::{seed_instrument, seed_order_state, seed_principal, test_pool};
    use tokio::sync::broadcast::error::TryRecvError;

    fn fill() -> ExecutionReport {
        ExecutionReport::Fill {
            execution_id: Uuid::new_v4().to_string(),
            fill_qty: 1.0,
            fill_price: 100.0,
            venue: "TEST".to_string(),
        }
    }

    /// `seed_order_state` writes `version = 1` with no events behind it, while the
    /// event store checks versions against the log. `version` picks which side
    /// of that check the order lands on. Returns (order, portfolio, instrument).
    async fn seeded_order(pool: &PgPool, version: i64) -> (Uuid, Uuid, String) {
        let (principal_id, _) = seed_principal(pool, "bus-fill").await;
        let instrument_id = seed_instrument(pool, "BUSFILL").await;
        let order_id = seed_order_state(pool, principal_id, instrument_id, "bus-fill").await;
        let portfolio_id: Uuid = sqlx::query_scalar(
            "UPDATE order_state SET version = $2 WHERE order_id = $1 RETURNING portfolio_id",
        )
        .bind(order_id)
        .bind(version)
        .fetch_one(pool)
        .await
        .expect("set version");
        (order_id, portfolio_id, instrument_id.to_string())
    }

    /// Remove everything `seeded_order` and the fill under test wrote, so runs
    /// don't pile up held positions in the shared dev DB. The order's events
    /// stay: `order_event` is append-only and refuses deletes.
    async fn remove_seeded(pool: &PgPool, order_id: Uuid) {
        let mut tx = pool.begin().await.expect("begin cleanup");
        let row = sqlx::query(
            "DELETE FROM order_state WHERE order_id = $1 \
             RETURNING principal_id, portfolio_id, account_id, broker_connection_code, instrument_id",
        )
        .bind(order_id)
        .fetch_one(&mut *tx)
        .await
        .expect("delete order_state");
        let portfolio_id: Uuid = row.get("portfolio_id");
        let instrument_id: String = row.get("instrument_id");
        for (sql, bind) in [
            ("DELETE FROM position WHERE portfolio_id = CAST($1 AS uuid)", portfolio_id.to_string()),
            ("DELETE FROM account WHERE id = CAST($1 AS uuid)", row.get::<Uuid, _>("account_id").to_string()),
            ("DELETE FROM broker_connection WHERE code = $1", row.get("broker_connection_code")),
            ("DELETE FROM portfolio WHERE id = CAST($1 AS uuid)", portfolio_id.to_string()),
            ("DELETE FROM principal WHERE id = CAST($1 AS uuid)", row.get::<Uuid, _>("principal_id").to_string()),
            ("DELETE FROM instrument WHERE id = CAST($1 AS bigint)", instrument_id),
        ] {
            sqlx::query(sql).bind(bind).execute(&mut *tx).await.expect(sql);
        }
        tx.commit().await.expect("commit cleanup");
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn fill_publishes_appended_then_position_changed() {
        let pool = test_pool().await;
        let (order_id, portfolio_id, instrument_id) = seeded_order(&pool, 0).await;
        let bus = EventBus::new();
        let mut rx = bus.subscribe_order();

        let applied = process_execution_report(&pool, &None, order_id, fill(), "bus-test", Some(&bus)).await;
        let mut got = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            got.push(ev);
        }
        // before any assert, so a failing run cleans up too
        remove_seeded(&pool, order_id).await;

        applied.expect("apply fill");
        let (last, appended) = got.split_last().expect("events published");
        assert!(!appended.is_empty());
        assert!(appended
            .iter()
            .all(|ev| matches!(ev, OrderLaneEvent::Appended { order_id: o, .. } if *o == order_id)));
        assert_eq!(*last, OrderLaneEvent::PositionChanged { portfolio_id, instrument_id });
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn failed_append_publishes_nothing() {
        let pool = test_pool().await;
        // order_state says version 1, the log says 0: the append is refused
        let (order_id, _, _) = seeded_order(&pool, 1).await;
        let bus = EventBus::new();
        let mut rx = bus.subscribe_order();

        let applied = apply_once(&pool, &None, order_id, fill(), "bus-test", Some(&bus)).await;
        remove_seeded(&pool, order_id).await;

        let err = applied.expect_err("version conflict");

        assert!(err.downcast_ref::<ConcurrencyRetry>().is_some());
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }
}
