//! The contract between the core and the connectors.
//!
//! Connectors (the broker streams and FIX sessions) only *report* what a venue
//! did, as [`ExecEvent`]s on a [`GatewayMsg`] channel. The OMS's order engine is
//! the one place that books them. Everything here is plain data, so the line
//! could become a network link later.

use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::error;
use uuid::Uuid;

use crate::adapter::BrokerAdapter;

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
    pub fn from_report(order_id: Uuid, venue: &str, actor: &'static str, report: VenueReport) -> Self {
        let kind = match report {
            VenueReport::Fill { execution_id, fill_qty, fill_price, .. } => {
                ExecKind::Fill { execution_id, qty: fill_qty, price: fill_price }
            }
            VenueReport::Canceled { reason, .. } => ExecKind::Canceled { reason },
            VenueReport::Reject { reason, .. } => ExecKind::Rejected { reason },
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

/// One order the broker currently reports as open, normalized across transports.
#[derive(Debug, Clone, PartialEq)]
pub struct BrokerOpenOrder {
    /// Our order UUID (the client order id we sent).
    pub client_order_id: String,
    pub symbol: String,
    pub executed_qty: f64,
}

/// How the broker says a no-longer-open order finished (from `order_status`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BrokerOrderOutcome {
    Filled,
    Canceled,
    Rejected,
    /// Still open at the broker (shouldn't happen on the ResolveTerminal path).
    Working,
}

/// One order's current state at the broker, resolved on demand for an OMS order that
/// dropped out of the open-order snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct BrokerOrderState {
    pub outcome: BrokerOrderOutcome,
    pub executed_qty: f64,
    pub avg_px: f64,
}

/// What a venue reported about one order, before the engine weighs it against
/// what the OMS has booked. The same shape as the order state machine's
/// `ExecutionReport`, owned here so the gateway needs nothing of the core.
#[derive(Debug, Clone, PartialEq)]
pub enum VenueReport {
    Fill { execution_id: String, fill_qty: f64, fill_price: f64, venue: String },
    Reject { reason: String, venue: Option<String> },
    Canceled { reason: Option<String>, venue: Option<String> },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_maps_to_kind() {
        let id = Uuid::nil();
        let fill = VenueReport::Fill { execution_id: "x".into(), fill_qty: 1.5, fill_price: 9.0, venue: "V".into() };
        let canceled = VenueReport::Canceled { reason: Some("user".into()), venue: Some("V".into()) };
        let reject = VenueReport::Reject { reason: "no funds".into(), venue: None };
        let kind = |r| ExecEvent::from_report(id, "V", "t", r).kind;
        assert_eq!(kind(fill), ExecKind::Fill { execution_id: "x".into(), qty: 1.5, price: 9.0 });
        assert_eq!(kind(canceled), ExecKind::Canceled { reason: Some("user".into()) });
        assert_eq!(kind(reject), ExecKind::Rejected { reason: "no funds".into() });
    }
}
