//! IBKR FIX dialect (FIX 4.2 / CTCI).
//!
//! IBKR's institutional FIX gateway speaks FIX 4.2. Logon (35=A) carries the
//! account password; orders route by IBKR `conId` (the immutable contract id),
//! which the OMS already stores as `broker_instrument.native_id` and passes on
//! `BrokerOrderRequest.native_id`. Falls back to the ticker symbol when no conId
//! is present.

use quickfix::{FieldMap, Message};

use crate::adapters::{BrokerError, BrokerOrderRequest};
use crate::fix::dialect::{
    fix_ord_type, fix_side, fix_time_in_force, parse_standard_exec_report, tag, transact_time,
    FixDialect, ParsedExecReport,
};

pub struct IbkrDialect {
    /// FIX account password, sent in the Logon.
    password: String,
}

impl IbkrDialect {
    pub fn new(password: String) -> Self {
        Self { password }
    }
}

impl FixDialect for IbkrDialect {
    fn begin_string(&self) -> &'static str {
        "FIX.4.2"
    }

    fn venue(&self) -> &'static str {
        "IBKR"
    }

    fn augment_logon(&self, msg: &mut Message) {
        // Username(553)/Password(554) — IBKR's standard credential fields.
        let _ = msg.set_field(tag::PASSWORD, self.password.as_str());
    }

    fn build_new_order_single(&self, req: &BrokerOrderRequest) -> Result<Message, BrokerError> {
        let mut msg = Message::new();
        msg.with_header_mut(|h| h.set_field(tag::MSG_TYPE, "D"))
            .map_err(|e| BrokerError::Network(format!("set MsgType: {e}")))?;

        let set = |m: &mut Message, t: i32, v: &str| -> Result<(), BrokerError> {
            m.set_field(t, v).map_err(|e| BrokerError::Network(format!("set {t}: {e}")))
        };

        // Route by conId when present (immutable), else by ticker.
        match &req.native_id {
            Some(conid) if !conid.is_empty() => {
                set(&mut msg, tag::SECURITY_ID, conid)?;
                set(&mut msg, tag::SECURITY_ID_SOURCE, "K")?; // K = IBKR conId
            }
            _ => {}
        }
        set(&mut msg, tag::SYMBOL, &req.symbol)?;
        set(&mut msg, tag::SIDE, fix_side(&req.side)?)?;
        set(&mut msg, tag::ORDER_QTY, &req.quantity.to_string())?;
        set(&mut msg, tag::ORD_TYPE, fix_ord_type(&req.order_type)?)?;
        set(&mut msg, tag::TIME_IN_FORCE, fix_time_in_force(&req.time_in_force))?;
        set(&mut msg, tag::TRANSACT_TIME, &transact_time())?;
        if req.order_type == "limit" {
            let px = req
                .limit_price
                .ok_or_else(|| BrokerError::NotConfigured("limit order without limit_price".into()))?;
            set(&mut msg, tag::PRICE, &px.to_string())?;
        }
        Ok(msg)
    }

    fn build_cancel(&self, external_order_id: &str, symbol: &str) -> Result<Message, BrokerError> {
        let mut msg = Message::new();
        msg.with_header_mut(|h| h.set_field(tag::MSG_TYPE, "F"))
            .map_err(|e| BrokerError::Network(format!("set MsgType: {e}")))?;
        let set = |m: &mut Message, t: i32, v: &str| -> Result<(), BrokerError> {
            m.set_field(t, v).map_err(|e| BrokerError::Network(format!("set {t}: {e}")))
        };
        // Cancel needs a fresh ClOrdID; the adapter's ClOrdID belongs to the new
        // order, so mint one here and reference the resting order by OrderID.
        set(&mut msg, tag::CL_ORD_ID, &format!("cxl-{external_order_id}"))?;
        set(&mut msg, tag::ORDER_ID, external_order_id)?;
        set(&mut msg, tag::SYMBOL, symbol)?;
        set(&mut msg, tag::TRANSACT_TIME, &transact_time())?;
        Ok(msg)
    }

    fn parse_execution_report(&self, msg: &Message) -> Option<ParsedExecReport> {
        parse_standard_exec_report(msg, self.venue())
    }
}
