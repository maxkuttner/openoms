//! The per-broker seam. QuickFIX owns the session; a `FixDialect` owns the
//! parts that differ between venues: the FIX version, the Logon auth fields, how
//! an order maps onto `NewOrderSingle`/`OrderCancelRequest`, and how an inbound
//! `ExecutionReport` (35=8) maps back to [`VenueEvent`]s.

use chrono::{DateTime, NaiveDateTime, Utc};
use quickfix::{FieldMap, Message};
use tracing::warn;

use crate::adapter::{BrokerError, BrokerOrderRequest};
use crate::contract::{BrokerOpenOrder, BrokerOrderOutcome, BrokerOrderState};
use crate::venue_event::{VenueEvent, VenueFill, VenueOrderStatus, VenueOrderUpdate};

/// FIX field tags used across dialects. Named so the message-building code reads
/// like the spec rather than a wall of integers. A few are kept for completeness
/// of the protocol table even where not yet read.
#[allow(dead_code)]
pub mod tag {
    pub const MSG_TYPE: i32 = 35;
    pub const MSG_SEQ_NUM: i32 = 34;
    pub const EXEC_TRANS_TYPE: i32 = 20;
    pub const ACCOUNT: i32 = 1;
    pub const COMMISSION: i32 = 12;
    pub const COMM_TYPE: i32 = 13;
    pub const CURRENCY: i32 = 15;
    pub const COMM_CURRENCY: i32 = 479;
    pub const MISC_FEE_AMT: i32 = 137;
    pub const MISC_FEE_CURR: i32 = 138;
    pub const CL_ORD_ID: i32 = 11;
    pub const ORIG_CL_ORD_ID: i32 = 41;
    pub const ORDER_ID: i32 = 37;
    pub const EXEC_ID: i32 = 17;
    pub const SYMBOL: i32 = 55;
    pub const SECURITY_ID: i32 = 48;
    pub const SECURITY_ID_SOURCE: i32 = 22;
    pub const SIDE: i32 = 54;
    pub const ORDER_QTY: i32 = 38;
    pub const ORD_TYPE: i32 = 40;
    pub const PRICE: i32 = 44;
    pub const TIME_IN_FORCE: i32 = 59;
    pub const TRANSACT_TIME: i32 = 60;
    pub const EXEC_TYPE: i32 = 150;
    pub const ORD_STATUS: i32 = 39;
    pub const LAST_QTY: i32 = 32;
    pub const LAST_PX: i32 = 31;
    pub const CUM_QTY: i32 = 14;
    pub const AVG_PX: i32 = 6;
    pub const TEXT: i32 = 58;
    pub const ORD_REJ_REASON: i32 = 103;
    pub const CXL_REJ_REASON: i32 = 102;
    pub const RAW_DATA_LENGTH: i32 = 95;
    pub const RAW_DATA: i32 = 96;
    pub const USERNAME: i32 = 553;
    pub const PASSWORD: i32 = 554;
    // Order Mass Status Request (35=AF) / Order Status Request (35=H) and the
    // status-report fields they draw back.
    pub const MASS_STATUS_REQ_ID: i32 = 584;
    pub const MASS_STATUS_REQ_TYPE: i32 = 585;
    pub const TOT_NUM_REPORTS: i32 = 911;
    pub const LAST_RPT_REQUESTED: i32 = 912;
}

/// A parsed inbound `ExecutionReport` (35=8).
pub struct ParsedExecReport {
    /// `OrigClOrdID`(41) on cancels, else `ClOrdID`(11), as sent — not required
    /// to be one of our UUIDs (ingest matches by venue order id or adopts).
    pub client_order_id: Option<String>,
    /// The broker's own order id from `OrderID`(37), when present. Returned by
    /// `submit_order` as the external order id.
    pub external_order_id: Option<String>,
    /// True for the first ack of a new order (`ExecType=0` New), which resolves a
    /// pending `submit_order`.
    pub is_ack: bool,
    /// A reject reason, when `ExecType`=8 — used to fail a pending `submit_order`.
    pub reject_reason: Option<String>,
    /// What the report says about the order and its executions, for ingest.
    pub events: Vec<VenueEvent>,
}

/// The venue-specific half of a FIX connection. `Send + Sync` so it can be shared
/// across the QuickFIX callback threads and the async submit path.
pub trait FixDialect: Send + Sync {
    /// FIX version string for the session, e.g. `"FIX.4.2"` / `"FIX.4.4"`.
    fn begin_string(&self) -> &'static str;

    /// The venue this dialect trades: the broker registry key (`"IBKR"` / `"BINANCE"`).
    fn venue(&self) -> &'static str;

    /// Add broker-specific auth fields to an outgoing Logon (35=A). Called from
    /// `ApplicationCallback::on_msg_to_admin` once QuickFIX has stamped the header
    /// (so `MsgSeqNum`/`SendingTime` are available for signing).
    fn augment_logon(&self, _msg: &mut Message) {}

    /// Build a `NewOrderSingle` (35=D) for `req`. `ClOrdID` is set by the caller.
    fn build_new_order_single(&self, req: &BrokerOrderRequest) -> Result<Message, BrokerError>;

    /// Build an `OrderCancelRequest` (35=F) for a resting order.
    fn build_cancel(&self, external_order_id: &str, symbol: &str) -> Result<Message, BrokerError>;

