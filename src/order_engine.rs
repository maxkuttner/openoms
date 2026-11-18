//! The order engine: the one place that books what venues report.
//!
//! Connectors (the broker streams and FIX sessions) only *report* what a venue
//! did, as venue events on a [`GatewayMsg`] channel (through their
//! connection's reconciler). The engine applies them one at a time through
//! ingest, so order state has a single writer. A connector holds no database
//! pool and no bus, so the booking rules live in one place, and a new
//! connector cannot get them subtly wrong.

use sqlx::PgPool;
use tokio::sync::mpsc;
use tracing::{debug, error, info};
use uuid::Uuid;

use crate::domain::orders::commands::ExecutionReport;
use crate::event_bus::EventBus;
use crate::execution::process_execution_report;
use gateway::contract::GatewayMsg;

/// The only place that books. Applies messages one at a time, in the order
/// connectors sent them.
pub async fn run(mut rx: mpsc::Receiver<GatewayMsg>, pool: PgPool, bus: EventBus) {
    info!("order engine: started");
    while let Some(msg) = rx.recv().await {
        match msg {
            GatewayMsg::Reject { order_id, reason, actor } => {
                guarded(order_id, async {
                    let report = ExecutionReport::Reject { reason, venue: None };
                    if let Err(e) = process_execution_report(&pool, order_id, report, actor, Some(&bus)).await {
                        error!(order_id = %order_id, error = %e, "order engine: reject not applied");
                    }
                })
                .await;
            }
            GatewayMsg::Venue { connection, event, instruments, reply } => {
                // The provider rides on the message (the reconciler owns one per
                // connection), so the engine keeps no per-connection state.
                let mut outcome = None;
                guarded(Uuid::nil(), async {
                    let res = crate::ingest::apply::ingest(&pool, &bus, &connection, event, instruments.as_deref()).await;
                    match &res {
                        Ok(ingested) => debug!(connection = %connection, ?ingested, "order engine: venue event applied"),
                        Err(e) => error!(connection = %connection, error = %e, "order engine: venue event not applied"),
                    }
                    outcome = Some(
                        res.map(|i| gateway::contract::IngestOutcome {
                            fill_gap: i.fill_gap,
                            duplicate: i.decision == crate::ingest::decide::Decision::Duplicate,
                            broken: matches!(
                                i.decision,
                                crate::ingest::decide::Decision::Break(_) | crate::ingest::decide::Decision::AttachToBreak
                            ),
                        })
                        .map_err(|e| e.to_string()),
                    );
                })
                .await;
                if let Some(reply) = reply {
                    let _ = reply.send(outcome.unwrap_or_else(|| Err("ingest panicked".into())));
                }
            }
        }
    }
    info!("order engine: every connector is gone, stopping");
}

/// Run one event's booking, containing a panic to that event. The engine is the
/// only consumer of every connector's events; if a bug in one booking ended its
/// task, every later fill from every venue would be dropped. Returns whether the
/// booking completed.
async fn guarded(order_id: Uuid, booking: impl std::future::Future<Output = ()>) -> bool {
    use futures_util::FutureExt;
    match std::panic::AssertUnwindSafe(booking).catch_unwind().await {
        Ok(()) => true,
        Err(_) => {
            error!(order_id = %order_id, "order engine: booking panicked; event skipped, engine continues");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gateway::contract::EngineHandle;

    /// One event whose booking panics must cost that event, not the engine:
    /// with the engine gone, every later fill from every connector is lost.
    #[tokio::test]
    async fn a_panicking_booking_does_not_stop_the_engine() {
        let survived = guarded(Uuid::nil(), async { panic!("booking bug") }).await;
        assert!(!survived);
        assert!(guarded(Uuid::nil(), async {}).await);
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn messages_sent_before_the_engine_starts_are_applied() {
        use crate::execution::tests::{remove_seeded, seeded_order};
        let pool = crate::handlers::tests::test_pool().await;
        let (order_id, _, _) = seeded_order(&pool, 0).await;
        let (engine, rx) = EngineHandle::new();

        engine.reject(order_id, "no funds".into(), "engine-test").await;
        tokio::spawn(run(rx, pool.clone(), crate::event_bus::EventBus::new()));

        let mut status = String::new();
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            status = sqlx::query_scalar("SELECT status FROM order_state WHERE order_id = $1")
                .bind(order_id)
                .fetch_one(&pool)
                .await
                .unwrap();
            if status == "rejected" {
                break;
            }
        }
        remove_seeded(&pool, order_id).await;
        assert_eq!(status, "rejected");
    }
}
