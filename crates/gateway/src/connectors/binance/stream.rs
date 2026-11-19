//! Binance Spot user-data stream over the WebSocket API — real-time
//! `executionReport` push, mirroring the Alpaca stream (no polling).
//!
//! Binance retired the REST `listenKey` stream; the current mechanism is:
//! connect the WS API, `session.logon` with an Ed25519 signature, then
//! `userDataStream.subscribe`. Events arrive wrapped as
//! `{"subscriptionId":N,"event":{"e":"executionReport",...}}`. A `TRADE`
//! execution fires per fill (including partials), so fills reflect immediately
//! and incrementally, as execution events to the order engine.

use std::sync::Arc;

use chrono::{TimeZone, Utc};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tracing::{info, warn};

use crate::connectors::binance::adapter::BinanceAdapter;
use crate::connectors::binance::runtime::{
    health_event, runtime_events,
};
use crate::runtime::{
    AccountIdentity, StreamState, TradeProvenance, VenueEvent,
};
use crate::stream_health::StreamHandle;
use crate::stream_supervisor::{supervise, Session, StreamResult};
use crate::venue_event::{VenueEvent as VEvent, VenueFill, VenueOrderStatus, VenueOrderUpdate};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How often to ping the socket. A stalled user-data subscription (fills stop
/// arriving without a Close frame) is otherwise invisible; the ping surfaces a
/// dead socket, and the reconnect's `Connected` makes the reconciler pull the gap.
const HEARTBEAT_SECS: u64 = 30;

struct BinanceSession {
    adapter: Arc<BinanceAdapter>,
    health: StreamHandle,
    sink: mpsc::Sender<VEvent>,
}

#[async_trait::async_trait]
impl Session for BinanceSession {
    async fn run_once(&mut self) -> StreamResult {
        let result = connect_and_run(&self.adapter, &self.health, &self.sink).await;
        let _ = self.sink.send(VEvent::Disconnected).await;
        result
    }
}

/// Credentials are not taken here: the adapter already holds them (it signs with
/// an Ed25519 key loaded at construction), which is why the old `_api_key` /
/// `_api_secret` parameters were unused.
pub async fn run(
    environment: &'static str,
    adapter: Arc<BinanceAdapter>,
    health: StreamHandle,
    tx: mpsc::Sender<VEvent>,
) {
    info!(env = environment, "starting Binance user-data stream");
    // Every (re)subscribe sends `Connected`, on which the reconciler pulls the gap.
    let session = BinanceSession {
        adapter,
        health: health.clone(),
        sink: tx,
    };
    let name: &'static str = if environment == "LIVE" { "BINANCE/LIVE" } else { "BINANCE/PAPER" };
    supervise(name, health, session).await
}

/// Runtime push stream: health, orders and fees reach `tx`. Reconnects forever
/// with an explicit stale/closed health event so the runtime can suspend.
pub(crate) async fn runtime(
    identity: AccountIdentity,
    adapter: Arc<BinanceAdapter>,
    tx: mpsc::Sender<VenueEvent>,
) {
    loop {
        match runtime_once(&identity, &adapter, &tx).await {
            Ok(()) => {
                if tx.send(health_event(StreamState::Closed)).await.is_err() {
                    return;
                }
            }
            Err(e) => {
                warn!(error = %e, "Binance runtime stream failed");
                if tx.send(health_event(StreamState::Stale)).await.is_err() {
                    return;
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn runtime_once(
    identity: &AccountIdentity,
    adapter: &Arc<BinanceAdapter>,
    tx: &mpsc::Sender<VenueEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (mut ws, _) = connect_async(adapter.ws_url()).await?;
    info!("Binance WS-API connected for runtime");
    handshake(&mut ws, adapter).await?;
    info!("Binance WS-API runtime subscribed");
    if tx.send(health_event(StreamState::Live)).await.is_err() {
        return Ok(());
    }

    let mut heartbeat = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECS));
    heartbeat.tick().await;
    loop {
        let text = tokio::select! {
            _ = heartbeat.tick() => {
                ws.send(Message::Ping(Vec::new())).await?;
                continue;
            }
            msg = ws.next() => {
                let Some(msg) = msg else { return Ok(()) };
                match msg? {
                    Message::Text(t) => t,
                    Message::Binary(b) => match String::from_utf8(b) {
                        Ok(s) => s,
                        Err(_) => continue,
                    },
                    Message::Ping(p) => { ws.send(Message::Pong(p)).await?; continue; }
                    Message::Close(_) => return Ok(()),
                    _ => continue,
                }
            }
        };
        let v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => { warn!(error = %e, "Binance runtime WS: parse failed"); continue; }
        };
        if v.get("event").is_some() && v["event"]["e"].as_str() == Some("executionReport") {
            for event in runtime_events(&v["event"], identity, TradeProvenance::Stream) {
                if tx.send(event).await.is_err() {
                    return Ok(());
                }
            }
        }
    }
}

/// Authenticate and subscribe. The caller owns the read loop after this.
async fn handshake(
    ws: &mut Ws,
    adapter: &BinanceAdapter,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let ts = Utc::now().timestamp_millis();
    let payload = format!("apiKey={}&timestamp={}", adapter.api_key(), ts);
    let logon = serde_json::json!({
        "id": "logon",
        "method": "session.logon",
        "params": { "apiKey": adapter.api_key(), "timestamp": ts, "signature": adapter.sign(&payload) }
    });
    ws.send(Message::Text(logon.to_string())).await?;
    wait_ack(ws, "logon", "session.logon").await?;

    let sub = serde_json::json!({ "id": "sub", "method": "userDataStream.subscribe" });
    ws.send(Message::Text(sub.to_string())).await?;
    wait_ack(ws, "sub", "userDataStream.subscribe").await
}

async fn wait_ack(
    ws: &mut Ws,
    id: &str,
    label: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    loop {
        let Some(msg) = ws.next().await else {
            return Err(format!("{label}: socket closed before acknowledgement").into());
        };
        match msg? {
            Message::Text(t) => {
                let v: serde_json::Value = serde_json::from_str(&t)?;
                if v["id"].as_str() == Some(id) {
                    if v["status"].as_i64() == Some(200) {
                        return Ok(());
                    }
                    return Err(format!("{label} failed: {v}").into());
                }
            }
            Message::Binary(b) => {
                let Ok(t) = String::from_utf8(b) else { continue };
                let v: serde_json::Value = serde_json::from_str(&t)?;
                if v["id"].as_str() == Some(id) {
                    if v["status"].as_i64() == Some(200) {
                        return Ok(());
                    }
                    return Err(format!("{label} failed: {v}").into());
                }
            }
            Message::Ping(p) => ws.send(Message::Pong(p)).await?,
            Message::Close(_) => {
                return Err(format!("{label}: socket closed before acknowledgement").into());
            }
            _ => {}
        }
    }
}


async fn connect_and_run(
    adapter: &Arc<BinanceAdapter>,
    health: &StreamHandle,
    sink: &mpsc::Sender<VEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (mut ws, _) = connect_async(adapter.ws_url()).await?;
    info!("Binance WS-API connected");

    // session.logon — Ed25519 signature over the alphabetically-sorted params.
    let ts = Utc::now().timestamp_millis();
    let payload = format!("apiKey={}&timestamp={}", adapter.api_key(), ts);
    let logon = serde_json::json!({
        "id": "logon",
        "method": "session.logon",
        "params": { "apiKey": adapter.api_key(), "timestamp": ts, "signature": adapter.sign(&payload) }
    });
    ws.send(Message::Text(logon.to_string())).await?;

    let mut heartbeat = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECS));
    heartbeat.tick().await; // consume the immediate first tick

    loop {
        let text = tokio::select! {
            // Periodic liveness probe; the reconciler polls for missed fills on its own timer.
            _ = heartbeat.tick() => {
                ws.send(Message::Ping(Vec::new())).await?;
                continue;
            }
            msg = ws.next() => {
                let Some(msg) = msg else { break };
                health.record_event();
                match msg? {
                    Message::Text(t) => t,
                    Message::Binary(b) => match String::from_utf8(b) {
                        Ok(s) => s,
                        Err(_) => continue,
                    },
                    Message::Ping(p) => { ws.send(Message::Pong(p)).await?; continue; }
                    Message::Close(_) => break,
                    _ => continue,
                }
            }
        };

        let v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => { warn!(error = %e, raw = %text, "Binance WS: parse failed"); continue; }
        };

        // Pushed user-data event: {"subscriptionId":N,"event":{...}}
        if v.get("event").is_some() {
            if v["event"]["e"].as_str() == Some("executionReport") {
                for event in venue_events(&v["event"]) {
                    if sink.send(event).await.is_err() {
                        return Ok(());
                    }
                }
            }
            continue;
        }

        // Otherwise it's a request response, keyed by our id.
        match v["id"].as_str() {
            Some("logon") => {
                if v["status"].as_i64() == Some(200) {
                    info!("Binance WS-API session authenticated");
                    let sub = serde_json::json!({ "id": "sub", "method": "userDataStream.subscribe" });
                    ws.send(Message::Text(sub.to_string())).await?;
                } else {
                    return Err(format!("session.logon failed: {}", v).into());
                }
            }
            Some("sub") => {
                if v["status"].as_i64() == Some(200) {
                    info!("Binance WS-API subscribed to user-data stream");
                    health.set_live();
                    // The reconciler pulls fills and the open-order snapshot
                    // for whatever happened while the stream was down.
                    if sink.send(VEvent::Connected).await.is_err() {
                        return Ok(());
                    }
                } else {
                    return Err(format!("userDataStream.subscribe failed: {}", v).into());
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// What one `executionReport` on the OMS's own account means, whoever placed
/// the order: a `TRADE` is a fill (its commission `n`/`N` rides along), and
/// every report is also the order's current state. The stream does not know
/// the account's reference: the connection's reconciler stamps it (its only
/// account), and a co-located follow sets its sub-account's.
pub(crate) fn venue_events(event: &serde_json::Value) -> Vec<VEvent> {
    let mut out = Vec::new();
    // On a cancel, Binance puts the cancel-request's id in `c` and the original
    // order's client id in `C`; otherwise the order's id is in `c`. Prefer `C`.
    let client_order_id = match event["C"].as_str() {
        Some(c) if !c.is_empty() => Some(c.to_string()),
        _ => event["c"].as_str().filter(|c| !c.is_empty()).map(str::to_string),
    };
    let num = |key: &str| event[key].as_str().and_then(|s| s.parse::<f64>().ok());
    let venue_order_id = event["i"].as_i64().map(|i| i.to_string()).unwrap_or_default();
    let symbol = event["s"].as_str().unwrap_or("").to_string();
    let buy = event["S"].as_str() == Some("BUY");
    let ts = |key: &str| event[key].as_i64().and_then(|ms| Utc.timestamp_millis_opt(ms).single());
    let exec_type = event["x"].as_str().unwrap_or("");
    if exec_type == "TRADE" {
        if let (Some(trade_id), Some(qty), Some(price)) = (event["t"].as_i64(), num("l"), num("L")) {
            out.push(VEvent::Fill(VenueFill {
                exec_id: super::fill_exec_id(&symbol, &trade_id.to_string(), buy),
                venue_order_id: venue_order_id.clone(),
                client_order_id: client_order_id.clone(),
                account_ref: String::new(),
                symbol: symbol.clone(),
                buy,
                qty,
                price,
                fee: match (num("n"), event["N"].as_str()) {
                    (Some(amount), Some(ccy)) if amount != 0.0 && !ccy.is_empty() => Some((amount, ccy.to_string())),
                    _ => None,
                },
                executed_at: ts("T").or_else(|| ts("E")),
            }));
        } else {
            warn!(?event, "Binance WS: TRADE report without trade id, qty or price");
        }
    }
    let status = match event["X"].as_str().unwrap_or(exec_type) {
        "NEW" | "PARTIALLY_FILLED" | "PENDING_NEW" => VenueOrderStatus::Working,
        "FILLED" => VenueOrderStatus::Filled,
        "CANCELED" | "PENDING_CANCEL" => VenueOrderStatus::Canceled,
        "REJECTED" => VenueOrderStatus::Rejected {
            reason: event["r"].as_str().filter(|r| !r.is_empty() && *r != "NONE").unwrap_or("REJECTED").to_string(),
        },
        "EXPIRED" | "EXPIRED_IN_MATCH" => VenueOrderStatus::Expired,
        other => {
            warn!(status = other, "Binance WS: unknown order status, no order update");
            return out;
        }
    };
    let order_type = event["o"].as_str().unwrap_or("").to_string();
    out.push(VEvent::Order(VenueOrderUpdate {
        venue_order_id,
        client_order_id,
        account_ref: String::new(),
        symbol,
        buy,
        limit_px: if order_type == "MARKET" { None } else { num("p").filter(|p| *p != 0.0) },
        order_type,
        qty: num("q").unwrap_or(0.0),
        status,
        cum_qty: num("z").unwrap_or(0.0),
        ts: ts("E").or_else(|| ts("T")),
    }));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use crate::venue_event::{VenueEvent as VEvent, VenueFill, VenueOrderStatus};

    #[test]
    fn trade_on_non_uuid_client_id_is_a_venue_fill() {
        let event = serde_json::json!({
            "e": "executionReport", "x": "TRADE", "X": "PARTIALLY_FILLED", "c": "mm1-q-000001",
            "s": "BTCUSDT", "S": "BUY", "o": "LIMIT", "i": 9001, "q": "1.0", "p": "64000.5", "z": "0.25",
            "l": "0.5", "L": "64000.1", "t": 77, "n": "0.01", "N": "BNB", "T": 1700000000000i64,
        });
        let events = venue_events(&event);
        assert_eq!(
            events[0],
            VEvent::Fill(VenueFill {
                exec_id: "BTCUSDT:77:B".into(), venue_order_id: "9001".into(), client_order_id: Some("mm1-q-000001".into()),
                account_ref: String::new(), symbol: "BTCUSDT".into(), buy: true, qty: 0.5, price: 64000.1,
                fee: Some((0.01, "BNB".into())),
                executed_at: Some(Utc.timestamp_millis_opt(1700000000000).unwrap()),
            })
        );
        let VEvent::Order(o) = &events[1] else { panic!("an order update after the fill") };
        assert_eq!((o.status.clone(), o.cum_qty), (VenueOrderStatus::Working, 0.25));
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn zero_commission_trade_carries_no_fee() {
        let event = serde_json::json!({
            "e": "executionReport", "x": "TRADE", "X": "FILLED", "c": "x", "s": "BTCUSDT", "S": "SELL",
            "i": 1, "l": "0.5", "L": "1", "t": 2, "n": "0", "N": "BNB",
        });
        let VEvent::Fill(f) = &venue_events(&event)[0] else { panic!("a fill") };
        assert_eq!(f.fee, None);
    }

    #[test]
    #[allow(non_snake_case)]
    fn cancel_uses_orig_client_id_C() {
        let cancel = serde_json::json!({
            "e": "executionReport", "x": "CANCELED", "X": "CANCELED", "c": "cancel-req", "C": "mm1-q-000002",
            "s": "BTCUSDT", "S": "SELL", "o": "LIMIT", "i": 9002, "q": "0.5", "p": "64100", "z": "0",
        });
        let events = venue_events(&cancel);
        assert_eq!(events.len(), 1);
        let VEvent::Order(o) = &events[0] else { panic!("an order update") };
        assert_eq!(o.client_order_id.as_deref(), Some("mm1-q-000002"));
        assert_eq!(o.venue_order_id, "9002");
        assert_eq!(o.status, VenueOrderStatus::Canceled);
    }

    #[test]
    fn new_order_report_is_order_update_working() {
        let new = serde_json::json!({
            "e": "executionReport", "x": "NEW", "X": "NEW", "c": "mm1-q-000003", "s": "ETHUSDT", "S": "BUY",
            "o": "LIMIT", "i": 9003, "q": "2", "p": "3000", "z": "0", "E": 1700000000000i64,
        });
        let events = venue_events(&new);
        assert_eq!(events.len(), 1);
        let VEvent::Order(o) = &events[0] else { panic!("an order update") };
        assert_eq!(o.status, VenueOrderStatus::Working);
        assert_eq!((o.symbol.as_str(), o.buy, o.order_type.as_str(), o.qty, o.limit_px), ("ETHUSDT", true, "LIMIT", 2.0, Some(3000.0)));
        assert_eq!(o.client_order_id.as_deref(), Some("mm1-q-000003"));
    }

    #[test]
    fn rejected_and_expired_reports_close_the_order() {
        let rej = serde_json::json!({ "e": "executionReport", "x": "REJECTED", "X": "REJECTED", "r": "INSUFFICIENT_BALANCE", "c": "a", "s": "BTCUSDT", "S": "BUY", "i": 1, "q": "1", "z": "0" });
        let VEvent::Order(o) = &venue_events(&rej)[0] else { panic!() };
        assert_eq!(o.status, VenueOrderStatus::Rejected { reason: "INSUFFICIENT_BALANCE".into() });
        let exp = serde_json::json!({ "e": "executionReport", "x": "EXPIRED", "X": "EXPIRED", "c": "a", "s": "BTCUSDT", "S": "BUY", "i": 1, "q": "1", "z": "0", "o": "MARKET" });
        let VEvent::Order(o) = &venue_events(&exp)[0] else { panic!() };
        assert_eq!((o.status.clone(), o.limit_px), (VenueOrderStatus::Expired, None));
    }
}
