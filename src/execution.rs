//! Broker-agnostic execution-report apply path.
//!
//! Both broker fill streams (Alpaca, Binance, …) normalize their venue-native
//! trade updates into an [`ExecutionReport`] keyed by our order UUID, then call
//! [`process_execution_report`]. It rehydrates the order aggregate, applies the
//! report through the state machine, and — in a single transaction — updates
//! `order_state`, maintains the position projection, and appends the resulting
//! events (the outbox trigger queues them for Kafka). On commit it publishes to
//! the bus's order lane.

use chrono::{SubsecRound, Utc};
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
use crate::positions;

/// Signals an optimistic-concurrency version conflict — the caller retries.
#[derive(Debug)]
pub(crate) struct ConcurrencyRetry {
    pub(crate) expected: i64,
    pub(crate) actual: i64,
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
    order_id: Uuid,
    report: ExecutionReport,
    actor: &str,
    bus: Option<&EventBus>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    retry_on_conflict(order_id, "execution", || apply_once(pool, order_id, report.clone(), actor, bus)).await
}

/// Run `attempt` up to five times while it fails with [`ConcurrencyRetry`],
/// backing off a little more each time.
pub(crate) async fn retry_on_conflict<T, F, Fut>(
    order_id: Uuid,
    what: &str,
    mut attempt_once: F,
) -> Result<T, Box<dyn std::error::Error + Send + Sync>>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, Box<dyn std::error::Error + Send + Sync>>>,
{
    const MAX_ATTEMPTS: u32 = 5;
    for attempt in 1..=MAX_ATTEMPTS {
        match attempt_once().await {
            Ok(v) => return Ok(v),
            Err(e) if e.downcast_ref::<ConcurrencyRetry>().is_some() => {
                if attempt == MAX_ATTEMPTS {
                    return Err(e);
                }
                warn!(order_id = %order_id, attempt, "{what}: {e}, retrying");
                tokio::time::sleep(std::time::Duration::from_millis(50 * attempt as u64)).await;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

/// The order's current state as `order_state` holds it, or `None` when there is
/// no such order.
fn is_fill_event(e: &crate::domain::orders::events::OrderDomainEvent) -> bool {
    matches!(e.payload, OrderEventPayload::OrderPartiallyFilled { .. } | OrderEventPayload::OrderFilled { .. })
}

pub(crate) async fn load_state(
    conn: &mut sqlx::PgConnection,
    order_id: Uuid,
) -> Result<Option<OrderAggregateState>, Box<dyn std::error::Error + Send + Sync>> {
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
            status, resume_to_status, version, origin
        FROM order_state
        WHERE order_id = $1
        "#,
    )
    .bind(order_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else { return Ok(None) };

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
        observed: row.get::<String, _>("origin") == "EXTERNAL",
    };
    Ok(Some(state))
}

async fn apply_once(
    pool: &PgPool,
    order_id: Uuid,
    report: ExecutionReport,
    actor: &str,
    bus: Option<&EventBus>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let event_store = OrderEventStore::new(pool.clone());
    let mut tx = pool.begin().await?;

    let state = match load_state(&mut tx, order_id).await? {
        Some(s) => s,
        // TODO: If the OMS is supposed to act as a projection for colocated quoting engines,
        //  trading algorithms, etc., we should be able to handle the case where the order is
        //  not found in the DB.
        None => {
            warn!(order_id = %order_id, "execution: order not found in DB, skipping");
            return Ok(());
        }
    };

    // an observed (external) order's status is the venue's: it still takes
    // fills until booked cum reaches quantity
    let takes_fill = state.observed
        && matches!(report, ExecutionReport::Fill { .. })
        && state.cum_qty < state.original_qty;
    if state.status.is_terminal() && !takes_fill {
        warn!(order_id = %order_id, status = state.status.as_str(), "execution: order already in terminal state, skipping duplicate event");
        return Ok(());
    }

    let expected_version = state.version;
    let aggregate = OrderAggregate::from_state(state);
    let metadata = EventMetadata {
        event_id: Uuid::new_v4().to_string(),
        timestamp: Utc::now(),
        actor: actor.to_string(),
        occurred_at: None,
    };

    let cmd = ReceiveExecutionReport {
        order_id: order_id.to_string(),
        report,
    };

    let mut events = match aggregate.decide(OrderCommand::ReceiveExecutionReport(cmd), metadata) {
        Ok(evts) => evts,
        Err(rejection) => {
            warn!(order_id = %order_id, reason = %rejection.message, "execution: state machine rejected execution report, skipping");
            return Ok(());
        }
    };
    // An OMS fill without a venue time trades at its booking time. Store it as
    // occurred_at (at the DB's microsecond precision), so the key the position
    // compares is the key a rebuild orders by.
    for event in events.iter_mut().filter(|e| is_fill_event(e)) {
        event.occurred_at.get_or_insert(event.timestamp.trunc_subsecs(6));
    }

    let mut applied = aggregate;
    for event in &events {
        applied.apply(event).map_err(|e| format!("apply failed: {e:?}"))?;
    }

    let new_state = applied.state.ok_or("missing aggregate state after apply")?;
    let trade_time = |e: &crate::domain::orders::events::OrderDomainEvent| e.occurred_at.unwrap_or(e.timestamp);
    let last_trade = events.iter().filter(|e| is_fill_event(e)).map(trade_time).max();

    sqlx::query(
        r#"
        UPDATE order_state
        SET
            status          = $2,
            leaves_qty      = $3,
            cum_qty         = $4,
            avg_px          = $5,
            version         = $6,
            last_trade_at   = GREATEST(last_trade_at, $8),
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
    .bind(last_trade)
    .execute(&mut *tx)
    .await?;

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
                occurred_at: e.occurred_at,
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

    // Maintain the position projection from any fill events, in the same tx so it
    // commits atomically with the order_state update + event append above. The
    // events go first: a late fill rebuilds the position from the log.
    let portfolio_uuid = Uuid::parse_str(&new_state.portfolio_id)?;
    let mut any_fill = false;
    for event in &events {
        let (fill_qty, fill_price) = match &event.payload {
            OrderEventPayload::OrderPartiallyFilled { fill_qty, fill_price, .. }
            | OrderEventPayload::OrderFilled { fill_qty, fill_price, .. } => (*fill_qty, *fill_price),
            _ => continue,
        };
        any_fill = true;
        let rebuilt = positions::persist_fill(
            &mut *tx,
            portfolio_uuid,
            &new_state.instrument_id,
            new_state.side,
            fill_qty,
            fill_price,
            trade_time(event),
        )
        .await?;
        if rebuilt {
            break;
        }
    }

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


    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
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
    pub(crate) async fn seeded_order(pool: &PgPool, version: i64) -> (Uuid, Uuid, String) {
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

    /// Remove everything `seeded_order` and the code under test wrote, so runs
    /// don't pile up held positions or unpublished outbox rows in the shared dev
    /// DB. The order's events stay: `order_event` is append-only and refuses deletes.
    pub(crate) async fn remove_seeded(pool: &PgPool, order_id: Uuid) {
        let mut tx = pool.begin().await.expect("begin cleanup");
        sqlx::query("DELETE FROM order_fee WHERE order_id = $1")
            .bind(order_id)
            .execute(&mut *tx)
            .await
            .expect("delete order_fee rows");
        sqlx::query(
            "DELETE FROM kafka_outbox WHERE global_position IN \
             (SELECT global_position FROM order_event WHERE order_id = $1)",
        )
        .bind(order_id)
        .execute(&mut *tx)
        .await
        .expect("delete outbox rows");
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
            // last: deleting the position above queued one more change
            ("DELETE FROM kafka_position_outbox WHERE portfolio_id = CAST($1 AS uuid)", portfolio_id.to_string()),
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

        let applied = process_execution_report(&pool, order_id, fill(), "bus-test", Some(&bus)).await;
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

    /// An external order the venue reported Filled before its fills were
    /// booked still books fills until booked cum reaches quantity, and keeps
    /// its Filled status; past quantity the engine skips it as before.
    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn engine_fill_on_observed_filled_order_books_until_qty() {
        let pool = test_pool().await;
        let (order_id, _, _) = seeded_order(&pool, 0).await;
        sqlx::query(
            "UPDATE order_state SET origin = 'EXTERNAL', status = 'filled', original_qty = 2, \
             leaves_qty = 2, cum_qty = 0, avg_px = NULL WHERE order_id = $1",
        )
        .bind(order_id)
        .execute(&pool)
        .await
        .expect("make observed-filled");

        let mut results = Vec::new();
        for _ in 0..3 {
            results.push(process_execution_report(&pool, order_id, fill(), "obs-test", None).await.is_ok());
        }
        let (cum, status, order_trade, position_trade): (f64, String, Option<chrono::DateTime<Utc>>, Option<chrono::DateTime<Utc>>) =
            sqlx::query_as(
                "SELECT os.cum_qty::double precision, os.status, os.last_trade_at, p.last_trade_at \
                 FROM order_state os LEFT JOIN position p \
                   ON p.portfolio_id = os.portfolio_id AND p.instrument_id = os.instrument_id \
                 WHERE os.order_id = $1",
            )
            .bind(order_id)
            .fetch_one(&pool)
            .await
            .expect("read state");
        remove_seeded(&pool, order_id).await;

        assert_eq!(results, vec![true, true, true]);
        assert_eq!(cum, 2.0);
        assert_eq!(status, "filled");
        // a fill without a venue time trades at its booking time
        assert!(order_trade.is_some_and(|t| (Utc::now() - t).num_seconds().abs() < 60));
        assert_eq!(order_trade, position_trade);
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn failed_append_publishes_nothing() {
        let pool = test_pool().await;
        // order_state says version 1, the log says 0: the append is refused
        let (order_id, _, _) = seeded_order(&pool, 1).await;
        let bus = EventBus::new();
        let mut rx = bus.subscribe_order();

        let applied = apply_once(&pool, order_id, fill(), "bus-test", Some(&bus)).await;
        remove_seeded(&pool, order_id).await;

        let err = applied.expect_err("version conflict");

        assert!(err.downcast_ref::<ConcurrencyRetry>().is_some());
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }
}
