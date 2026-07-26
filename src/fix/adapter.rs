//! `FixBrokerAdapter` — the outbound half. Implements the existing
//! [`BrokerAdapter`] trait so order routing in `handlers.rs` is unchanged: the
//! registry hands back this adapter and calls `submit_order`/`cancel_order`.
//!
//! Sends go out through QuickFIX's `send_to_target` (a free function keyed by
//! `SessionId`, needing no initiator handle). The ack comes back asynchronously
//! on a session thread via [`FixApplication`], which resolves the oneshot this
//! adapter registered under the order's `ClOrdID`.

use std::time::Duration;

use quickfix::{send_to_target, FieldMap, SessionId};
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::adapters::{
    BrokerAdapter, BrokerError, BrokerOrderRequest, BrokerOrderResponse,
};
use crate::fix::app::PendingAcks;
use crate::fix::dialect::{tag, FixDialect};

use std::sync::Arc;

/// How long `submit_order` waits for the broker's ack before giving up. The order
/// isn't lost — a late ack/fill still flows through `process_execution_report`;
/// this only bounds the synchronous submit call.
const ACK_TIMEOUT: Duration = Duration::from_secs(10);

pub struct FixBrokerAdapter {
    dialect: Arc<dyn FixDialect>,
    pending: PendingAcks,
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
        sender_comp_id: String,
        target_comp_id: String,
    ) -> Self {
        Self {
            begin_string: dialect.begin_string().to_string(),
            dialect,
            pending,
            sender_comp_id,
            target_comp_id,
        }
    }

    fn session_id(&self) -> Result<SessionId, BrokerError> {
        SessionId::try_new(&self.begin_string, &self.sender_comp_id, &self.target_comp_id, "")
            .map_err(|e| BrokerError::NotConfigured(format!("bad FIX session id: {e}")))
    }

    /// Build and send the `NewOrderSingle`. Kept synchronous so the non-`Send`
    /// QuickFIX `Message`/`SessionId` never live across the ack `.await`.
    fn send_new_order(&self, req: &BrokerOrderRequest) -> Result<(), BrokerError> {
        let mut msg = self.dialect.build_new_order_single(req)?;
        msg.set_field(tag::CL_ORD_ID, req.order_id.as_str())
            .map_err(|e| BrokerError::Network(format!("set ClOrdID: {e}")))?;
        let session = self.session_id()?;
        send_to_target(msg, &session).map_err(|e| BrokerError::Network(format!("FIX send failed: {e}")))
    }

    /// Build and send an `OrderCancelRequest`. Synchronous for the same reason as
    /// [`send_new_order`].
    fn send_cancel(&self, external_order_id: &str, symbol: &str) -> Result<(), BrokerError> {
        let msg = self.dialect.build_cancel(external_order_id, symbol)?;
        let session = self.session_id()?;
        send_to_target(msg, &session)
            .map_err(|e| BrokerError::Network(format!("FIX cancel send failed: {e}")))
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
}