    /// Map an inbound `ExecutionReport` (35=8) to venue events. Returns `None`
    /// when the message isn't an execution report. Most dialects delegate to
    /// [`parse_standard_exec_report`].
    fn parse_execution_report(&self, msg: &Message) -> Option<ParsedExecReport>;

    /// Build an `Order Mass Status Request` (35=AF) for the snapshot side of order
    /// reconciliation — `MassStatusReqType`(585)=7 (all orders). The broker replies
    /// with one `ExecutionReport` (`ExecType=I`) per order, echoing
    /// `MassStatusReqID`(584), the last carrying `LastRptRequested`(912)=Y. Standard
    /// FIX 4.3+; on a venue that doesn't answer it (e.g. an older gateway) the
    /// caller's wait simply times out and reconciliation is skipped.
    fn build_mass_status_request(&self, req_id: &str) -> Result<Message, BrokerError> {
        let mut msg = Message::new();
        msg.with_header_mut(|h| h.set_field(tag::MSG_TYPE, "AF"))
            .map_err(|e| BrokerError::Network(format!("set MsgType: {e}")))?;
        msg.set_field(tag::MASS_STATUS_REQ_ID, req_id)
            .map_err(|e| BrokerError::Network(format!("set MassStatusReqID: {e}")))?;
        msg.set_field(tag::MASS_STATUS_REQ_TYPE, "7")
            .map_err(|e| BrokerError::Network(format!("set MassStatusReqType: {e}")))?;
        Ok(msg)
    }

    /// Build an `Order Status Request` (35=H) for one resting order, correlated on
    /// `ClOrdID`(11) (our order UUID) so the reply routes back to the waiter.
    fn build_order_status_request(
        &self,
        client_order_id: &str,
        external_order_id: &str,
        symbol: &str,
    ) -> Result<Message, BrokerError> {
        let mut msg = Message::new();
        msg.with_header_mut(|h| h.set_field(tag::MSG_TYPE, "H"))
            .map_err(|e| BrokerError::Network(format!("set MsgType: {e}")))?;
        let set = |m: &mut Message, t: i32, v: &str| -> Result<(), BrokerError> {
            m.set_field(t, v).map_err(|e| BrokerError::Network(format!("set {t}: {e}")))
        };
        set(&mut msg, tag::CL_ORD_ID, client_order_id)?;
        if !external_order_id.is_empty() {
            set(&mut msg, tag::ORDER_ID, external_order_id)?;
        }
        set(&mut msg, tag::SYMBOL, symbol)?;
        Ok(msg)
    }
}

/// A solicited order-status `ExecutionReport` (`ExecType=I`), the reply to a 35=AF /
/// 35=H. Carries the order's cumulative state rather than a single fill.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusReport {
    /// Echoed `MassStatusReqID`(584) when this is one of a mass-status batch.
    pub mass_status_req_id: Option<String>,
    /// `ClOrdID`(11) — our order UUID (may be empty on an empty mass-status reply).
    pub client_order_id: String,
    /// `OrderID`(37) — the venue's id for the order.
    pub venue_order_id: String,
    pub symbol: String,
    /// `CumQty`(14) — total executed quantity for the order.
    pub cum_qty: f64,
    /// `AvgPx`(6) — average fill price.
    pub avg_px: f64,
    /// `OrdStatus`(39): `0`=New `1`=PartiallyFilled `2`=Filled `4`=Canceled
    /// `8`=Rejected `C`=Expired.
    pub ord_status: String,
    /// `Account`(1), empty when absent.
    pub account: String,
    /// `Side`(54)=1.
    pub buy: bool,
    /// `OrderQty`(38).
    pub qty: f64,
    /// `OrdType`(40) as `market`/`limit` (else the raw value).
    pub order_type: String,
    /// `Price`(44), when non-zero.
    pub limit_px: Option<f64>,
    /// `Text`(58).
    pub text: Option<String>,
    /// `OrdRejReason`(103); `5` = unknown order.
    pub ord_rej_reason: Option<String>,
    /// `LastRptRequested`(912)=Y — the final report of a mass-status batch.
    pub last_rpt_requested: bool,
    /// `TotNumReports`(911)=0 — the venue's "no open orders" empty-batch reply.
    pub empty_batch: bool,
    /// `TransactTime`(60): the venue's time of this report.
    pub transact_time: Option<DateTime<Utc>>,
}

impl StatusReport {
    /// Terminal `OrdStatus` — the order is no longer open.
    pub fn is_terminal(&self) -> bool {
        matches!(self.ord_status.as_str(), "2" | "4" | "8" | "C")
    }

    /// As a broker open-order snapshot row (for the mass-status path). `None` when
    /// terminal (a terminal order isn't "open") or unidentified.
    pub fn to_open_order(&self) -> Option<BrokerOpenOrder> {
        if self.is_terminal() || self.client_order_id.is_empty() {
            return None;
        }
        Some(BrokerOpenOrder {
            client_order_id: self.client_order_id.clone(),
            venue_order_id: self.venue_order_id.clone(),
            symbol: self.symbol.clone(),
            executed_qty: self.cum_qty,
        })
    }

