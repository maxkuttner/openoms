//! Kafka outbox: every committed order event waits in `kafka_outbox` (filled by a
//! trigger on `order_event`, see migration 0033) until Kafka has acked it. The
//! bridge reads it oldest-first, so an outage only delays events, never drops them.

use std::sync::Arc;

use sqlx::{PgPool, Row};
use tokio::sync::broadcast;
use tokio::time::{sleep, Duration};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::event_bus::OrderLaneEvent;

const BATCH_SIZE: i64 = 500;
/// Wait after a failed drain before trying again.
const RETRY_SECS: u64 = 5;
/// Backstop when the order lane is quiet: an event queued by a path that did not
/// publish on the bus is still picked up.
const IDLE_RECHECK_SECS: u64 = 30;

pub struct PositionSnapshot {
    pub net_qty: f64,
    pub avg_cost: f64,
    pub realized_pnl: f64,
}

/// One queued event with the order context its Kafka records need.
pub struct OutboxRow {
    pub global_position: i64,
    pub order_id: Uuid,
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
            o.global_position, e.order_id, e.event_type, e.payload_json,
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

#[derive(Clone)]
pub struct OutboxTopics {
    pub orders: String,
    pub fills: String,
    pub positions: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutboxRecord {
    pub topic: String,
    pub key: String,
    pub payload: String,
}

/// The Kafka records one event becomes: always one on the orders topic (the stored
/// payload, verbatim — the same message the direct publish used to send); a fill
/// adds one on fills and, when the position row exists, one on positions. Fills
/// key by order so they share its partition; positions key by what they describe,
/// so a compacted topic keeps the latest state per position.
pub fn records_for(row: &OutboxRow, topics: &OutboxTopics) -> Vec<OutboxRecord> {
    let order_key = row.order_id.to_string();
    let mut records = vec![OutboxRecord {
        topic: topics.orders.clone(),
        key: order_key.clone(),
        payload: row.payload.to_string(),
    }];
    if !matches!(row.event_type.as_str(), "order_partially_filled" | "order_filled") {
        return records;
    }

    records.push(OutboxRecord {
        topic: topics.fills.clone(),
        key: order_key,
        payload: serde_json::json!({
            "event": row.payload,
            "portfolio_id": row.portfolio_id,
            "instrument_id": row.instrument_id,
            "side": row.side,
        })
        .to_string(),
    });
    if let (Some(p), Some(portfolio_id), Some(instrument_id)) = (&row.position, row.portfolio_id, &row.instrument_id) {
        records.push(OutboxRecord {
            topic: topics.positions.clone(),
            key: format!("{portfolio_id}:{instrument_id}"),
            payload: serde_json::json!({
                "portfolio_id": portfolio_id,
                "instrument_id": instrument_id,
                "net_qty": p.net_qty,
                "avg_cost": p.avg_cost,
                "realized_pnl": p.realized_pnl,
                "global_position": row.global_position,
            })
            .to_string(),
        });
    }
    records
}

#[async_trait::async_trait]
pub trait OutboxSink: Send + Sync {
    /// Publish one record; Ok only once the broker has acked it.
    async fn send(&self, record: &OutboxRecord) -> Result<(), String>;
}

/// Send events in order and stop at the first failure, so nothing later in the
/// batch overtakes it. Returns the global positions whose records were ALL acked,
/// plus the failure, if any. An event that failed part-way is not acked: it is
/// re-sent whole next time, and its already-sent records arrive twice.
pub async fn deliver(batch: &[(i64, Vec<OutboxRecord>)], sink: &dyn OutboxSink) -> (Vec<i64>, Option<String>) {
    let mut acked = Vec::with_capacity(batch.len());
    for (global_position, records) in batch {
        for record in records {
            if let Err(e) = sink.send(record).await {
                return (acked, Some(e));
            }
        }
        acked.push(*global_position);
    }
    (acked, None)
}

/// Turns a stream of attempt outcomes into one log line per outage edge, so a
/// broker that stays down for an hour logs twice, not once per retry.
#[derive(Default)]
pub struct OutageLog {
    failed_attempts: u32,
}

#[derive(Debug, PartialEq)]
pub enum Transition {
    WentDown,
    Recovered { failed_attempts: u32 },
}

impl OutageLog {
    pub fn on_attempt(&mut self, ok: bool) -> Option<Transition> {
        match (ok, self.failed_attempts) {
            (true, 0) => None,
            (true, failed_attempts) => {
                self.failed_attempts = 0;
                Some(Transition::Recovered { failed_attempts })
            }
            (false, n) => {
                self.failed_attempts = n + 1;
                (n == 0).then_some(Transition::WentDown)
            }
        }
    }
}

#[derive(Debug)]
pub enum DrainError {
    Db(sqlx::Error),
    Sink { published: usize, error: String },
}

/// Publish until the outbox is empty or the sink fails. Returns events published.
pub async fn drain(
    pool: &PgPool,
    sink: &dyn OutboxSink,
    topics: &OutboxTopics,
    batch_size: i64,
) -> Result<usize, DrainError> {
    let mut published = 0;
    loop {
        let rows = fetch_batch(pool, batch_size).await.map_err(DrainError::Db)?;
        if rows.is_empty() {
            return Ok(published);
        }
        let batch: Vec<(i64, Vec<OutboxRecord>)> =
            rows.iter().map(|r| (r.global_position, records_for(r, topics))).collect();
        let (acked, failure) = deliver(&batch, sink).await;
        ack(pool, &acked).await.map_err(DrainError::Db)?;
        published += acked.len();
        if let Some(error) = failure {
            return Err(DrainError::Sink { published, error });
        }
    }
}

/// The Kafka bridge: drain the outbox whenever the order lane says something was
/// committed (or the idle timer fires), and retry on a timer while Kafka is down.
/// Runs for the life of the process; nothing on the order path waits on it.
pub async fn run(
    pool: PgPool,
    sink: Arc<dyn OutboxSink>,
    mut order_rx: broadcast::Receiver<OrderLaneEvent>,
    topics: OutboxTopics,
) -> ! {
    let mut outage = OutageLog::default();
    let mut lane_open = true;
    info!("kafka outbox: bridge started");
    loop {
        let caught_up = match drain(&pool, sink.as_ref(), &topics, BATCH_SIZE).await {
            Ok(published) => {
                if let Some(Transition::Recovered { failed_attempts }) = outage.on_attempt(true) {
                    info!(failed_attempts, published, "kafka outbox: Kafka recovered, caught up");
                } else if published > 0 {
                    debug!(published, "kafka outbox: published");
                }
                true
            }
            Err(DrainError::Sink { published, error }) => {
                if outage.on_attempt(false) == Some(Transition::WentDown) {
                    warn!(%error, published, "kafka outbox: Kafka down, events stay queued until it is back");
                }
                false
            }
            Err(DrainError::Db(e)) => {
                error!(error = %e, "kafka outbox: reading the outbox failed");
                false
            }
        };

        if !caught_up {
            sleep(Duration::from_secs(RETRY_SECS)).await;
            continue;
        }
        tokio::select! {
            r = order_rx.recv(), if lane_open => {
                if matches!(r, Err(broadcast::error::RecvError::Closed)) {
                    lane_open = false;
                }
            }
            _ = sleep(Duration::from_secs(IDLE_RECHECK_SECS)) => {}
        }
    }
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

    fn topics() -> OutboxTopics {
        OutboxTopics { orders: "orders".into(), fills: "fills".into(), positions: "positions".into() }
    }

    fn row(event_type: &str, position: Option<PositionSnapshot>) -> OutboxRow {
        OutboxRow {
            global_position: 42,
            order_id: Uuid::new_v4(),
            event_type: event_type.to_string(),
            payload: serde_json::json!({ "event_type": event_type, "payload": { "fill_qty": 1.0 } }),
            portfolio_id: Some(Uuid::new_v4()),
            instrument_id: Some("123".to_string()),
            side: Some("buy".to_string()),
            position,
        }
    }

    fn json(r: &OutboxRecord) -> serde_json::Value {
        serde_json::from_str(&r.payload).expect("payload is json")
    }

    #[test]
    fn routed_event_goes_to_orders_only() {
        let row = row("order_routed", None);
        let records = records_for(&row, &topics());
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].topic, "orders");
        assert_eq!(records[0].key, row.order_id.to_string());
        assert_eq!(json(&records[0]), row.payload);
    }

