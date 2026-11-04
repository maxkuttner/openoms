//! The order engine and the contract connectors speak to it.
//!
//! Connectors (the broker streams and FIX sessions) only *report* what a venue
//! did, as [`ExecEvent`]s on a [`GatewayMsg`] channel. The engine is the one place
//! that books: it turns each event into the state machine's `ExecutionReport`s,
//! against what the OMS has already booked, and applies them. A connector holds
//! no database pool and no bus — so the booking rules live in one place, and a
//! new connector cannot get them subtly wrong.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use sqlx::PgPool;
use tokio::sync::mpsc;
use tracing::{debug, error, info};
use uuid::Uuid;

use crate::adapters::BrokerAdapter;
use crate::domain::orders::commands::ExecutionReport;
use crate::event_bus::EventBus;
use crate::execution::process_execution_report;
use crate::recon_orders::{
    increment_price, missing_qty, oms_fill_state, run_order_reconcile, BrokerOrderOutcome, BrokerOrderState,
};

/// Connectors wait (rather than drop) once this many messages are queued: a
/// stalled engine slows the streams down, it never loses a fill.
pub const ENGINE_CHANNEL_CAPACITY: usize = 4096;

/// Where an event came from: the live session, or a snapshot the engine's
/// reconcile fetched. (Drop copy joins when the first connector has one.)
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExecSource {
    Session,
    Snapshot,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExecKind {
    /// One fill with the venue's execution id (Alpaca, Binance, Bybit, FIX).
    Fill { execution_id: String, qty: f64, price: f64 },
    /// Cumulative order state; books whatever exceeds what the OMS has (Coinbase, OKX).
    Cumulative { cum_qty: f64, avg_px: f64 },
    /// The order is closed at the venue, with its final cumulative state.
    Terminal(BrokerOrderState),
    Canceled { reason: Option<String> },
    Rejected { reason: String },
}

/// What a venue did to one order.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecEvent {
    pub order_id: Uuid,
    pub venue: String,
    /// Labels the events this books (`"binance"`, `"ibkr"`, …).
    pub actor: &'static str,
    pub source: ExecSource,
    pub kind: ExecKind,
}

impl ExecEvent {
    /// A live-session event from a transport that already speaks the state
    /// machine's `ExecutionReport` (Alpaca, Binance, FIX).
    pub fn from_report(order_id: Uuid, venue: &str, actor: &'static str, report: ExecutionReport) -> Self {
        let kind = match report {
            ExecutionReport::Fill { execution_id, fill_qty, fill_price, .. } => {
                ExecKind::Fill { execution_id, qty: fill_qty, price: fill_price }
            }
            ExecutionReport::Canceled { reason, .. } => ExecKind::Canceled { reason },
            ExecutionReport::Reject { reason, .. } => ExecKind::Rejected { reason },
        };
        Self { order_id, venue: venue.to_string(), actor, source: ExecSource::Session, kind }
    }
}

/// Everything a connector sends the engine.
pub enum GatewayMsg {
    Exec(ExecEvent),
    /// Reconcile this broker's open orders now (a reconnect, or the heartbeat).
    /// Carries the adapter because the diff asks the broker about each order it
    /// resolves, not just for one snapshot.
    Reconcile { broker_code: &'static str, actor: &'static str, adapter: Arc<dyn BrokerAdapter> },
}

/// Whether [`reports_for`] needs the OMS's booked fill state for this event.
pub fn needs_booked_state(kind: &ExecKind) -> bool {
    matches!(kind, ExecKind::Cumulative { .. } | ExecKind::Terminal(_))
}

