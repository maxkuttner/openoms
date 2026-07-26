//! Binance Spot FIX dialect (FIX 4.4, Order-Entry session).
//!
//! Auth is Ed25519: the Logon (35=A) carries the API key in `Username`(553) and a
//! signature in `RawData`(96) over the SOH-joined values of
//! `MsgType|SenderCompID|TargetCompID|MsgSeqNum|SendingTime` — exactly the header
//! fields QuickFIX has already stamped by the time `augment_logon` runs. TLS is
//! required (the session is started with an SSL server-kind).

use ed25519_dalek::pkcs8::DecodePrivateKey;
use ed25519_dalek::{Signer, SigningKey};

use base64::Engine;
use quickfix::{FieldMap, Message};

use crate::adapters::{BrokerError, BrokerOrderRequest};
use crate::fix::dialect::{
    fix_ord_type, fix_side, fix_time_in_force, parse_standard_exec_report, tag, FixDialect,
    ParsedExecReport,
};

/// Binance-custom Logon field: how the server handles message ordering. `1` =
/// unordered (recommended for order entry).
const MESSAGE_HANDLING: i32 = 25035;

pub struct BinanceDialect {
    api_key: String,
    signing_key: SigningKey,
}

impl BinanceDialect {
    /// `private_key_pem` is the Ed25519 PKCS#8 PEM whose public key is registered
    /// on Binance for `api_key` — the same credential the REST/WS adapter uses.
    pub fn new(api_key: String, private_key_pem: &str) -> Result<Self, BrokerError> {
        let signing_key = SigningKey::from_pkcs8_pem(private_key_pem)
            .map_err(|e| BrokerError::NotConfigured(format!("invalid Ed25519 key PEM: {e}")))?;
        Ok(Self { api_key, signing_key })
    }

    fn sign(&self, payload: &str) -> String {
        let sig = self.signing_key.sign(payload.as_bytes());
        base64::engine::general_purpose::STANDARD.encode(sig.to_bytes())
    }
}

impl FixDialect for BinanceDialect {
    fn begin_string(&self) -> &'static str {
        "FIX.4.4"
    }

    fn venue(&self) -> &'static str {
        "BINANCE"
    }

    fn augment_logon(&self, msg: &mut Message) {
        // Read the header fields QuickFIX has already set.
        let get = |m: &Message, t: i32| m.with_header(|h| h.get_field(t)).unwrap_or_default();
        let sender = get(msg, 49);
        let target = get(msg, 56);
        let seq = get(msg, 34);
        let sending_time = get(msg, 52);

        // Signature payload: the field VALUES joined by SOH, MsgType first.
        let payload = format!("A\x01{sender}\x01{target}\x01{seq}\x01{sending_time}");
        let sig = self.sign(&payload);

        let _ = msg.set_field(tag::USERNAME, self.api_key.as_str());
        let _ = msg.set_field(tag::RAW_DATA_LENGTH, sig.len() as i32);
        let _ = msg.set_field(tag::RAW_DATA, sig.as_str());
        let _ = msg.set_field(MESSAGE_HANDLING, 1i32);
    }

    fn build_new_order_single(&self, req: &BrokerOrderRequest) -> Result<Message, BrokerError> {
        let mut msg = Message::new();
        msg.with_header_mut(|h| h.set_field(tag::MSG_TYPE, "D"))
            .map_err(|e| BrokerError::Network(format!("set MsgType: {e}")))?;
        let set = |m: &mut Message, t: i32, v: &str| -> Result<(), BrokerError> {
            m.set_field(t, v).map_err(|e| BrokerError::Network(format!("set {t}: {e}")))
        };

        // NB: Binance's FIX dictionary does NOT accept TransactTime(60) on
        // NewOrderSingle/OrderCancelRequest — sending it draws a session Reject
        // (373=2, "Tag not defined for this message type"). So it is omitted here,
        // unlike the IBKR dialect.
        set(&mut msg, tag::SYMBOL, &req.symbol)?; // e.g. BTCUSDT
        set(&mut msg, tag::SIDE, fix_side(&req.side)?)?;
        set(&mut msg, tag::ORDER_QTY, &req.quantity.to_string())?;
        let ord_type = fix_ord_type(&req.order_type)?;
        set(&mut msg, tag::ORD_TYPE, ord_type)?;
        if req.order_type == "limit" {
            let px = req
                .limit_price
                .ok_or_else(|| BrokerError::NotConfigured("limit order without limit_price".into()))?;
            set(&mut msg, tag::PRICE, &px.to_string())?;
            // TimeInForce applies to limit orders; market orders omit it.
            set(&mut msg, tag::TIME_IN_FORCE, fix_time_in_force(&req.time_in_force))?;
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
        set(&mut msg, tag::CL_ORD_ID, &format!("cxl-{external_order_id}"))?;
        set(&mut msg, tag::ORDER_ID, external_order_id)?;
        set(&mut msg, tag::SYMBOL, symbol)?;
        Ok(msg)
    }

    fn parse_execution_report(&self, msg: &Message) -> Option<ParsedExecReport> {
        parse_standard_exec_report(msg, self.venue())
    }
}
