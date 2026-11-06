//! FIX 4.4 messages ⇄ [`ClientRequest`] / [`ClientReport`]. Pure: no sessions.

use quickfix::{FieldMap, Group, Message, QuickFixError};

use super::{ClientReport, ClientRequest, ExecType, OrdStatus, OrdType, Side, Tif, BEGIN_STRING};

const MSG_TYPE: i32 = 35;

fn msg_type(m: &Message) -> String {
    m.with_header(|h| h.get_field(MSG_TYPE)).unwrap_or_default()
}

fn reject(ref_msg_type: &str, text: impl Into<String>) -> ClientReport {
    ClientReport::BusinessReject { ref_msg_type: ref_msg_type.to_string(), text: text.into() }
}

fn required(m: &Message, tag: i32, t: &str) -> Result<String, ClientReport> {
    m.get_field(tag).filter(|v| !v.is_empty()).ok_or_else(|| reject(t, format!("tag {tag} is required")))
}

fn number(m: &Message, tag: i32, t: &str) -> Result<f64, ClientReport> {
    required(m, tag, t)?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .ok_or_else(|| reject(t, format!("tag {tag} is not a number")))
}

/// A request from a strategy, or the Business Message Reject (35=j) to answer
/// it with.
pub fn decode(m: &Message) -> Result<ClientRequest, ClientReport> {
    let t = msg_type(m);
    match t.as_str() {
        "D" => {
            let side = match required(m, 54, &t)?.as_str() {
                "1" => Side::Buy,
                "2" => Side::Sell,
                other => return Err(reject(&t, format!("side {other} is not supported"))),
            };
            let ord_type = match required(m, 40, &t)?.as_str() {
                "1" => OrdType::Market,
                "2" => OrdType::Limit,
                other => return Err(reject(&t, format!("order type {other} is not supported"))),
            };
            let tif = match m.get_field(59).as_deref().unwrap_or("0") {
                "0" => Tif::Day,
                "1" => Tif::Gtc,
                "3" => Tif::Ioc,
                other => return Err(reject(&t, format!("time in force {other} is not supported"))),
            };
            let price = match ord_type {
                OrdType::Limit => Some(number(m, 44, &t)?),
                OrdType::Market => None,
            };
            Ok(ClientRequest::NewOrder {
                cl_ord_id: required(m, 11, &t)?,
                symbol: required(m, 55, &t)?,
                ex_destination: required(m, 100, &t)?,
                side,
                qty: number(m, 38, &t)?,
                ord_type,
                price,
                tif,
            })
        }
        "F" => Ok(ClientRequest::Cancel { cl_ord_id: required(m, 11, &t)?, orig_cl_ord_id: required(m, 41, &t)? }),
        "H" => Ok(ClientRequest::Status { cl_ord_id: required(m, 11, &t)? }),
        "x" => Ok(ClientRequest::SecurityList { req_id: required(m, 320, &t)? }),
        other => Err(reject(other, format!("message type {other} is not supported"))),
    }
}

fn side_code(s: Side) -> &'static str {
    match s {
        Side::Buy => "1",
        Side::Sell => "2",
    }
}

fn exec_type_code(e: ExecType) -> &'static str {
    match e {
        ExecType::New => "0",
        ExecType::Canceled => "4",
        ExecType::Rejected => "8",
        ExecType::Expired => "C",
        ExecType::Trade => "F",
        ExecType::OrderStatus => "I",
    }
}

fn ord_status_code(s: OrdStatus) -> &'static str {
    match s {
        OrdStatus::New => "0",
        OrdStatus::PartiallyFilled => "1",
        OrdStatus::Filled => "2",
        OrdStatus::Canceled => "4",
        OrdStatus::Rejected => "8",
        OrdStatus::Expired => "C",
    }
}

fn with_type(t: &str) -> Result<Message, QuickFixError> {
    let mut m = Message::new();
    m.with_header_mut(|h| {
        h.set_field(8, BEGIN_STRING)?;
        h.set_field(MSG_TYPE, t)
    })?;
    Ok(m)
}

