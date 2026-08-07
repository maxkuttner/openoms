//! `FixBrokerAdapter` — the outbound half. Implements the existing
//! [`BrokerAdapter`] trait so order routing in `handlers.rs` is unchanged: the
//! registry hands back this adapter and calls `submit_order`/`cancel_order`.
//!
//! Sends go out through QuickFIX's `send_to_target` (a free function keyed by
//! `SessionId`, needing no initiator handle). The ack comes back asynchronously
//! on a session thread via [`FixApplication`], which resolves the oneshot this
//! adapter registered under the order's `ClOrdID`.

use std::time::Duration;

use quickfix::{send_to_target, FieldMap, Message, SessionId};
use tokio::sync::oneshot;
use tracing::{info, warn};
use uuid::Uuid;

use crate::adapters::{
    BrokerAdapter, BrokerError, BrokerOrderRequest, BrokerOrderResponse,
};
use crate::fix::app::{PendingAcks, PendingSnapshots, PendingStatus, SnapshotWaiter};
use crate::fix::dialect::{tag, FixDialect};
use crate::recon_orders::{BrokerOpenOrder, BrokerOrderState};

use std::sync::Arc;

/// How long `submit_order` waits for the broker's ack before giving up. The order
/// isn't lost — a late ack/fill still flows through `process_execution_report`;
/// this only bounds the synchronous submit call.
const ACK_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a mass-status snapshot / single order-status waits for the broker before
/// giving up. A venue that doesn't answer 35=AF (e.g. an older gateway) simply causes
/// `open_orders` to time out and reconciliation to be skipped that cycle.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10);
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);

pub struct FixBrokerAdapter {
    dialect: Arc<dyn FixDialect>,
    pending: PendingAcks,
    snapshots: PendingSnapshots,
    statuses: PendingStatus,
    /// Optional REST adapter that owns the order-reconciliation reads (open orders,
    /// order status). Some venues route orders over FIX but expose no reliable FIX
    /// message for them — Binance Spot FIX being the case, where open orders come from
    /// its REST API. When present, recon reads forward here; when absent (e.g. IBKR),
    /// they use the native FIX 35=AF/35=H path.
    recon_delegate: Option<Arc<dyn BrokerAdapter>>,
    // SessionId components — rebuilt per send rather than held, so no FFI handle
    // crosses threads.
    begin_string: String,
    sender_comp_id: String,
    target_comp_id: String,
}

impl FixBrokerAdapter {
    pub fn new(
        dialect: Arc<dyn FixDialect>,
        pending: PendingAcks,
        snapshots: PendingSnapshots,
        statuses: PendingStatus,
        recon_delegate: Option<Arc<dyn BrokerAdapter>>,
        sender_comp_id: String,
        target_comp_id: String,
    ) -> Self {
        Self {
            begin_string: dialect.begin_string().to_string(),
            dialect,
            pending,
            snapshots,
            statuses,
            recon_delegate,
            sender_comp_id,
            target_comp_id,
        }
    }

    fn session_id(&self) -> Result<SessionId, BrokerError> {
        SessionId::try_new(&self.begin_string, &self.sender_comp_id, &self.target_comp_id, "")
            .map_err(|e| BrokerError::NotConfigured(format!("bad FIX session id: {e}")))
    }

    /// Send a pre-built message to this session's target. Synchronous so the non-Send
    /// QuickFIX `Message`/`SessionId` never live across an `.await`.
    fn send(&self, msg: Message) -> Result<(), BrokerError> {
        let session = self.session_id()?;
        send_to_target(msg, &session).map_err(|e| BrokerError::Network(format!("FIX send failed: {e}")))
    }

    /// Build and send the `NewOrderSingle`. Kept synchronous so the non-`Send`
    /// QuickFIX `Message`/`SessionId` never live across the ack `.await`.
    fn send_new_order(&self, req: &BrokerOrderRequest) -> Result<(), BrokerError> {
        let mut msg = self.dialect.build_new_order_single(req)?;
        msg.set_field(tag::CL_ORD_ID, req.order_id.as_str())
            .map_err(|e| BrokerError::Network(format!("set ClOrdID: {e}")))?;
        self.send(msg)
    }

    /// Build and send an `OrderCancelRequest`. Synchronous for the same reason as
    /// [`send_new_order`].
    fn send_cancel(&self, external_order_id: &str, symbol: &str) -> Result<(), BrokerError> {
        self.send(self.dialect.build_cancel(external_order_id, symbol)?)
    }
}

