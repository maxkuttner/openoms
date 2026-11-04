//! The order engine and the contract connectors speak to it.
//!
//! Connectors (the broker streams and FIX sessions) only *report* what a venue
//! did, as [`ExecEvent`]s on a [`GatewayMsg`] channel. The engine is the one place
//! that books: it turns each event into the state machine's `ExecutionReport`s,
//! against what the OMS has already booked, and applies them. A connector holds
//! no database pool and no bus — so the booking rules live in one place, and a
//! new connector cannot get them subtly wrong.

use std::sync::Arc;

use uuid::Uuid;

use crate::adapters::BrokerAdapter;
use crate::domain::orders::commands::ExecutionReport;
use crate::recon_orders::{increment_price, missing_qty, BrokerOrderOutcome, BrokerOrderState};

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
}