    /// As a venue order update for the reconciler's open-order snapshot. `None`
    /// when terminal, unidentified (no `OrderID`) or of an unknown status.
    pub fn to_venue_update(&self) -> Option<VenueOrderUpdate> {
        if self.is_terminal() || self.venue_order_id.is_empty() || !matches!(self.ord_status.as_str(), "0" | "1") {
            return None;
        }
        Some(VenueOrderUpdate {
            venue_order_id: self.venue_order_id.clone(),
            client_order_id: Some(self.client_order_id.clone()).filter(|c| !c.is_empty()),
            account_ref: self.account.clone(),
            symbol: self.symbol.clone(),
            buy: self.buy,
            order_type: self.order_type.clone(),
            qty: self.qty,
            limit_px: self.limit_px,
            status: VenueOrderStatus::Working,
            cum_qty: self.cum_qty,
            ts: self.transact_time,
            created_at: None,
        })
    }

    /// As a resolved order state (for the single 35=H path).
    pub fn to_order_state(&self) -> BrokerOrderState {
        let outcome = match self.ord_status.as_str() {
            "2" => BrokerOrderOutcome::Filled,
            "4" => BrokerOrderOutcome::Canceled,
            "8" | "C" => BrokerOrderOutcome::Rejected,
            _ => BrokerOrderOutcome::Working,
        };
        BrokerOrderState { outcome, executed_qty: self.cum_qty, avg_px: self.avg_px }
    }
}

/// Parse a solicited order-status `ExecutionReport` (35=8 with `ExecType`(150)=`I`).
/// Returns `None` for anything else — including live fills/acks, which stay on the
/// existing execution path.
pub fn parse_order_status_report(msg: &Message) -> Option<StatusReport> {
    if msg.with_header(|h| h.get_field(tag::MSG_TYPE)).as_deref() != Some("8") {
        return None;
    }
    if msg.get_field(tag::EXEC_TYPE).as_deref() != Some("I") {
        return None;
    }
    Some(StatusReport {
        mass_status_req_id: msg.get_field(tag::MASS_STATUS_REQ_ID).filter(|s| !s.is_empty()),
        client_order_id: msg.get_field(tag::CL_ORD_ID).unwrap_or_default(),
        venue_order_id: order_id_field(msg).unwrap_or_default(),
        symbol: msg.get_field(tag::SYMBOL).unwrap_or_default(),
        cum_qty: msg.get_field(tag::CUM_QTY).and_then(|s| s.parse().ok()).unwrap_or(0.0),
        avg_px: msg.get_field(tag::AVG_PX).and_then(|s| s.parse().ok()).unwrap_or(0.0),
        ord_status: msg.get_field(tag::ORD_STATUS).unwrap_or_default(),
        account: msg.get_field(tag::ACCOUNT).unwrap_or_default(),
        text: msg.get_field(tag::TEXT).filter(|s| !s.is_empty()),
        ord_rej_reason: msg.get_field(tag::ORD_REJ_REASON).filter(|s| !s.is_empty()),
        buy: msg.get_field(tag::SIDE).as_deref() == Some("1"),
        qty: msg.get_field(tag::ORDER_QTY).and_then(|s| s.parse().ok()).unwrap_or(0.0),
        order_type: ord_type_name(msg.get_field(tag::ORD_TYPE).as_deref()),
        limit_px: msg.get_field(tag::PRICE).and_then(|s| s.parse::<f64>().ok()).filter(|p| *p != 0.0),
        last_rpt_requested: msg.get_field(tag::LAST_RPT_REQUESTED).as_deref() == Some("Y"),
        empty_batch: msg.get_field(tag::TOT_NUM_REPORTS).and_then(|s| s.parse::<i64>().ok()) == Some(0),
        transact_time: msg.get_field(tag::TRANSACT_TIME).as_deref().and_then(fix_time),
    })
}

/// A mass-status batch as the open-order snapshot of `account_ref` (every
/// account when empty; a report without `Account`(1) counts for any account).
pub fn snapshot_updates(batch: &[StatusReport], account_ref: &str) -> Vec<VenueOrderUpdate> {
    batch
        .iter()
        .filter(|r| account_ref.is_empty() || r.account.is_empty() || r.account == account_ref)
        .filter_map(StatusReport::to_venue_update)
        .collect()
}

/// `OrderID`(37), with the placeholders venues send for "no order" (`0`,
/// `NONE`) treated as absent.
fn order_id_field(msg: &Message) -> Option<String> {
    msg.get_field(tag::ORDER_ID).filter(|s| !s.is_empty() && s != "0" && !s.eq_ignore_ascii_case("NONE"))
}

/// The answer to a 35=H for the order known at the venue as `external_order_id`.
/// A reject (OrdStatus 8) is trusted only when it names that very order and is
/// not the venue's "unknown order" answer (OrdRejReason 5, or such a Text):
/// IBKR answers 35=H for an order it doesn't know (other session, day or client)
/// with OrdStatus 8, which would otherwise close a live or filled order.
pub fn order_state_from_status_reply(sr: &StatusReport, external_order_id: &str) -> Result<BrokerOrderState, BrokerError> {
    if sr.ord_status == "8" {
        let unknown_text = sr.text.as_deref().is_some_and(|t| t.to_ascii_lowercase().contains("unknown"));
        let same_order = !external_order_id.is_empty() && sr.venue_order_id == external_order_id;
        if !same_order || sr.ord_rej_reason.as_deref() == Some("5") || unknown_text {
            return Err(BrokerError::Network(format!(
                "order status unresolved: venue does not know order {external_order_id} (OrderID {:?}, reason {:?}, text {:?})",
                sr.venue_order_id, sr.ord_rej_reason, sr.text
            )));
        }
    }
    Ok(sr.to_order_state())
}

