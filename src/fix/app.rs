//! The QuickFIX [`ApplicationCallback`] bridge.
//!
//! QuickFIX drives its own session threads and invokes these callbacks on them.
//! We keep the callbacks cheap and thread-safe: parse the message, resolve any
//! pending `submit_order` ack, and hand normalized execution reports to an async
//! drain task over an unbounded channel (whose `send` is synchronous, so it is
//! safe to call from the C++ callback thread). Session admin — logon, heartbeat,
//! sequence numbers, resend/gap-fill — is entirely QuickFIX's job.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use quickfix::{ApplicationCallback, FieldMap, Message, MsgFromAdminError, MsgFromAppError, SessionId};
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};
use uuid::Uuid;

use crate::domain::orders::commands::ExecutionReport;
use crate::fix::dialect::{parse_order_status_report, tag, FixDialect, StatusReport};
use crate::recon_orders::{BrokerOpenOrder, BrokerOrderState};
use crate::stream_health::StreamHandle;

/// Result handed back to a waiting `submit_order`: the broker `OrderID` on accept,
/// or a reject reason.
pub type AckResult = Result<String, String>;

/// ClOrdID → the oneshot a `submit_order` is blocked on. Shared between the async
/// adapter (which inserts) and the callback thread (which resolves).
pub type PendingAcks = Arc<Mutex<HashMap<String, oneshot::Sender<AckResult>>>>;

/// An in-flight `open_orders` (35=AF) call: the mass-status reports accumulate in
/// `acc` on the callback thread until the batch's final report, then `tx` delivers
/// them to the awaiting adapter.
pub struct SnapshotWaiter {
    pub acc: Vec<BrokerOpenOrder>,
    pub tx: oneshot::Sender<Vec<BrokerOpenOrder>>,
}

/// MassStatusReqID → the mass-status collector for an `open_orders` call.
pub type PendingSnapshots = Arc<Mutex<HashMap<String, SnapshotWaiter>>>;

/// ClOrdID → the oneshot an `order_status` (35=H) call is blocked on.
pub type PendingStatus = Arc<Mutex<HashMap<String, oneshot::Sender<BrokerOrderState>>>>;

/// A normalized inbound report addressed to one of our orders, carried from the
/// callback thread to the async drain task.
pub type InboundReport = (Uuid, ExecutionReport);

/// Bridges one broker's QuickFIX session to the OMS. One instance per session.
pub struct FixApplication {
    dialect: Arc<dyn FixDialect>,
    health: StreamHandle,
    pending: PendingAcks,
    snapshots: PendingSnapshots,
    statuses: PendingStatus,
    report_tx: mpsc::UnboundedSender<InboundReport>,
    /// Nudges the order-reconcile driver to snapshot on each (re)logon.
    logon_tx: mpsc::Sender<()>,
}

impl FixApplication {
    pub fn new(
        dialect: Arc<dyn FixDialect>,
        health: StreamHandle,
        pending: PendingAcks,
        snapshots: PendingSnapshots,
        statuses: PendingStatus,
        report_tx: mpsc::UnboundedSender<InboundReport>,
        logon_tx: mpsc::Sender<()>,
    ) -> Self {
        Self { dialect, health, pending, snapshots, statuses, report_tx, logon_tx }
    }

    /// Resolve a pending `submit_order` for `cl_ord_id`, if one is waiting.
    fn resolve_ack(&self, cl_ord_id: &str, result: AckResult) {
        if let Ok(mut map) = self.pending.lock() {
            if let Some(tx) = map.remove(cl_ord_id) {
                let _ = tx.send(result);
            }
        }
    }

    /// Accumulate one mass-status report into its `open_orders` waiter; on the
    /// batch's final report (or an empty batch) deliver the collected snapshot.
    fn collect_snapshot(&self, sr: &StatusReport) {
        let Some(reqid) = sr.mass_status_req_id.clone() else { return };
        let Ok(mut map) = self.snapshots.lock() else { return };
        if let Some(w) = map.get_mut(&reqid) {
            if let Some(open) = sr.to_open_order() {
                w.acc.push(open);
            }
        }
        if sr.last_rpt_requested || sr.empty_batch {
            if let Some(w) = map.remove(&reqid) {
                let _ = w.tx.send(w.acc);
            }
        }
    }

