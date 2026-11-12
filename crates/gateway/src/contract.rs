//! The contract between the core and the connectors.
//!
//! Connectors (the broker streams and FIX sessions) only *report* what a venue
//! did, as [`ExecEvent`]s on a [`GatewayMsg`] channel. The OMS's order engine is
//! the one place that books them. Everything here is plain data, so the line
//! could become a network link later.

use std::sync::Arc;

use chrono::{DateTime, Utc};

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
    /// The venue holds the order under this id: routes a `submitted` order.
    Ack { venue_order_id: String },
    /// One fee the venue charged (positive) or rebated (negative); never touches fills.
    Fee { fee_id: String, execution_id: Option<String>, amount: f64, currency: String },
    /// Running total of the fees charged so far on this order, in one currency;
    /// the engine books the difference from what it already has.
    FeeTotal { total: f64, currency: String },
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
    Reconcile { broker_code: &'static str, environment: &'static str, actor: &'static str, adapter: Arc<dyn BrokerAdapter> },
}

/// What a followed venue account (a co-located strategy's) did. The OMS did
/// not place these orders; it only watches them, books their fills, and may
/// cancel them.
#[derive(Debug, Clone, PartialEq)]
pub enum FollowEvent {
    /// The account stream is (re)connected: time to fill any gap.
    Connected,
    Opened { venue_order_id: String, client_order_id: String, symbol: String, buy: bool, qty: f64, price: f64 },
    /// `"canceled"`, `"rejected"`, `"expired"` or `"filled"`.
    Closed { venue_order_id: String, outcome: &'static str },
    Fill(ExternalFill),
}

/// One fill on a followed account, by the venue's trade id.
#[derive(Debug, Clone, PartialEq)]
pub struct ExternalFill {
    pub trade_id: String,
    pub venue_order_id: String,
    /// The strategy's own client id (empty when the venue's history omits it).
    pub client_order_id: String,
    pub symbol: String,
    pub buy: bool,
    pub qty: f64,
    pub price: f64,
    /// `None` means the venue did not report a fee; not a zero fee.
    pub fee_amount: Option<f64>,
    pub fee_currency: Option<String>,
    /// Venue execution time; `None` only when the source omitted it.
    pub executed_at: Option<DateTime<Utc>>,
}

/// Where a follow stream reports, tagged with the strategy it follows.
#[derive(Clone)]
pub struct FollowHandle(pub mpsc::Sender<(String, FollowEvent)>);

impl FollowHandle {
    pub async fn send(&self, strategy_code: &str, event: FollowEvent) {
        if self.0.send((strategy_code.to_string(), event)).await.is_err() {
            error!(strategy = strategy_code, "follow sink is gone; event dropped");
        }
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

    /// Reconcile one connection (`broker_code` in `environment`) through `adapter`.
    pub async fn reconcile(&self, broker_code: &'static str, environment: &'static str, actor: &'static str, adapter: Arc<dyn BrokerAdapter>) {
        if self.0.send(GatewayMsg::Reconcile { broker_code, environment, actor, adapter }).await.is_err() {
            error!(broker = broker_code, "order engine is gone; reconcile skipped");
        }
    }
}

/// One order the broker currently reports as open, normalized across transports.
#[derive(Debug, Clone, PartialEq)]
pub struct BrokerOpenOrder {
    /// Our order UUID (the client order id we sent).
    pub client_order_id: String,
    /// The venue's own id for the order.
    pub venue_order_id: String,
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