fn ord_type_name(v: Option<&str>) -> String {
    match v {
        Some("1") => "market".to_string(),
        Some("2") => "limit".to_string(),
        other => other.unwrap_or_default().to_string(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use quickfix::FieldMap;
    use uuid::Uuid;

    use crate::adapter::BrokerOrderRequest;
    use crate::venue_event::{VenueEvent, VenueFill, VenueOrderStatus, VenueOrderUpdate};

    pub(crate) fn order(order_id: &Uuid, order_type: &str, limit: Option<f64>) -> BrokerOrderRequest {
        BrokerOrderRequest {
            order_id: order_id.to_string(),
            symbol: "BTCUSDT".to_string(),
            native_id: Some("42".to_string()),
            quantity: 1.5,
            side: "buy".to_string(),
            order_type: order_type.to_string(),
            time_in_force: "gtc".to_string(),
            limit_price: limit,
            external_account_ref: "acct".to_string(),
            legs: vec![],
        }
    }

    /// Build an inbound ExecutionReport (35=8) message for parser tests.
    fn exec_report(fields: &[(i32, &str)]) -> Message {
        let mut m = Message::new();
        m.with_header_mut(|h| h.set_field(tag::MSG_TYPE, "8")).unwrap();
        for (t, v) in fields {
            m.set_field(*t, *v).unwrap();
        }
        m
    }

    fn fill_of(p: &ParsedExecReport) -> &VenueFill {
        p.events.iter().find_map(|e| match e { VenueEvent::Fill(f) => Some(f), _ => None }).expect("fill")
    }

    fn order_of(p: &ParsedExecReport) -> &VenueOrderUpdate {
        p.events.iter().find_map(|e| match e { VenueEvent::Order(o) => Some(o), _ => None }).expect("order")
    }

    #[test]
    fn parses_new_ack() {
        let id = Uuid::new_v4();
        let msg = exec_report(&[
            (tag::CL_ORD_ID, &id.to_string()),
            (tag::ORDER_ID, "BROKER-99"),
            (tag::EXEC_TYPE, "0"),
            (tag::ORD_STATUS, "0"),
            (tag::SYMBOL, "BTCUSDT"),
            (tag::SIDE, "1"),
            (tag::ORDER_QTY, "1.5"),
            (tag::ORD_TYPE, "2"),
            (tag::PRICE, "100"),
        ]);
        let p = parse_standard_exec_report(&msg, tag::EXEC_ID).unwrap();
        assert_eq!(p.client_order_id.as_deref(), Some(id.to_string().as_str()));
        assert!(p.is_ack);
        assert_eq!(p.external_order_id.as_deref(), Some("BROKER-99"));
        let o = order_of(&p);
        assert_eq!(o.status, VenueOrderStatus::Working);
        assert_eq!((o.symbol.as_str(), o.buy, o.qty, o.limit_px, o.order_type.as_str()), ("BTCUSDT", true, 1.5, Some(100.0), "limit"));
    }

    #[test]
    fn parses_fill_44_trade_and_42_fill() {
        let id = Uuid::new_v4();
        for exec_type in ["F", "2", "1"] {
            let msg = exec_report(&[
                (tag::CL_ORD_ID, &id.to_string()),
                (tag::ORDER_ID, "9001"),
                (tag::ACCOUNT, "U123"),
                (tag::EXEC_TYPE, exec_type),
                (tag::ORD_STATUS, "1"),
                (tag::SYMBOL, "AAPL"),
                (tag::SIDE, "2"),
                (tag::LAST_QTY, "0.5"),
                (tag::LAST_PX, "100.25"),
                (tag::CUM_QTY, "0.5"),
                (tag::EXEC_ID, "E-1"),
                (tag::COMMISSION, "1.25"),
                (tag::CURRENCY, "USD"),
            ]);
            let p = parse_standard_exec_report(&msg, tag::EXEC_ID).unwrap();
            let f = fill_of(&p);
            assert_eq!((f.exec_id.as_str(), f.venue_order_id.as_str(), f.account_ref.as_str()), ("E-1", "9001", "U123"), "{exec_type}");
            assert_eq!((f.qty, f.price, f.buy, f.symbol.as_str()), (0.5, 100.25, false, "AAPL"));
            assert_eq!(f.fee, Some((1.25, "USD".into())));
            assert_eq!(order_of(&p).cum_qty, 0.5);
        }
    }

    #[test]
    fn fill_without_exec_id_books_no_fill() {
        let msg = exec_report(&[(tag::CL_ORD_ID, "x"), (tag::ORDER_ID, "1"), (tag::EXEC_TYPE, "F"), (tag::LAST_QTY, "1"), (tag::LAST_PX, "2")]);
        let p = parse_standard_exec_report(&msg, tag::EXEC_ID).unwrap();
        assert!(!p.events.iter().any(|e| matches!(e, VenueEvent::Fill(_))));
    }

    #[test]
    fn exec_report_with_non_uuid_clordid_is_kept() {
        let msg = exec_report(&[
            (tag::CL_ORD_ID, "manual-42"),
            (tag::ORDER_ID, "777"),
            (tag::EXEC_TYPE, "F"),
            (tag::ORD_STATUS, "2"),
            (tag::LAST_QTY, "1"),
            (tag::LAST_PX, "10"),
            (tag::EXEC_ID, "X-1"),
        ]);
        let p = parse_standard_exec_report(&msg, tag::EXEC_ID).expect("kept");
        assert_eq!(fill_of(&p).client_order_id.as_deref(), Some("manual-42"));
        assert_eq!(order_of(&p).status, VenueOrderStatus::Filled);
        assert_eq!(fill_of(&p).account_ref, "");
    }

    #[test]
    fn parses_reject_sets_reason_and_report() {
        let id = Uuid::new_v4();
        let msg = exec_report(&[
            (tag::CL_ORD_ID, &id.to_string()),
            (tag::ORDER_ID, "555"),
            (tag::EXEC_TYPE, "8"),
            (tag::ORD_STATUS, "8"),
            (tag::TEXT, "insufficient balance"),
        ]);
        let p = parse_standard_exec_report(&msg, tag::EXEC_ID).unwrap();
        assert_eq!(p.reject_reason.as_deref(), Some("insufficient balance"));
        assert_eq!(order_of(&p).status, VenueOrderStatus::Rejected { reason: "insufficient balance".into() });
    }

    #[test]
    fn cancel_keys_on_orig_cl_ord_id() {
        let orig = Uuid::new_v4();
        let msg = exec_report(&[
            (tag::ORIG_CL_ORD_ID, &orig.to_string()),
            (tag::CL_ORD_ID, "cxl-BROKER-99"), // the cancel's own id, not a UUID
            (tag::ORDER_ID, "BROKER-99"),
            (tag::EXEC_TYPE, "4"),
            (tag::ORD_STATUS, "4"),
        ]);
        let p = parse_standard_exec_report(&msg, tag::EXEC_ID).unwrap();
        assert_eq!(p.client_order_id.as_deref(), Some(orig.to_string().as_str()));
        assert_eq!(order_of(&p).status, VenueOrderStatus::Canceled);
    }

    #[test]
    fn expired_maps_to_expired() {
        let msg = exec_report(&[(tag::CL_ORD_ID, "a"), (tag::ORDER_ID, "1"), (tag::EXEC_TYPE, "C"), (tag::ORD_STATUS, "C")]);
        let p = parse_standard_exec_report(&msg, tag::EXEC_ID).unwrap();
        assert_eq!(order_of(&p).status, VenueOrderStatus::Expired);
    }

    #[test]
    fn ignores_non_exec_report() {
        // A heartbeat (35=0) is not an execution report.
        let mut m = Message::new();
        m.with_header_mut(|h| h.set_field(tag::MSG_TYPE, "0")).unwrap();
        assert!(parse_standard_exec_report(&m, tag::EXEC_ID).is_none());
    }

    #[test]
    fn status_report_only_matches_exec_type_i() {
        // A live fill (ExecType=F) must NOT be picked up as a status report.
        let fill = exec_report(&[(tag::CL_ORD_ID, "x"), (tag::EXEC_TYPE, "F")]);
        assert!(parse_order_status_report(&fill).is_none());
    }

    #[test]
    fn parses_mass_status_open_order() {
        let id = Uuid::new_v4();
        let msg = exec_report(&[
            (tag::MASS_STATUS_REQ_ID, "req-1"),
            (tag::CL_ORD_ID, &id.to_string()),
            (tag::EXEC_TYPE, "I"),
            (tag::ORD_STATUS, "1"), // partially filled → still open
            (tag::SYMBOL, "BTCUSDT"),
            (tag::CUM_QTY, "0.5"),
            (tag::AVG_PX, "101.0"),
            (tag::LAST_RPT_REQUESTED, "Y"),
        ]);
        let sr = parse_order_status_report(&msg).expect("status report");
        assert_eq!(sr.mass_status_req_id.as_deref(), Some("req-1"));
        assert!(sr.last_rpt_requested);
        assert!(!sr.is_terminal());
        let open = sr.to_open_order().expect("open order");
        assert_eq!(open.client_order_id, id.to_string());
        assert_eq!(open.executed_qty, 0.5);
        // avg_px is retained on to_order_state for terminal fill pricing.
        assert_eq!(sr.to_order_state().avg_px, 101.0);
    }

    #[test]
    fn ibkr_fix_snapshot_maps_mass_status_to_order_updates() {
        let report = |cl: &str, oid: &str, status: &str, acct: &str| {
            parse_order_status_report(&exec_report(&[
                (tag::MASS_STATUS_REQ_ID, "req-1"),
                (tag::CL_ORD_ID, cl),
                (tag::ORDER_ID, oid),
                (tag::ACCOUNT, acct),
                (tag::EXEC_TYPE, "I"),
                (tag::ORD_STATUS, status),
                (tag::SYMBOL, "AAPL"),
                (tag::SIDE, "2"),
                (tag::ORDER_QTY, "10"),
                (tag::ORD_TYPE, "2"),
                (tag::PRICE, "150.5"),
                (tag::CUM_QTY, "4"),
            ]))
            .unwrap()
        };
        let batch = vec![
            report("manual-1", "701", "1", "U1"),
            report("x", "702", "2", "U1"), // terminal: not an open order
            report("y", "703", "0", "U2"), // other account
        ];
        let snap = snapshot_updates(&batch, "U1");
        assert_eq!(snap.len(), 1);
        let o = &snap[0];
        assert_eq!((o.venue_order_id.as_str(), o.client_order_id.as_deref(), o.account_ref.as_str()), ("701", Some("manual-1"), "U1"));
        assert_eq!((o.status.clone(), o.cum_qty, o.qty, o.buy, o.limit_px), (VenueOrderStatus::Working, 4.0, 10.0, false, Some(150.5)));
        assert_eq!(o.order_type, "limit");
        // An empty account ref takes every account's open orders.
        assert_eq!(snapshot_updates(&batch, "").len(), 2);
    }

    fn status_reply(fields: &[(i32, &str)]) -> StatusReport {
        let mut all = vec![(tag::CL_ORD_ID, "c-1"), (tag::EXEC_TYPE, "I")];
        all.extend_from_slice(fields);
        parse_order_status_report(&exec_report(&all)).unwrap()
    }

    #[test]
    fn fix_order_update_carries_update_time_and_no_created_time() {
        let msg = exec_report(&[
            (tag::CL_ORD_ID, "c-1"), (tag::ORDER_ID, "B-1"), (tag::EXEC_TYPE, "0"), (tag::ORD_STATUS, "0"),
            (tag::SYMBOL, "BTCUSDT"), (tag::SIDE, "1"), (tag::ORDER_QTY, "1"), (tag::ORD_TYPE, "2"), (tag::PRICE, "100"),
            (tag::TRANSACT_TIME, "20261119-10:00:01.250"),
        ]);
        let p = parse_standard_exec_report(&msg, tag::EXEC_ID).unwrap();
        let o = order_of(&p);
        assert_eq!(o.ts, Some("2026-11-19T10:00:01.250Z".parse().unwrap()));
        assert_eq!(o.created_at, None);
        // The mass-status snapshot carries the report's TransactTime as the update time.
        let sr = status_reply(&[
            (tag::ORDER_ID, "701"), (tag::ORD_STATUS, "0"), (tag::SIDE, "1"), (tag::ORDER_QTY, "1"),
            (tag::TRANSACT_TIME, "20261119-10:00:02"),
        ]);
        let u = snapshot_updates(&[sr], "").remove(0);
        assert_eq!((u.ts, u.created_at), (Some("2026-11-19T10:00:02Z".parse().unwrap()), None));
    }

    #[test]
    fn fix_stream_fill_carries_executed_at() {
        let msg = exec_report(&[
            (tag::CL_ORD_ID, "c-1"), (tag::ORDER_ID, "B-1"), (tag::EXEC_ID, "E-1"), (tag::EXEC_TYPE, "F"), (tag::ORD_STATUS, "1"),
            (tag::SYMBOL, "BTCUSDT"), (tag::SIDE, "1"), (tag::LAST_QTY, "0.5"), (tag::LAST_PX, "100"),
            (tag::TRANSACT_TIME, "20261119-10:00:03.500"),
        ]);
        let p = parse_standard_exec_report(&msg, tag::EXEC_ID).unwrap();
        assert_eq!(fill_of(&p).executed_at, Some("2026-11-19T10:00:03.500Z".parse().unwrap()));
    }

    #[test]
    fn unknown_order_status_reply_does_not_close_the_order() {
        // IBKR answers 35=H for an order it doesn't know with OrdStatus=8.
        let unknown = status_reply(&[(tag::ORDER_ID, "0"), (tag::ORD_STATUS, "8"), (tag::ORD_REJ_REASON, "5")]);
        assert!(order_state_from_status_reply(&unknown, "701").is_err());
        let other = status_reply(&[(tag::ORDER_ID, "999"), (tag::ORD_STATUS, "8")]);
        assert!(order_state_from_status_reply(&other, "701").is_err());
        let text = status_reply(&[(tag::ORDER_ID, "701"), (tag::ORD_STATUS, "8"), (tag::TEXT, "Unknown order")]);
        assert!(order_state_from_status_reply(&text, "701").is_err());
        // A real reject of this very order is trusted.
        let real = status_reply(&[(tag::ORDER_ID, "701"), (tag::ORD_STATUS, "8"), (tag::TEXT, "margin")]);
        assert_eq!(order_state_from_status_reply(&real, "701").unwrap().outcome, BrokerOrderOutcome::Rejected);
        // Non-reject statuses pass through.
        let filled = status_reply(&[(tag::ORDER_ID, "701"), (tag::ORD_STATUS, "2"), (tag::CUM_QTY, "3")]);
        assert_eq!(order_state_from_status_reply(&filled, "701").unwrap().outcome, BrokerOrderOutcome::Filled);
    }

    #[test]
    fn fix_trade_bust_is_not_booked_as_a_fill() {
        let base = [
            (tag::CL_ORD_ID, "c"), (tag::ORDER_ID, "1"), (tag::EXEC_ID, "E"), (tag::LAST_QTY, "1"), (tag::LAST_PX, "2"), (tag::ORD_STATUS, "1"),
        ];
        for extra in [[(tag::EXEC_TYPE, "2"), (tag::EXEC_TRANS_TYPE, "1")], [(tag::EXEC_TYPE, "2"), (tag::EXEC_TRANS_TYPE, "2")], [(tag::EXEC_TYPE, "H"), (tag::EXEC_TRANS_TYPE, "0")], [(tag::EXEC_TYPE, "G"), (tag::EXEC_TRANS_TYPE, "0")]] {
            let mut f = base.to_vec();
            f.extend_from_slice(&extra);
            let p = parse_standard_exec_report(&exec_report(&f), tag::EXEC_ID).unwrap();
            assert!(!p.events.iter().any(|e| matches!(e, VenueEvent::Fill(_))), "{extra:?}");
        }
        let mut f = base.to_vec();
        f.extend_from_slice(&[(tag::EXEC_TYPE, "2"), (tag::EXEC_TRANS_TYPE, "0")]);
        assert!(parse_standard_exec_report(&exec_report(&f), tag::EXEC_ID).unwrap().events.iter().any(|e| matches!(e, VenueEvent::Fill(_))));
    }

    #[test]
    fn placeholder_order_id_is_absent() {
        for oid in ["0", "NONE"] {
            let p = parse_standard_exec_report(&exec_report(&[(tag::CL_ORD_ID, "c"), (tag::ORDER_ID, oid), (tag::EXEC_TYPE, "8"), (tag::ORD_STATUS, "8")]), tag::EXEC_ID).unwrap();
            assert_eq!(p.external_order_id, None);
            assert!(p.events.is_empty());
        }
    }

    #[test]
    fn terminal_status_is_not_an_open_order() {
        let id = Uuid::new_v4();
        let msg = exec_report(&[
            (tag::CL_ORD_ID, &id.to_string()),
            (tag::EXEC_TYPE, "I"),
            (tag::ORD_STATUS, "2"), // filled
            (tag::CUM_QTY, "1.0"),
            (tag::AVG_PX, "100.0"),
        ]);
        let sr = parse_order_status_report(&msg).unwrap();
        assert!(sr.is_terminal());
        assert!(sr.to_open_order().is_none());
        assert_eq!(sr.to_order_state().outcome, BrokerOrderOutcome::Filled);
    }
}

/// FIX `Side`(54): buy=`1`, sell=`2`.
pub fn fix_side(side: &str) -> Result<&'static str, BrokerError> {
    match side {
        "buy" => Ok("1"),
        "sell" => Ok("2"),
        other => Err(BrokerError::NotConfigured(format!("unsupported side: {other}"))),
    }
}

/// FIX `OrdType`(40): market=`1`, limit=`2`.
pub fn fix_ord_type(order_type: &str) -> Result<&'static str, BrokerError> {
    match order_type {
        "market" => Ok("1"),
        "limit" => Ok("2"),
        other => Err(BrokerError::NotConfigured(format!("unsupported order type: {other}"))),
    }
}