/// What the engine applies for one event, given what the OMS has booked.
/// Cumulative and terminal states book only the quantity the OMS is missing,
/// priced against its own booked average — which is what makes a repeated or
/// replayed update safe without execution-id dedupe. A terminal state books that
/// missing fill before the close, so a partial-then-canceled order keeps its trade.
pub fn reports_for(ev: &ExecEvent, booked_cum: f64, booked_avg: Option<f64>) -> Vec<ExecutionReport> {
    let venue = ev.venue.clone();
    match &ev.kind {
        ExecKind::Fill { execution_id, qty, price } => vec![ExecutionReport::Fill {
            execution_id: execution_id.clone(),
            fill_qty: *qty,
            fill_price: *price,
            venue,
        }],
        ExecKind::Cumulative { cum_qty, avg_px } => missing_qty(booked_cum, *cum_qty)
            .map(|fill_qty| ExecutionReport::Fill {
                execution_id: format!("{venue}-{}-{cum_qty}", ev.order_id),
                fill_qty,
                fill_price: increment_price(booked_cum, booked_avg, *cum_qty, *avg_px),
                venue: venue.clone(),
            })
            .into_iter()
            .collect(),
        ExecKind::Terminal(st) => {
            let mut reports: Vec<ExecutionReport> = missing_qty(booked_cum, st.executed_qty)
                .map(|fill_qty| ExecutionReport::Fill {
                    execution_id: format!("recon-{}", ev.order_id),
                    fill_qty,
                    fill_price: increment_price(booked_cum, booked_avg, st.executed_qty, st.avg_px),
                    venue: venue.clone(),
                })
                .into_iter()
                .collect();
            match st.outcome {
                BrokerOrderOutcome::Canceled => {
                    reports.push(ExecutionReport::Canceled { reason: None, venue: Some(venue) })
                }
                BrokerOrderOutcome::Rejected => {
                    reports.push(ExecutionReport::Reject { reason: "rejected".to_string(), venue: Some(venue) })
                }
                // a fill above closes a filled order; a working one has nothing to
                // close, but what it executed is still booked
                BrokerOrderOutcome::Filled | BrokerOrderOutcome::Working => {}
            }
            reports
        }
        ExecKind::Canceled { reason } => vec![ExecutionReport::Canceled { reason: reason.clone(), venue: Some(venue) }],
        ExecKind::Rejected { reason } => vec![ExecutionReport::Reject { reason: reason.clone(), venue: Some(venue) }],
    }
}

/// How a connector reaches the engine.
#[derive(Clone)]
pub struct EngineHandle(mpsc::Sender<GatewayMsg>);

impl EngineHandle {
    pub fn new() -> (Self, mpsc::Receiver<GatewayMsg>) {
        let (tx, rx) = mpsc::channel(ENGINE_CHANNEL_CAPACITY);
        (Self(tx), rx)
    }

    pub async fn exec(&self, ev: ExecEvent) {
        let order_id = ev.order_id;
        if self.0.send(GatewayMsg::Exec(ev)).await.is_err() {
            error!(order_id = %order_id, "order engine is gone; execution event not booked");
        }
    }

    pub async fn reconcile(&self, broker_code: &'static str, actor: &'static str, adapter: Arc<dyn BrokerAdapter>) {
        if self.0.send(GatewayMsg::Reconcile { broker_code, actor, adapter }).await.is_err() {
            error!(broker = broker_code, "order engine is gone; reconcile skipped");
        }
    }
}

/// The only place that books. Applies execution events one at a time, in the
/// order connectors sent them; runs each reconcile on its own task (it waits on
/// the broker's REST API, which must not hold up fills), at most one per broker.
pub async fn run(mut rx: mpsc::Receiver<GatewayMsg>, pool: PgPool, bus: EventBus) {
    info!("order engine: started");
    let reconciling: Arc<Mutex<HashSet<&'static str>>> = Arc::default();
    while let Some(msg) = rx.recv().await {
        match msg {
            GatewayMsg::Exec(ev) => apply_exec(&pool, &bus, &ev).await,
            GatewayMsg::Reconcile { broker_code, actor, adapter } => {
                if !reconciling.lock().unwrap().insert(broker_code) {
                    debug!(broker = broker_code, "order engine: reconcile already running, skipped");
                    continue;
                }
                let done = Reconciling { set: reconciling.clone(), broker_code };
                let (pool, bus) = (pool.clone(), bus.clone());
                tokio::spawn(async move {
                    let _done = done;
                    run_order_reconcile(&pool, &*adapter, broker_code, actor, Some(&bus)).await;
                });
            }
        }
    }
    info!("order engine: every connector is gone, stopping");
}

/// Frees a broker's reconcile slot however the task ends, panics included.
struct Reconciling {
    set: Arc<Mutex<HashSet<&'static str>>>,
    broker_code: &'static str,
}

impl Drop for Reconciling {
    fn drop(&mut self) {
        if let Ok(mut set) = self.set.lock() {
            set.remove(self.broker_code);
        }
    }
}

/// Book one event: read what the OMS has booked when the event is cumulative or
/// terminal, then apply each report the plan yields.
pub async fn apply_exec(pool: &PgPool, bus: &EventBus, ev: &ExecEvent) {
    let (booked_cum, booked_avg) = if needs_booked_state(&ev.kind) {
        match oms_fill_state(pool, ev.order_id).await {
            Some(state) => state,
            None => return, // unknown or unreadable order, already logged
        }
    } else {
        (0.0, None)
    };
    apply_planned(pool, Some(bus), ev, booked_cum, booked_avg).await;
}

