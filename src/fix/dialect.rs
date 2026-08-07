//! The per-broker seam. QuickFIX owns the session; a `FixDialect` owns the
//! parts that differ between venues: the FIX version, the Logon auth fields, how
//! an order maps onto `NewOrderSingle`/`OrderCancelRequest`, and how an inbound
//! `ExecutionReport` (35=8) maps back to our internal [`ExecutionReport`].

use quickfix::{FieldMap, Message};
use uuid::Uuid;

use crate::adapters::{BrokerError, BrokerOrderRequest};
use crate::domain::orders::commands::ExecutionReport;
use crate::recon_orders::{BrokerOpenOrder, BrokerOrderOutcome, BrokerOrderState};

/// FIX field tags used across dialects. Named so the message-building code reads
/// like the spec rather than a wall of integers. A few are kept for completeness
/// of the protocol table even where not yet read.
#[allow(dead_code)]
pub mod tag {
    pub const MSG_TYPE: i32 = 35;
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

/// A parsed inbound `ExecutionReport` (35=8), addressed to one of our orders.
pub struct ParsedExecReport {
    /// Our order UUID, taken from `ClOrdID`(11) (or `OrigClOrdID`(41) on cancels).
    pub order_id: Uuid,
    /// The broker's own order id from `OrderID`(37), when present. Populated on
    /// the ack so `submit_order` can return it as the external order id.
    pub external_order_id: Option<String>,
    /// True for the first ack of a new order (`ExecType=0` New), which resolves a
    /// pending `submit_order` rather than moving order state.
    pub is_ack: bool,
    /// The normalized report to apply, if this message changes OMS state. `None`
    /// for a pure ack (New) that only resolves the submit oneshot.
    pub report: Option<ExecutionReport>,
    /// A reject reason, when `ExecType`/`OrdStatus` say the order was rejected —
    /// used to fail a pending `submit_order`.
    pub reject_reason: Option<String>,
}

/// The venue-specific half of a FIX connection. `Send + Sync` so it can be shared
/// across the QuickFIX callback threads and the async submit path.
pub trait FixDialect: Send + Sync {
    /// FIX version string for the session, e.g. `"FIX.4.2"` / `"FIX.4.4"`.
    fn begin_string(&self) -> &'static str;

    /// The venue this dialect trades, as it appears in `ExecutionReport.venue`
    /// and the broker registry key (`"IBKR"` / `"BINANCE"`).
    fn venue(&self) -> &'static str;

    /// Add broker-specific auth fields to an outgoing Logon (35=A). Called from
    /// `ApplicationCallback::on_msg_to_admin` once QuickFIX has stamped the header
    /// (so `MsgSeqNum`/`SendingTime` are available for signing).
    fn augment_logon(&self, _msg: &mut Message) {}

    /// Build a `NewOrderSingle` (35=D) for `req`. `ClOrdID` is set by the caller.
    fn build_new_order_single(&self, req: &BrokerOrderRequest) -> Result<Message, BrokerError>;

    /// Build an `OrderCancelRequest` (35=F) for a resting order.
    fn build_cancel(&self, external_order_id: &str, symbol: &str) -> Result<Message, BrokerError>;