/// The FIX request/reply dance shared by `open_orders` and `order_status`: the waiter
/// is already registered (its `rx` passed in). Send the request; on send error or
/// reply timeout, run `cleanup` to drop the orphaned waiter. Keeps the non-`Send`
/// send synchronous (the `Message` is built and sent before any `.await`).
async fn await_correlated_reply<T>(
    rx: oneshot::Receiver<T>,
    sent: Result<(), BrokerError>,
    timeout: Duration,
    timeout_msg: &'static str,
    cleanup: impl FnOnce(),
) -> Result<T, BrokerError> {
    if let Err(e) = sent {
        cleanup();
        return Err(e);
    }
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(v)) => Ok(v),
        _ => {
            cleanup();
            Err(BrokerError::Network(timeout_msg.into()))
        }
    }
}

#[async_trait::async_trait]
impl BrokerAdapter for FixBrokerAdapter {
    async fn submit_order(&self, req: &BrokerOrderRequest) -> Result<BrokerOrderResponse, BrokerError> {
        // Register the ack waiter before sending, so a fast ack can't race us.
        let (tx, rx) = oneshot::channel();
        {
            let mut map = self
                .pending
                .lock()
                .map_err(|_| BrokerError::Network("pending-acks lock poisoned".into()))?;
            map.insert(req.order_id.clone(), tx);
        }

        info!(venue = self.dialect.venue(), order_id = %req.order_id, symbol = %req.symbol, "submitting order over FIX");
        // All non-Send QuickFIX handles live and die inside this sync call.
        if let Err(e) = self.send_new_order(req) {
            self.pending.lock().ok().and_then(|mut m| m.remove(&req.order_id));
            return Err(e);
        }

        match tokio::time::timeout(ACK_TIMEOUT, rx).await {
            Ok(Ok(Ok(external_order_id))) => Ok(BrokerOrderResponse { external_order_id }),
            Ok(Ok(Err(reason))) => Err(BrokerError::BrokerRejected(reason)),
            // Sender dropped without sending — should not happen, treat as network.
            Ok(Err(_)) => Err(BrokerError::Network("FIX ack channel dropped".into())),
            Err(_) => {
                self.pending.lock().ok().and_then(|mut m| m.remove(&req.order_id));
                warn!(venue = self.dialect.venue(), order_id = %req.order_id, "FIX ack timed out");
                Err(BrokerError::Network("timed out waiting for FIX ack".into()))
            }
        }
    }

    async fn cancel_order(&self, external_order_id: &str, symbol: &str) -> Result<(), BrokerError> {
        // Sync send; the resulting ExecutionReport (Canceled) arrives asynchronously
        // and is applied by process_execution_report, mirroring the WS cancel path.
        self.send_cancel(external_order_id, symbol)
    }

    /// Snapshot open orders. Forwards to the REST recon delegate when present (Binance
    /// FIX), else uses an Order Mass Status Request (35=AF). Register the collector
    /// before sending so a fast reply can't race us, then await the batch.
    async fn open_orders(&self) -> Result<Vec<BrokerOpenOrder>, BrokerError> {
        if let Some(d) = &self.recon_delegate {
            return d.open_orders().await;
        }
        let req_id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        {
            let mut map = self
                .snapshots
                .lock()
                .map_err(|_| BrokerError::Network("snapshots lock poisoned".into()))?;
            map.insert(req_id.clone(), SnapshotWaiter { acc: Vec::new(), tx });
        }

        let sent = self.dialect.build_mass_status_request(&req_id).and_then(|m| self.send(m));
        await_correlated_reply(
            rx,
            sent,
            SNAPSHOT_TIMEOUT,
            "timed out waiting for FIX mass-status snapshot",
            || {
                self.snapshots.lock().ok().and_then(|mut m| m.remove(&req_id));
            },
        )
        .await
    }

    /// Resolve one order's state via an Order Status Request (35=H), correlated on
    /// `ClOrdID` (our order UUID).
    async fn order_status(
        &self,
        client_order_id: &str,
        external_order_id: &str,
        symbol: &str,
    ) -> Result<BrokerOrderState, BrokerError> {
        if let Some(d) = &self.recon_delegate {
            return d.order_status(client_order_id, external_order_id, symbol).await;
        }
        let (tx, rx) = oneshot::channel();
        {
            let mut map = self
                .statuses
                .lock()
                .map_err(|_| BrokerError::Network("statuses lock poisoned".into()))?;
            map.insert(client_order_id.to_string(), tx);
        }

        let sent = self
            .dialect
            .build_order_status_request(client_order_id, external_order_id, symbol)
            .and_then(|m| self.send(m));
        await_correlated_reply(
            rx,
            sent,
            STATUS_TIMEOUT,
            "timed out waiting for FIX order status",
            || {
                self.statuses.lock().ok().and_then(|mut m| m.remove(client_order_id));
            },
        )
        .await
    }
}
