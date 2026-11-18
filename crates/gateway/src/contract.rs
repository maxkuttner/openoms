//! The contract between the core and the connectors.
//!
//! Connectors (the broker streams and FIX sessions) only *report* what a venue
//! did, as venue events that reach the engine as [`GatewayMsg::Venue`]. The
//! OMS's order engine is the one place that books them. Everything here is plain data, so the line
//! could become a network link later.

use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tracing::error;
use uuid::Uuid;

use crate::adapter::InstrumentProvider;
use crate::venue_event::VenueEvent;

/// Connectors wait (rather than drop) once this many messages are queued: a
/// stalled engine slows the streams down, it never loses a fill.
pub const ENGINE_CHANNEL_CAPACITY: usize = 4096;

/// Everything a connector sends the engine.
pub enum GatewayMsg {
    /// The venue refused one of our orders at submission, synchronously (no
    /// venue event will report it): reject it.
    Reject { order_id: Uuid, reason: String, actor: &'static str },
    /// What a venue connection reported, by connection code. `instruments`
    /// lets ingest resolve a symbol the OMS does not know yet; `reply`, when
    /// set, gets the outcome once the event is applied (pulls wait on it).
    Venue {
        connection: String,
        event: VenueEvent,
        instruments: Option<Arc<dyn InstrumentProvider>>,
        reply: Option<VenueReply>,
    },
}

/// What ingesting one venue event came to, as far as the sender cares.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct IngestOutcome {
    /// The venue reports more executed than the OMS has booked: pull the fills.
    pub fill_gap: bool,
    /// Already booked; nothing was written.
    pub duplicate: bool,
    /// Not booked: recorded as a break for a person to resolve.
    pub broken: bool,
}

/// Where the engine answers a `GatewayMsg::Venue`; `Err` is the ingest error, as text.
pub type VenueReply = oneshot::Sender<Result<IngestOutcome, String>>;

/// How a connector reaches the engine.
#[derive(Clone)]
pub struct EngineHandle(mpsc::Sender<GatewayMsg>);

impl EngineHandle {
    pub fn new() -> (Self, mpsc::Receiver<GatewayMsg>) {
        let (tx, rx) = mpsc::channel(ENGINE_CHANNEL_CAPACITY);
        (Self(tx), rx)
    }

    /// Reject a stored order the venue refused at submission.
    pub async fn reject(&self, order_id: Uuid, reason: String, actor: &'static str) {
        if self.0.send(GatewayMsg::Reject { order_id, reason, actor }).await.is_err() {
            error!(order_id = %order_id, "order engine is gone; reject not booked");
        }
    }

    /// Fire and forget: nobody hears how it went.
    pub async fn venue(&self, connection: &str, event: VenueEvent) {
        self.venue_send(connection, event, None, None).await;
    }

    pub async fn venue_send(
        &self,
        connection: &str,
        event: VenueEvent,
        instruments: Option<Arc<dyn InstrumentProvider>>,
        reply: Option<VenueReply>,
    ) {
        let msg = GatewayMsg::Venue { connection: connection.to_string(), event, instruments, reply };
        if self.0.send(msg).await.is_err() {
            error!(connection, "order engine is gone; venue event dropped");
        }
    }

    /// Apply one event and wait until it is booked (or failed).
    pub async fn venue_ack(
        &self,
        connection: &str,
        event: VenueEvent,
        instruments: Option<Arc<dyn InstrumentProvider>>,
    ) -> Result<IngestOutcome, String> {
        let (tx, rx) = oneshot::channel();
        self.venue_send(connection, event, instruments, Some(tx)).await;
        rx.await.unwrap_or_else(|_| Err("order engine dropped the venue event".into()))
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