/// Apply [`reports_for`] in order against a booked state the caller already has
/// (recon reads it for its whole working set at once).
pub(crate) async fn apply_planned(
    pool: &PgPool,
    bus: Option<&EventBus>,
    ev: &ExecEvent,
    booked_cum: f64,
    booked_avg: Option<f64>,
) {
    if matches!(&ev.kind, ExecKind::Terminal(st) if st.outcome == BrokerOrderOutcome::Working) {
        info!(order_id = %ev.order_id, "order engine: order still working at broker, no terminal action");
    }
    for report in reports_for(ev, booked_cum, booked_avg) {
        if let Err(e) = process_execution_report(pool, ev.order_id, report, ev.actor, bus).await {
            error!(order_id = %ev.order_id, source = ?ev.source, error = %e, "order engine: apply failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recon_orders::{BrokerOrderOutcome, BrokerOrderState};

    fn ev(kind: ExecKind) -> ExecEvent {
        ExecEvent { order_id: Uuid::nil(), venue: "TEST".into(), actor: "test", source: ExecSource::Session, kind }
    }

    fn fill(r: &ExecutionReport) -> (f64, f64) {
        match r {
            ExecutionReport::Fill { fill_qty, fill_price, .. } => (*fill_qty, *fill_price),
            other => panic!("expected a fill, got {other:?}"),
        }
    }

    #[test]
    fn fill_becomes_one_fill_report() {
        let reports = reports_for(&ev(ExecKind::Fill { execution_id: "x1".into(), qty: 2.0, price: 10.0 }), 0.0, None);
        assert_eq!(
            reports,
            vec![ExecutionReport::Fill { execution_id: "x1".into(), fill_qty: 2.0, fill_price: 10.0, venue: "TEST".into() }]
        );
    }

    #[test]
    fn cumulative_books_only_the_missing_quantity() {
        let reports = reports_for(&ev(ExecKind::Cumulative { cum_qty: 1.0, avg_px: 105.0 }), 0.5, Some(100.0));
        assert_eq!(reports.len(), 1);
        let (qty, price) = fill(&reports[0]);
        assert!((qty - 0.5).abs() < 1e-9);
        assert!((price - 110.0).abs() < 1e-9);
    }

    #[test]
    fn repeated_cumulative_update_books_nothing() {
        assert!(reports_for(&ev(ExecKind::Cumulative { cum_qty: 1.0, avg_px: 105.0 }), 1.0, Some(105.0)).is_empty());
    }

    #[test]
    fn terminal_cancel_books_missing_fill_first() {
        let st = BrokerOrderState { outcome: BrokerOrderOutcome::Canceled, executed_qty: 0.5, avg_px: 10.0 };
        let reports = reports_for(&ev(ExecKind::Terminal(st)), 0.2, Some(10.0));
        assert_eq!(reports.len(), 2);
        assert!((fill(&reports[0]).0 - 0.3).abs() < 1e-9);
        assert!(matches!(reports[1], ExecutionReport::Canceled { .. }));
    }

    #[test]
    fn terminal_filled_with_nothing_missing_is_empty() {
        let st = BrokerOrderState { outcome: BrokerOrderOutcome::Filled, executed_qty: 1.0, avg_px: 10.0 };
        assert!(reports_for(&ev(ExecKind::Terminal(st)), 1.0, Some(10.0)).is_empty());
    }

    #[test]
    fn terminal_working_is_empty() {
        let st = BrokerOrderState { outcome: BrokerOrderOutcome::Working, executed_qty: 0.0, avg_px: 0.0 };
        assert!(reports_for(&ev(ExecKind::Terminal(st)), 0.0, None).is_empty());
    }

    /// Same as recon always did: the venue still lists it working, but what it
    /// executed beyond the OMS's books is booked anyway.
    #[test]
    fn terminal_working_still_books_what_executed() {
        let st = BrokerOrderState { outcome: BrokerOrderOutcome::Working, executed_qty: 0.4, avg_px: 10.0 };
        let reports = reports_for(&ev(ExecKind::Terminal(st)), 0.1, Some(10.0));
        assert_eq!(reports.len(), 1);
        assert!((fill(&reports[0]).0 - 0.3).abs() < 1e-9);
    }

    #[test]
    fn needs_booked_state_only_for_cumulative_and_terminal() {
        let st = BrokerOrderState { outcome: BrokerOrderOutcome::Filled, executed_qty: 1.0, avg_px: 1.0 };
        assert!(needs_booked_state(&ExecKind::Cumulative { cum_qty: 1.0, avg_px: 1.0 }));
        assert!(needs_booked_state(&ExecKind::Terminal(st)));
        assert!(!needs_booked_state(&ExecKind::Fill { execution_id: "x".into(), qty: 1.0, price: 1.0 }));
        assert!(!needs_booked_state(&ExecKind::Canceled { reason: None }));
        assert!(!needs_booked_state(&ExecKind::Rejected { reason: "r".into() }));
    }

    /// Blocks `open_orders` until released, counting calls — a broker whose
    /// snapshot call is slow.
    struct SlowBroker {
        calls: std::sync::atomic::AtomicUsize,
        release: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl BrokerAdapter for SlowBroker {
        async fn submit_order(
            &self,
            _: &crate::adapters::BrokerOrderRequest,
        ) -> Result<crate::adapters::BrokerOrderResponse, crate::adapters::BrokerError> {
            unreachable!("recon never submits")
        }
        async fn cancel_order(&self, _: &str, _: &str) -> Result<(), crate::adapters::BrokerError> {
            unreachable!("recon never cancels")
        }
        async fn open_orders(&self) -> Result<Vec<crate::recon_orders::BrokerOpenOrder>, crate::adapters::BrokerError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.release.notified().await;
            Ok(Vec::new())
        }
    }

    /// A pool that refuses at once, so recon's working-set query fails fast
    /// (and reads as an empty working set) instead of waiting on a connect timeout.
    fn refusing_pool() -> PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(100))
            .connect_lazy("postgres://127.0.0.1:1/never")
            .unwrap()
    }

    #[tokio::test]
    async fn second_reconcile_for_a_busy_broker_is_dropped() {
        let broker = Arc::new(SlowBroker { calls: Default::default(), release: tokio::sync::Notify::new() });
        let (engine, rx) = EngineHandle::new();
        tokio::spawn(run(rx, refusing_pool(), crate::event_bus::EventBus::new()));
        let calls = || broker.calls.load(std::sync::atomic::Ordering::SeqCst);
        let settle = || tokio::time::sleep(std::time::Duration::from_millis(300));

        engine.reconcile("FAKE", "fake", broker.clone()).await;
        engine.reconcile("FAKE", "fake", broker.clone()).await;
        settle().await;
        assert_eq!(calls(), 1, "a second recon for a broker still reconciling is dropped");

        broker.release.notify_waiters();
        settle().await;
        engine.reconcile("FAKE", "fake", broker.clone()).await;
        settle().await;
        assert_eq!(calls(), 2, "once it finished, the next one runs");
        broker.release.notify_waiters();
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn messages_sent_before_the_engine_starts_are_applied() {
        use crate::execution::tests::{remove_seeded, seeded_order};
        let pool = crate::handlers::tests::test_pool().await;
        let (order_id, _, _) = seeded_order(&pool, 0).await;
        let (engine, rx) = EngineHandle::new();

        engine
            .exec(ExecEvent {
                order_id,
                venue: "TEST".into(),
                actor: "engine-test",
                source: ExecSource::Session,
                kind: ExecKind::Fill { execution_id: "e1".into(), qty: 1.0, price: 100.0 },
            })
            .await;
        tokio::spawn(run(rx, pool.clone(), crate::event_bus::EventBus::new()));

        let mut status = String::new();
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            status = sqlx::query_scalar("SELECT status FROM order_state WHERE order_id = $1")
                .bind(order_id)
                .fetch_one(&pool)
                .await
                .unwrap();
            if status == "filled" {
                break;
            }
        }
        remove_seeded(&pool, order_id).await;
        assert_eq!(status, "filled");
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn cumulative_update_twice_books_once() {
        use crate::execution::tests::{remove_seeded, seeded_order};
        let pool = crate::handlers::tests::test_pool().await;
        let (order_id, _, _) = seeded_order(&pool, 0).await;
        let bus = crate::event_bus::EventBus::new();
        let ev = ExecEvent {
            order_id,
            venue: "TEST".into(),
            actor: "engine-test",
            source: ExecSource::Session,
            kind: ExecKind::Cumulative { cum_qty: 0.5, avg_px: 100.0 },
        };

        apply_exec(&pool, &bus, &ev).await;
        apply_exec(&pool, &bus, &ev).await;
        let cum: f64 = sqlx::query_scalar("SELECT cum_qty::float8 FROM order_state WHERE order_id = $1")
            .bind(order_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        let partials: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM order_event WHERE order_id = $1 AND event_type = 'order_partially_filled'",
        )
        .bind(order_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        remove_seeded(&pool, order_id).await;

        assert!((cum - 0.5).abs() < 1e-9);
        assert_eq!(partials, 1);
    }
}