    #[test]
    fn fill_fans_out_to_orders_fills_positions() {
        let row = row("order_filled", Some(PositionSnapshot { net_qty: 2.5, avg_cost: 100.0, realized_pnl: 0.0 }));
        let records = records_for(&row, &topics());
        let names: Vec<&str> = records.iter().map(|r| r.topic.as_str()).collect();
        assert_eq!(names, ["orders", "fills", "positions"]);
        assert_eq!(json(&records[1])["portfolio_id"], row.portfolio_id.unwrap().to_string());
        assert_eq!(json(&records[1])["event"], row.payload);
        assert_eq!(records[2].key, format!("{}:123", row.portfolio_id.unwrap()));
        assert_eq!(json(&records[2])["net_qty"], 2.5);
        assert_eq!(json(&records[2])["global_position"], 42);
    }

    #[test]
    fn fill_without_position_row_skips_positions() {
        let records = records_for(&row("order_partially_filled", None), &topics());
        assert_eq!(records.len(), 2);
    }

    /// Records every send it accepts; from send number `fail_from` (0-based, counting
    /// attempts) on, refuses — until `fail_from` is cleared, which brings it back up.
    #[derive(Default)]
    struct FakeSink {
        sent: std::sync::Mutex<Vec<OutboxRecord>>,
        attempts: std::sync::atomic::AtomicUsize,
        fail_from: std::sync::Mutex<Option<usize>>,
    }