/// The FIX message for a report; the session fills the header's CompIDs.
pub fn encode(report: &ClientReport) -> Result<Message, QuickFixError> {
    match report {
        ClientReport::Exec {
            cl_ord_id, order_id, exec_id, exec_type, ord_status, symbol, side,
            order_qty, cum_qty, leaves_qty, avg_px, last_qty, last_px, text,
        } => {
            let mut m = with_type("8")?;
            m.set_field(11, cl_ord_id.as_str())?;
            m.set_field(37, order_id.as_str())?;
            m.set_field(17, exec_id.as_str())?;
            m.set_field(150, exec_type_code(*exec_type))?;
            m.set_field(39, ord_status_code(*ord_status))?;
            m.set_field(55, symbol.as_str())?;
            m.set_field(54, side_code(*side))?;
            m.set_field(38, order_qty.to_string())?;
            m.set_field(14, cum_qty.to_string())?;
            m.set_field(151, leaves_qty.to_string())?;
            m.set_field(6, avg_px.to_string())?;
            if let Some(q) = last_qty {
                m.set_field(32, q.to_string())?;
            }
            if let Some(p) = last_px {
                m.set_field(31, p.to_string())?;
            }
            if let Some(text) = text {
                m.set_field(58, text.as_str())?;
            }
            Ok(m)
        }
        ClientReport::CancelReject { cl_ord_id, orig_cl_ord_id, text } => {
            let mut m = with_type("9")?;
            m.set_field(11, cl_ord_id.as_str())?;
            m.set_field(41, orig_cl_ord_id.as_str())?;
            m.set_field(37, "NONE")?;
            m.set_field(39, "8")?;
            m.set_field(434, "1")?;
            m.set_field(58, text.as_str())?;
            Ok(m)
        }
        ClientReport::SecurityList { req_id, securities } => {
            let mut m = with_type("y")?;
            m.set_field(320, req_id.as_str())?;
            m.set_field(322, req_id.as_str())?;
            m.set_field(560, "0")?;
            for (ex_destination, symbol, id) in securities {
                let mut g = Group::try_with_orders(146, 55, &[55, 48, 22, 100])?;
                g.set_field(55, symbol.as_str())?;
                g.set_field(48, id.to_string())?;
                g.set_field(22, "8")?;
                g.set_field(100, ex_destination.as_str())?;
                m.add_group(&g)?;
            }
            Ok(m)
        }
        ClientReport::BusinessReject { ref_msg_type, text } => {
            let mut m = with_type("j")?;
            m.set_field(372, ref_msg_type.as_str())?;
            m.set_field(380, "3")?;
            m.set_field(58, text.as_str())?;
            Ok(m)
        }
    }
}

#[cfg(test)]
mod tests {
    use quickfix::{FieldMap, Message};

    use super::*;
    use crate::fix_acceptor::{ClientReport, ClientRequest, ExecType, OrdStatus, OrdType, Side, Tif};

    fn msg(text: &str) -> Message {
        Message::try_from_text(&text.replace('|', "\u{1}")).unwrap()
    }

    #[test]
    fn a_new_order_single_decodes_every_field() {
        let m = msg("8=FIX.4.4|9=1|35=D|11=c1|55=BTCUSDT|100=BINANCE|54=1|38=0.5|40=2|44=100.25|59=1|10=000|");
        match decode(&m) {
            Ok(ClientRequest::NewOrder { cl_ord_id, symbol, ex_destination, side, qty, ord_type, price, tif }) => {
                assert_eq!((cl_ord_id.as_str(), symbol.as_str(), ex_destination.as_str()), ("c1", "BTCUSDT", "BINANCE"));
                assert_eq!((side, qty, ord_type, price, tif), (Side::Buy, 0.5, OrdType::Limit, Some(100.25), Tif::Gtc));
            }
            other => panic!("expected a new order, got {other:?}"),
        }
    }

    #[test]
    fn a_non_finite_quantity_or_price_is_rejected() {
        for bad in ["38=NaN|44=100", "38=inf|44=100", "38=1|44=NaN", "38=1|44=-inf"] {
            let m = msg(&format!("8=FIX.4.4|9=1|35=D|11=c1|55=X|100=Y|54=1|{bad}|40=2|59=1|10=000|"));
            assert!(matches!(decode(&m), Err(ClientReport::BusinessReject { .. })), "{bad} accepted");
        }
    }

    #[test]
    fn an_unknown_message_type_is_a_business_reject() {
        let m = msg("8=FIX.4.4|9=1|35=G|11=c2|41=c1|10=000|");
        match decode(&m) {
            Err(ClientReport::BusinessReject { ref_msg_type, text }) => {
                assert_eq!(ref_msg_type, "G");
                assert!(text.contains("not supported"), "{text}");
            }
            other => panic!("expected a business reject, got {other:?}"),
        }
    }

    #[test]
    fn a_rejected_exec_carries_its_reason_in_58() {
        let report = ClientReport::Exec {
            cl_ord_id: "c1".into(),
            order_id: "NONE".into(),
            exec_id: "x1".into(),
            exec_type: ExecType::Rejected,
            ord_status: OrdStatus::Rejected,
            symbol: "BTCUSDT".into(),
            side: Side::Buy,
            order_qty: 1.0,
            cum_qty: 0.0,
            leaves_qty: 0.0,
            avg_px: 0.0,
            last_qty: None,
            last_px: None,
            text: Some("max_order_notional: 12000 > 10000".into()),
        };
        let m = encode(&report).unwrap();
        assert_eq!(m.with_header(|h| h.get_field(35)).as_deref(), Some("8"));
        assert_eq!(m.get_field(150).as_deref(), Some("8"));
        assert_eq!(m.get_field(39).as_deref(), Some("8"));
        assert_eq!(m.get_field(58).as_deref(), Some("max_order_notional: 12000 > 10000"));
        assert_eq!(m.get_field(11).as_deref(), Some("c1"));
    }
}