/// FIX `TimeInForce`(59): day=`0`, gtc=`1`, ioc=`3`, fok=`4`.
pub fn fix_time_in_force(tif: &str) -> &'static str {
    match tif {
        "gtc" => "1",
        "ioc" => "3",
        "fok" => "4",
        _ => "0", // day
    }
}

/// `TransactTime`(60) / `SendingTime` format: `YYYYMMDD-HH:MM:SS.sss` (UTC).
pub fn transact_time() -> String {
    chrono::Utc::now().format("%Y%m%d-%H:%M:%S%.3f").to_string()
}

/// Set `Account`(1) from `external_account_ref`, omitted when empty.
pub fn set_account(msg: &mut Message, req: &BrokerOrderRequest) -> Result<(), BrokerError> {
    if req.external_account_ref.is_empty() {
        return Ok(());
    }
    msg.set_field(tag::ACCOUNT, req.external_account_ref.as_str())
        .map_err(|e| BrokerError::Network(format!("set Account: {e}")))
}

/// FIX `UTCTimestamp`, with or without milliseconds.
fn fix_time(s: &str) -> Option<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(s, "%Y%m%d-%H:%M:%S%.f").ok().map(|t| t.and_utc())
}

/// Parse a standard FIX `ExecutionReport` (35=8) into venue events. The tags —
/// `ExecType`(150), `OrdStatus`(39), `LastQty`(32)/`LastPx`(31), `OrderID`(37),
/// `CumQty`(14), `Account`(1) — are common to FIX 4.2 and 4.4, so both dialects
/// share this; 4.2 fills report `ExecType` `1`/`2`, 4.4 reports `F`.
///
/// `exec_id_tag` names the tag a fill is keyed on: `ExecID`(17) by default,
/// Binance's `TradeID`(1003) so FIX and REST agree on one id per trade.
///
/// A fill missing its id, `OrderID`, qty or price is not booked, and an order
/// update missing `OrderID` is not sent; both are warned with `MsgSeqNum`.
pub fn parse_standard_exec_report(msg: &Message, exec_id_tag: i32) -> Option<ParsedExecReport> {
    if msg.with_header(|h| h.get_field(tag::MSG_TYPE)).as_deref() != Some("8") {
        return None;
    }
    let field = |t: i32| msg.get_field(t).filter(|s| !s.is_empty());
    let num = |t: i32| field(t).and_then(|s| s.parse::<f64>().ok());
    let seq = msg.with_header(|h| h.get_field(tag::MSG_SEQ_NUM));

    // Prefer OrigClOrdID (present on cancel reports) so a cancel keys on the
    // original order, not the cancel request's own id.
    let client_order_id = field(tag::ORIG_CL_ORD_ID).or_else(|| field(tag::CL_ORD_ID));
    let exec_type = field(tag::EXEC_TYPE).unwrap_or_default();
    let external_order_id = order_id_field(msg);
    let account_ref = field(tag::ACCOUNT).unwrap_or_default();
    let symbol = field(tag::SYMBOL).unwrap_or_default();
    let buy = field(tag::SIDE).as_deref() == Some("1");
    let ts = field(tag::TRANSACT_TIME).as_deref().and_then(fix_time);
    let exec_id = field(exec_id_tag);
    let text = field(tag::TEXT).or_else(|| field(tag::ORD_REJ_REASON));

    let mut out = ParsedExecReport {
        client_order_id: client_order_id.clone(),
        external_order_id: external_order_id.clone(),
        is_ack: exec_type == "0",
        reject_reason: (exec_type == "8").then(|| text.clone().unwrap_or_else(|| "rejected".to_string())),
        events: Vec::new(),
    };

    // A bust/correction (4.2 ExecTransType 1/2; 4.4 ExecType H/G) is not a new
    // fill; busts are not modelled, so it is warned and not booked.
    let trans = field(tag::EXEC_TRANS_TYPE);
    let is_bust = matches!(exec_type.as_str(), "G" | "H") || trans.as_deref().is_some_and(|t| t != "0");
    if is_bust {
        warn!(msg_seq_num = ?seq, exec_id = ?field(tag::EXEC_ID), exec_type, exec_trans_type = ?trans, "FIX: trade bust/correction not booked");
    } else if matches!(exec_type.as_str(), "1" | "2" | "F") {
        match (&exec_id, &external_order_id, num(tag::LAST_QTY), num(tag::LAST_PX)) {
            (Some(exec_id), Some(venue_order_id), Some(qty), Some(price)) => {
                let fee = match (num(tag::COMMISSION), field(tag::COMM_CURRENCY).or_else(|| field(tag::CURRENCY))) {
                    (Some(a), Some(c)) if a != 0.0 => Some((a, c)),
                    _ => match (num(tag::MISC_FEE_AMT), field(tag::MISC_FEE_CURR)) {
                        (Some(a), Some(c)) if a != 0.0 => Some((a, c)),
                        _ => None,
                    },
                };
                out.events.push(VenueEvent::Fill(VenueFill {
                    exec_id: exec_id.clone(),
                    venue_order_id: venue_order_id.clone(),
                    client_order_id: client_order_id.clone(),
                    account_ref: account_ref.clone(),
                    symbol: symbol.clone(),
                    buy,
                    qty,
                    price,
                    fee,
                    executed_at: ts,
                }));
            }
            (id, oid, qty, px) => {
                let missing: Vec<i32> = [
                    (id.is_none(), exec_id_tag),
                    (oid.is_none(), tag::ORDER_ID),
                    (qty.is_none(), tag::LAST_QTY),
                    (px.is_none(), tag::LAST_PX),
                ]
                .into_iter()
                .filter_map(|(m, t)| m.then_some(t))
                .collect();
                warn!(msg_seq_num = ?seq, exec_id = ?field(tag::EXEC_ID), ?missing, "FIX: fill report missing tags, not booked");
            }
        }
    }

    let status = match field(tag::ORD_STATUS).as_deref() {
        Some("0") | Some("1") => Some(VenueOrderStatus::Working),
        Some("2") => Some(VenueOrderStatus::Filled),
        Some("4") => Some(VenueOrderStatus::Canceled),
        Some("8") => Some(VenueOrderStatus::Rejected { reason: text.unwrap_or_else(|| "rejected".to_string()) }),
        Some("C") => Some(VenueOrderStatus::Expired),
        // Pending states (6/A/E) and the like: no order update.
        Some(_) => None,
        None => {
            warn!(msg_seq_num = ?seq, exec_id = ?field(tag::EXEC_ID), missing = tag::ORD_STATUS, "FIX: execution report without OrdStatus, no order update");
            None
        }
    };
    if let Some(status) = status {
        match &external_order_id {
            Some(venue_order_id) => out.events.push(VenueEvent::Order(VenueOrderUpdate {
                venue_order_id: venue_order_id.clone(),
                client_order_id,
                account_ref,
                symbol,
                buy,
                order_type: ord_type_name(field(tag::ORD_TYPE).as_deref()),
                qty: num(tag::ORDER_QTY).unwrap_or(0.0),
                limit_px: num(tag::PRICE).filter(|p| *p != 0.0),
                status,
                cum_qty: num(tag::CUM_QTY).unwrap_or(0.0),
                ts,
                created_at: None,
            })),
            None => warn!(msg_seq_num = ?seq, exec_id = ?field(tag::EXEC_ID), missing = tag::ORDER_ID, "FIX: execution report without OrderID, no order update"),
        }
    }

    Some(out)
}