    impl FakeSink {
        fn failing_from(n: usize) -> Self {
            Self { fail_from: std::sync::Mutex::new(Some(n)), ..Default::default() }
        }
        fn sent(&self) -> Vec<OutboxRecord> {
            self.sent.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl OutboxSink for FakeSink {
        async fn send(&self, record: &OutboxRecord) -> Result<(), String> {
            let n = self.attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if matches!(*self.fail_from.lock().unwrap(), Some(f) if n >= f) {
                return Err("broker down".into());
            }
            self.sent.lock().unwrap().push(record.clone());
            Ok(())
        }
    }

    fn rec(tag: &str) -> OutboxRecord {
        OutboxRecord { topic: "orders".into(), key: tag.into(), payload: tag.into() }
    }

    #[tokio::test]
    async fn all_acked_when_sink_is_up() {
        let sink = FakeSink::default();
        let batch = vec![(1, vec![rec("a")]), (2, vec![rec("b")]), (3, vec![rec("c")])];
        let (acked, err) = deliver(&batch, &sink).await;
        assert_eq!(acked, vec![1, 2, 3]);
        assert!(err.is_none());
        assert_eq!(sink.sent(), vec![rec("a"), rec("b"), rec("c")]);
    }

    #[tokio::test]
    async fn stops_at_first_failed_event() {
        let sink = FakeSink::failing_from(1);
        let batch = vec![(1, vec![rec("a")]), (2, vec![rec("b")]), (3, vec![rec("c")])];
        let (acked, err) = deliver(&batch, &sink).await;
        assert_eq!(acked, vec![1]);
        assert!(err.is_some());
        assert_eq!(sink.attempts.load(std::sync::atomic::Ordering::SeqCst), 2, "event 3 must not be tried");
    }

    #[tokio::test]
    async fn partially_sent_event_is_not_acked() {
        let sink = FakeSink::failing_from(1);
        let (acked, err) = deliver(&[(1, vec![rec("orders"), rec("fills")])], &sink).await;
        assert!(acked.is_empty());
        assert!(err.is_some());
    }

    #[test]
    fn outage_log_reports_each_transition_once() {
        let mut log = OutageLog::default();
        let seen: Vec<Option<Transition>> =
            [true, false, false, false, true, true].into_iter().map(|ok| log.on_attempt(ok)).collect();
        assert_eq!(
            seen,
            vec![None, Some(Transition::WentDown), None, None, Some(Transition::Recovered { failed_attempts: 3 }), None]
        );
    }

    /// The spec's phase 2 success test: Kafka down, then up — no event missing,
    /// none out of order. Batch size 2 so catch-up has to loop. Drains the shared
    /// dev outbox, so other pending rows may interleave: only this order's
    /// records are checked.
    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn kafka_down_then_up_publishes_everything_in_order() {
        let pool = test_pool().await;
        let (order_id, _, _) = seeded_order(&pool, 0).await;
        let events: Vec<NewOrderEvent> = (0..5)
            .map(|i| NewOrderEvent { payload: serde_json::json!({ "seq": i }), ..event("order_routed") })
            .collect();
        OrderEventStore::new(pool.clone()).append_events(order_id, 0, &events).await.expect("append");
        let sink = FakeSink::failing_from(2);

        let down = drain(&pool, &sink, &topics(), 2).await;
        *sink.fail_from.lock().unwrap() = None;
        let up = drain(&pool, &sink, &topics(), 2).await;
        let left = rows_for(&pool, order_id).await;
        remove_seeded(&pool, order_id).await;

        assert!(matches!(down, Err(DrainError::Sink { .. })));
        assert!(up.is_ok());
        let key = order_id.to_string();
        let ids: Vec<String> = sink
            .sent()
            .iter()
            .filter(|r| r.key == key)
            .map(|r| serde_json::from_str::<serde_json::Value>(&r.payload).unwrap()["seq"].to_string())
            .collect();
        assert_eq!(ids, ["0", "1", "2", "3", "4"], "every event once, in append order");
        assert!(left.is_empty(), "outbox drained");
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