    /// Map an inbound `ExecutionReport` (35=8) to our internal report. Returns
    /// `None` when the message isn't an execution report or its `ClOrdID` isn't a
    /// UUID we issued. Most dialects delegate to [`parse_standard_exec_report`].
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
    pub symbol: String,
    /// `CumQty`(14) — total executed quantity for the order.
    pub cum_qty: f64,
    /// `AvgPx`(6) — average fill price.
    pub avg_px: f64,
    /// `OrdStatus`(39): `0`=New `1`=PartiallyFilled `2`=Filled `4`=Canceled
    /// `8`=Rejected `C`=Expired.
    pub ord_status: String,
    /// `LastRptRequested`(912)=Y — the final report of a mass-status batch.
    pub last_rpt_requested: bool,
    /// `TotNumReports`(911)=0 — the venue's "no open orders" empty-batch reply.
    pub empty_batch: bool,
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
            symbol: self.symbol.clone(),
            executed_qty: self.cum_qty,
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
        symbol: msg.get_field(tag::SYMBOL).unwrap_or_default(),
        cum_qty: msg.get_field(tag::CUM_QTY).and_then(|s| s.parse().ok()).unwrap_or(0.0),
        avg_px: msg.get_field(tag::AVG_PX).and_then(|s| s.parse().ok()).unwrap_or(0.0),
        ord_status: msg.get_field(tag::ORD_STATUS).unwrap_or_default(),
        last_rpt_requested: msg.get_field(tag::LAST_RPT_REQUESTED).as_deref() == Some("Y"),
        empty_batch: msg.get_field(tag::TOT_NUM_REPORTS).and_then(|s| s.parse::<i64>().ok()) == Some(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use quickfix::FieldMap;
    use uuid::Uuid;

    use crate::adapters::BrokerOrderRequest;
    use crate::domain::orders::commands::ExecutionReport;
    use crate::fix::binance::BinanceDialect;
    use crate::fix::ibkr::IbkrDialect;

    fn order(order_id: &Uuid, order_type: &str, limit: Option<f64>) -> BrokerOrderRequest {
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

    // A throwaway Ed25519 PKCS#8 PEM (generated for tests only, not a real
    // credential) so the Binance dialect constructs its signing key.
    const TEST_ED25519_PEM: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIAGvAMHEDQzczRRSN2T3AYeEUyaWSwA6GXlSZ2hjM8TC\n-----END PRIVATE KEY-----\n";

    #[test]
    fn ibkr_builds_new_order_single_with_conid() {
        let id = Uuid::new_v4();
        let d = IbkrDialect::new("pw".into());
        let msg = d.build_new_order_single(&order(&id, "limit", Some(101.5))).unwrap();

        assert_eq!(msg.with_header(|h| h.get_field(tag::MSG_TYPE)).as_deref(), Some("D"));
        assert_eq!(msg.get_field(tag::SECURITY_ID).as_deref(), Some("42"));
        assert_eq!(msg.get_field(tag::SECURITY_ID_SOURCE).as_deref(), Some("K"));
        assert_eq!(msg.get_field(tag::SYMBOL).as_deref(), Some("BTCUSDT"));
        assert_eq!(msg.get_field(tag::SIDE).as_deref(), Some("1")); // buy
        assert_eq!(msg.get_field(tag::ORD_TYPE).as_deref(), Some("2")); // limit
        assert_eq!(msg.get_field(tag::PRICE).as_deref(), Some("101.5"));
    }

    #[test]
    fn binance_market_order_omits_price_and_tif() {
        let id = Uuid::new_v4();
        let d = BinanceDialect::new("key".into(), TEST_ED25519_PEM).unwrap();
        let msg = d.build_new_order_single(&order(&id, "market", None)).unwrap();

        assert_eq!(msg.with_header(|h| h.get_field(tag::MSG_TYPE)).as_deref(), Some("D"));
        assert_eq!(msg.get_field(tag::ORD_TYPE).as_deref(), Some("1")); // market
        assert_eq!(msg.get_field(tag::PRICE), None);
        assert_eq!(msg.get_field(tag::TIME_IN_FORCE), None);
    }

    #[test]
    fn limit_without_price_is_rejected() {
        let id = Uuid::new_v4();
        let d = IbkrDialect::new("pw".into());
        assert!(d.build_new_order_single(&order(&id, "limit", None)).is_err());
    }

    #[test]
    fn parses_new_ack() {
        let id = Uuid::new_v4();
        let msg = exec_report(&[
            (tag::CL_ORD_ID, &id.to_string()),
            (tag::ORDER_ID, "BROKER-99"),
            (tag::EXEC_TYPE, "0"),
        ]);
        let p = parse_standard_exec_report(&msg, "BINANCE").unwrap();
        assert_eq!(p.order_id, id);
        assert!(p.is_ack);
        assert!(p.report.is_none());
        assert_eq!(p.external_order_id.as_deref(), Some("BROKER-99"));
    }

    #[test]
    fn parses_fill_44_trade_and_42_fill() {
        let id = Uuid::new_v4();
        for exec_type in ["F", "2", "1"] {
            let msg = exec_report(&[
                (tag::CL_ORD_ID, &id.to_string()),
                (tag::EXEC_TYPE, exec_type),
                (tag::LAST_QTY, "0.5"),
                (tag::LAST_PX, "100.25"),
                (tag::EXEC_ID, "E-1"),
            ]);
            let p = parse_standard_exec_report(&msg, "BINANCE").unwrap();
            match p.report {
                Some(ExecutionReport::Fill { fill_qty, fill_price, execution_id, venue }) => {
                    assert_eq!(fill_qty, 0.5);
                    assert_eq!(fill_price, 100.25);
                    assert_eq!(execution_id, "E-1");
                    assert_eq!(venue, "BINANCE");
                }
                other => panic!("exec_type {exec_type}: expected Fill, got {other:?}"),
            }
        }
    }

    #[test]
    fn parses_reject_sets_reason_and_report() {
        let id = Uuid::new_v4();
        let msg = exec_report(&[
            (tag::CL_ORD_ID, &id.to_string()),
            (tag::EXEC_TYPE, "8"),
            (tag::TEXT, "insufficient balance"),
        ]);
        let p = parse_standard_exec_report(&msg, "IBKR").unwrap();
        assert_eq!(p.reject_reason.as_deref(), Some("insufficient balance"));
        assert!(matches!(p.report, Some(ExecutionReport::Reject { .. })));
    }

    #[test]
    fn cancel_keys_on_orig_cl_ord_id() {
        let orig = Uuid::new_v4();
        let msg = exec_report(&[
            (tag::ORIG_CL_ORD_ID, &orig.to_string()),
            (tag::CL_ORD_ID, "cxl-BROKER-99"), // the cancel's own id, not a UUID
            (tag::EXEC_TYPE, "4"),
        ]);
        let p = parse_standard_exec_report(&msg, "BINANCE").unwrap();
        assert_eq!(p.order_id, orig, "must key on the original order, not the cancel id");
        assert!(matches!(p.report, Some(ExecutionReport::Canceled { .. })));
    }

    #[test]
    fn ignores_non_exec_report() {
        // A heartbeat (35=0) is not an execution report.
        let mut m = Message::new();
        m.with_header_mut(|h| h.set_field(tag::MSG_TYPE, "0")).unwrap();
        assert!(parse_standard_exec_report(&m, "IBKR").is_none());
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

/// Parse a standard FIX `ExecutionReport` (35=8). The field tags used here —
/// `ExecType`(150), `OrdStatus`(39), `LastQty`(32)/`LastPx`(31), `OrderID`(37),
/// `ExecID`(17) — are common to FIX 4.2 and 4.4, so both dialects share this. The
/// only version wrinkle is `ExecType`: 4.2 fills report `1`/`2`, 4.4 reports `F`.
pub fn parse_standard_exec_report(msg: &Message, venue: &str) -> Option<ParsedExecReport> {
    // Only execution reports; MsgType lives in the header.
    if msg.with_header(|h| h.get_field(tag::MSG_TYPE)).as_deref() != Some("8") {
        return None;
    }

    // Prefer OrigClOrdID (present on cancel reports) so a cancel keys on the
    // original order, matching the WS execution path.
    let cl = msg
        .get_field(tag::ORIG_CL_ORD_ID)
        .filter(|s| !s.is_empty())
        .or_else(|| msg.get_field(tag::CL_ORD_ID))?;
    let order_id = Uuid::parse_str(&cl).ok()?;

    let exec_type = msg.get_field(tag::EXEC_TYPE).unwrap_or_default();
    let external_order_id = msg.get_field(tag::ORDER_ID).filter(|s| !s.is_empty());

    let mut out = ParsedExecReport {
        order_id,
        external_order_id: external_order_id.clone(),
        is_ack: false,
        report: None,
        reject_reason: None,
    };

    match exec_type.as_str() {
        // New — the ack a submit_order is waiting for. No OMS state change.
        "0" => {
            out.is_ack = true;
        }
        // Fill / partial fill. 4.2: 1=partial, 2=fill; 4.4: F=Trade.
        "1" | "2" | "F" => {
            let fill_qty = msg.get_field(tag::LAST_QTY).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let fill_price = msg.get_field(tag::LAST_PX).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let execution_id = msg
                .get_field(tag::EXEC_ID)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| cl.clone());
            out.report = Some(ExecutionReport::Fill {
                execution_id,
                fill_qty,
                fill_price,
                venue: venue.to_string(),
            });
        }
        // Canceled.
        "4" => {
            out.report = Some(ExecutionReport::Canceled {
                reason: msg.get_field(tag::TEXT),
                venue: Some(venue.to_string()),
            });
        }
        // Rejected — also fails a pending submit_order.
        "8" => {
            let reason = msg
                .get_field(tag::TEXT)
                .or_else(|| msg.get_field(tag::ORD_REJ_REASON))
                .unwrap_or_else(|| "rejected".to_string());
            out.reject_reason = Some(reason.clone());
            out.report = Some(ExecutionReport::Reject {
                reason,
                venue: Some(venue.to_string()),
            });
        }
        // Everything else (Replaced, PendingCancel, …) — no OMS state change.
        _ => {}
    }

    Some(out)
}