    /// Deliver a single 35=H reply to a waiting `order_status`. Returns whether one
    /// was waiting (so an unsolicited status can be ignored).
    fn resolve_status(&self, sr: &StatusReport) -> bool {
        if sr.client_order_id.is_empty() {
            return false;
        }
        let Ok(mut map) = self.statuses.lock() else { return false };
        if let Some(tx) = map.remove(&sr.client_order_id) {
            let _ = tx.send(sr.to_order_state());
            true
        } else {
            false
        }
    }
}

impl ApplicationCallback for FixApplication {
    fn on_create(&self, session: &SessionId) {
        info!(venue = self.dialect.venue(), ?session, "FIX session created");
        self.health.set_connecting();
    }

    fn on_logon(&self, session: &SessionId) {
        info!(venue = self.dialect.venue(), ?session, "FIX logon");
        self.health.set_live();
        // Snapshot-on-(re)logon: nudge the recon driver. `ResetOnLogon=Y` means
        // QuickFIX won't resend messages missed during the outage, so a mass-status
        // diff here is how we recover them.
        let _ = self.logon_tx.try_send(());
    }

    fn on_logout(&self, session: &SessionId) {
        warn!(venue = self.dialect.venue(), ?session, "FIX logout");
        self.health.set_down("logged out");
    }

    /// Outgoing admin messages: stamp Logon (35=A) with broker auth fields. The
    /// header (MsgSeqNum/SendingTime) is already populated here, which the Binance
    /// Ed25519 signature depends on.
    fn on_msg_to_admin(&self, msg: &mut Message, _session: &SessionId) {
        let is_logon = msg
            .with_header(|h| h.get_field(tag::MSG_TYPE))
            .as_deref()
            == Some("A");
        if is_logon {
            self.dialect.augment_logon(msg);
        }
    }

    /// Inbound admin messages — Heartbeat (35=0), TestRequest, Logon/Logout, etc.
    /// These carry no OMS state, but bumping freshness on them keeps the health
    /// badge warm on a quiet-but-live session, matching the WS streams (which tick
    /// on every keepalive frame). Without this, `last_event_at` only moves on fills.
    fn on_msg_from_admin(&self, msg: &Message, _session: &SessionId) -> Result<(), MsgFromAdminError> {
        self.health.record_event();
        // A session-level Reject (35=3) means the peer refused a message we sent
        // (bad tag, format, seqnum) — surface it rather than letting the caller
        // silently time out. It references the offending message by seqnum, not
        // ClOrdID, so we can only log it, not fail a specific pending order.
        if msg.with_header(|h| h.get_field(tag::MSG_TYPE)).as_deref() == Some("3") {
            warn!(
                venue = self.dialect.venue(),
                ref_seq = msg.get_field(45),
                ref_tag = msg.get_field(371),
                ref_msg_type = msg.get_field(372),
                text = msg.get_field(tag::TEXT),
                "FIX: session-level Reject from broker",
            );
        }
        Ok(())
    }

    /// Inbound application messages — execution reports live here.
    fn on_msg_from_app(&self, msg: &Message, _session: &SessionId) -> Result<(), MsgFromAppError> {
        self.health.record_event();

        // Solicited order-status replies (35=AF mass status / 35=H) carry
        // `ExecType=I` and are collected for reconciliation, not applied as fills.
        if let Some(sr) = parse_order_status_report(msg) {
            if sr.mass_status_req_id.is_some() {
                self.collect_snapshot(&sr);
            } else {
                // A single 35=H reply, or an unsolicited status we ignore.
                self.resolve_status(&sr);
            }
            return Ok(());
        }

        let Some(parsed) = self.dialect.parse_execution_report(msg) else {
            return Ok(());
        };

        // Resolve a waiting submit_order: accept → OrderID, reject → reason.
        let cl_ord_id = parsed.order_id.to_string();
        if let Some(reason) = &parsed.reject_reason {
            self.resolve_ack(&cl_ord_id, Err(reason.clone()));
        } else if parsed.is_ack {
            let ext = parsed.external_order_id.clone().unwrap_or_default();
            self.resolve_ack(&cl_ord_id, Ok(ext));
        }

        // Apply anything that changes OMS state (fills, cancels, rejects). A pure
        // New ack carries no report and only resolved the oneshot above.
        if let Some(report) = parsed.report {
            if self.report_tx.send((parsed.order_id, report)).is_err() {
                warn!(order_id = %parsed.order_id, "FIX: exec-report drain closed, dropping report");
            }
        }
        Ok(())
    }
}
