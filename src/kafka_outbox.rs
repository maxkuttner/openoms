//! Kafka outbox: every committed order event waits in `kafka_outbox` (filled by a
//! trigger on `order_event`, see migration 0033) until Kafka has acked it. The
//! bridge reads it oldest-first, so an outage only delays events, never drops them.

use sqlx::{PgPool, Row};
use uuid::Uuid;

pub struct PositionSnapshot {
    pub net_qty: f64,
    pub avg_cost: f64,
    pub realized_pnl: f64,
}

/// One queued event with the order context its Kafka records need.
pub struct OutboxRow {
    pub global_position: i64,
    pub order_id: Uuid,
    pub event_id: Uuid,
    pub event_type: String,
    /// `order_event.payload_json`, verbatim
    pub payload: serde_json::Value,
    pub portfolio_id: Option<Uuid>,
    pub instrument_id: Option<String>,
    pub side: Option<String>,
    /// the position row for (portfolio, instrument) as it is now
    pub position: Option<PositionSnapshot>,
}

pub async fn fetch_batch(pool: &PgPool, limit: i64) -> Result<Vec<OutboxRow>, sqlx::Error> {
    let rows = sqlx::query(
        r#"
        SELECT
            o.global_position, e.order_id, e.event_id, e.event_type, e.payload_json,
            s.portfolio_id, s.instrument_id, s.side,
            p.net_qty::double precision      AS net_qty,
            p.avg_cost::double precision     AS avg_cost,
            p.realized_pnl::double precision AS realized_pnl
        FROM kafka_outbox o
        JOIN order_event e USING (global_position)
        LEFT JOIN order_state s ON s.order_id = e.order_id
        LEFT JOIN position p ON p.portfolio_id = s.portfolio_id AND p.instrument_id = s.instrument_id
        ORDER BY o.global_position
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let net_qty: Option<f64> = r.get("net_qty");
            OutboxRow {
                global_position: r.get("global_position"),
                order_id: r.get("order_id"),
                event_id: r.get("event_id"),
                event_type: r.get("event_type"),
                payload: r.get("payload_json"),
                portfolio_id: r.get("portfolio_id"),
                instrument_id: r.get("instrument_id"),
                side: r.get("side"),
                position: net_qty.map(|net_qty| PositionSnapshot {
                    net_qty,
                    avg_cost: r.get("avg_cost"),
                    realized_pnl: r.get("realized_pnl"),
                }),
            }
        })
        .collect())
}

/// Remove acked events from the outbox. Returns how many rows went.
pub async fn ack(pool: &PgPool, positions: &[i64]) -> Result<u64, sqlx::Error> {
    let r = sqlx::query("DELETE FROM kafka_outbox WHERE global_position = ANY($1)")
        .bind(positions)
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}

pub async fn pending_count(pool: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM kafka_outbox").fetch_one(pool).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_store::{NewOrderEvent, OrderEventStore};
    use crate::execution::tests::{remove_seeded, seeded_order};
    use crate::handlers::tests::test_pool;

    fn event(event_type: &str) -> NewOrderEvent {
        NewOrderEvent {
            event_id: Uuid::new_v4().to_string(),
            event_type: event_type.to_string(),
            actor: "outbox-test".to_string(),
            payload: serde_json::json!({ "probe": event_type }),
            correlation_id: None,
            causation_id: None,
            schema_version: 0,
        }
    }

    async fn rows_for(pool: &PgPool, order_id: Uuid) -> Vec<OutboxRow> {
        fetch_batch(pool, 10_000)
            .await
            .expect("fetch batch")
            .into_iter()
            .filter(|r| r.order_id == order_id)
            .collect()
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn appended_event_is_queued_with_its_order_context() {
        let pool = test_pool().await;
        let (order_id, portfolio_id, _) = seeded_order(&pool, 0).await;
        let ev = event("order_routed");
        let r = OrderEventStore::new(pool.clone())
            .append_events(order_id, 0, std::slice::from_ref(&ev))
            .await
            .expect("append");

        let rows = rows_for(&pool, order_id).await;
        let acked = ack(&pool, &r.global_positions).await.expect("ack");
        remove_seeded(&pool, order_id).await;

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].global_position, r.global_positions[0]);
        assert_eq!(rows[0].event_type, "order_routed");
        assert_eq!(rows[0].portfolio_id, Some(portfolio_id));
        assert_eq!(rows[0].payload, ev.payload);
        assert_eq!(acked, 1);
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn rolled_back_append_queues_nothing() {
        let pool = test_pool().await;
        let order_id = Uuid::new_v4();
        let mut tx = pool.begin().await.expect("begin");
        OrderEventStore::new(pool.clone())
            .append_events_in_tx(&mut tx, order_id, 0, &[event("order_submitted")])
            .await
            .expect("append");
        tx.rollback().await.expect("rollback");

        assert!(rows_for(&pool, order_id).await.is_empty());
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn ack_removes_only_named_rows() {
        let pool = test_pool().await;
        let order_id = Uuid::new_v4();
        let r = OrderEventStore::new(pool.clone())
            .append_events(order_id, 0, &[event("order_submitted"), event("order_routed")])
            .await
            .expect("append");

        ack(&pool, &r.global_positions[..1]).await.expect("ack first");
        let left: Vec<i64> = rows_for(&pool, order_id).await.iter().map(|r| r.global_position).collect();
        ack(&pool, &r.global_positions[1..]).await.expect("ack second");

        assert_eq!(left, vec![r.global_positions[1]]);
    }
}
