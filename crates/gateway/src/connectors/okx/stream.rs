//! OKX spot private stream (v5 WebSocket): real-time order push as venue events.
//!
//! Connect `wss://ws.okx.com/ws/v5/private` (Demo Trading: `wspap.okx.com`), log
//! in with `{"op":"login","args":[{apiKey, passphrase, timestamp, sign}]}` where
//! `sign` is the base64 HMAC of `{unix seconds}GET/users/self/verify`, then
//! subscribe to the `orders` channel for SPOT. Every push carries the order's
//! state; one that also carries a `tradeId` is a fill (`fillSz` at `fillPx`, its
//! fee in `fillFee`/`fillFeeCcy`). The `tradeId` is the same id `/trade/fills`
//! carries, so a fill the stream and the pull both see is one fill; a later push
//! repeating the last fill's `tradeId` (a cancel after a partial fill) repeats
//! the same exec id, which ingest drops.

use std::sync::Arc;

use chrono::{TimeZone, Utc};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};

use crate::connectors::okx::adapter::{fee_of, fill_of, order_update_from_row, OkxAdapter};
use crate::stream_health::StreamHandle;
use crate::stream_supervisor::{supervise, Session, StreamResult};
use crate::venue_event::VenueEvent as VEvent;

/// OKX drops a connection that sends nothing for 30s; each beat sends "ping".
const HEARTBEAT_SECS: u64 = 20;

struct OkxSession {
    adapter: Arc<OkxAdapter>,
    health: StreamHandle,
    tx: mpsc::Sender<VEvent>,
}

#[async_trait::async_trait]
impl Session for OkxSession {
    async fn run_once(&mut self) -> StreamResult {
        let result = connect_and_run(&self.adapter, &self.health, &self.tx).await;
        let _ = self.tx.send(VEvent::Disconnected).await;
        result
    }
}

pub async fn run(environment: &'static str, adapter: Arc<OkxAdapter>, health: StreamHandle, tx: mpsc::Sender<VEvent>) {
    info!(env = environment, "starting OKX private stream");
    let session = OkxSession { adapter, health: health.clone(), tx };
    let name: &'static str = if environment == "LIVE" { "OKX/LIVE" } else { "OKX/PAPER" };
    supervise(name, health, session).await
}

/// The `login` request, signed for `unix_secs`.
pub(crate) fn login_message(adapter: &OkxAdapter, unix_secs: i64) -> serde_json::Value {
    serde_json::json!({
        "op": "login",
        "args": [{
            "apiKey": adapter.api_key(),
            "passphrase": adapter.passphrase(),
            "timestamp": unix_secs.to_string(),
            "sign": adapter.sign(&format!("{unix_secs}GET/users/self/verify")),
        }],
    })
}

fn subscribe_message() -> serde_json::Value {
    serde_json::json!({"op": "subscribe", "args": [{"channel": "orders", "instType": "SPOT"}]})
}

/// What one `orders` push means, whoever placed the order: an entry with a
/// `tradeId` is a fill (with its fee), and every entry is the order's current
/// state. One OKX key is one account, which ingest resolves by the connection alone.
pub(crate) fn venue_events(msg: &serde_json::Value) -> Vec<VEvent> {
    if msg["arg"]["channel"].as_str() != Some("orders") {
        return Vec::new();
    }
    let mut out = Vec::new();
    for d in msg["data"].as_array().map(Vec::as_slice).unwrap_or_default() {
        if d["tradeId"].as_str().is_some_and(|t| !t.is_empty()) {
            let at = d["fillTime"]
                .as_str()
                .or(d["uTime"].as_str())
                .and_then(|s| s.parse::<i64>().ok())
                .and_then(|ms| Utc.timestamp_millis_opt(ms).single());
            match fill_of(d, at, fee_of(&d["fillFee"], &d["fillFeeCcy"]), "") {
                Ok(f) => out.push(VEvent::Fill(f)),
                Err(missing) => warn!(trade_id = d["tradeId"].as_str().unwrap_or(""), missing, "OKX WS: fill skipped"),
            }
        }
        match order_update_from_row(d, "") {
            Some(o) => out.push(VEvent::Order(o)),
            None => warn!(?d, "OKX WS: order entry with unknown state or no ordId"),
        }
    }
    out
}

async fn connect_and_run(
    adapter: &Arc<OkxAdapter>,
    health: &StreamHandle,
    tx: &mpsc::Sender<VEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (mut ws, _) = connect_async(adapter.private_ws_url()).await?;
    info!("OKX private WS connected");
    ws.send(Message::Text(login_message(adapter, Utc::now().timestamp()).to_string())).await?;

    let mut heartbeat = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECS));
    heartbeat.tick().await; // consume the immediate first tick

    loop {
        let text = tokio::select! {
            _ = heartbeat.tick() => {
                ws.send(Message::Text("ping".into())).await?;
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
        if text == "pong" {
            continue;
        }

        let v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => { warn!(error = %e, raw = %text, "OKX WS: parse failed"); continue; }
        };

        if v.get("data").is_some() {
            for event in venue_events(&v) {
                if tx.send(event).await.is_err() {
                    return Ok(());
                }
            }
            continue;
        }

        // Otherwise an event: {"event":"login"|"subscribe"|"error", "code":.., "msg":..}
        match v["event"].as_str() {
            Some("login") if v["code"].as_str() == Some("0") => {
                info!("OKX private WS logged in");
                ws.send(Message::Text(subscribe_message().to_string())).await?;
            }
            Some("subscribe") => {
                info!("OKX private WS subscribed to orders");
                health.set_live();
                // The reconciler pulls fills and the open-order snapshot for the outage.
                if tx.send(VEvent::Connected).await.is_err() {
                    return Ok(());
                }
            }
            Some("login") | Some("error") => {
                return Err(format!("OKX WS {}: {} (code {})", v["event"], v["msg"], v["code"]).into());
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::venue_event::VenueOrderStatus;

    const ORDER_ID: &str = "3f6b1c2e-8a4d-4e5f-9b21-1c2d3e4f5a6b";

    fn sample(name: &str) -> serde_json::Value {
        let text = match name {
            "ws_orders_fill" => include_str!("testdata/ws_orders_fill.json"),
            "ws_orders_lifecycle" => include_str!("testdata/ws_orders_lifecycle.json"),
            other => panic!("no sample {other}"),
        };
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn login_signs_get_users_self_verify_with_unix_seconds() {
        // printf '%s' '1700000000GET/users/self/verify' | openssl dgst -sha256 -hmac secret -binary | base64
        let a = OkxAdapter::new("key".into(), "secret".into(), "pass".into(), "PAPER");
        let m = login_message(&a, 1700000000);
        assert_eq!(m["op"], "login");
        assert_eq!(m["args"][0]["apiKey"], "key");
        assert_eq!(m["args"][0]["passphrase"], "pass");
        assert_eq!(m["args"][0]["timestamp"], "1700000000");
        assert_eq!(m["args"][0]["sign"], "lhmJXK08fk9SI1ZwFXKFRrPtzfbNOwC+D1xMJJ/1KZg=");
    }

    #[test]
    fn subscribes_to_spot_orders() {
        let m = subscribe_message();
        assert_eq!(m["args"][0]["channel"], "orders");
        assert_eq!(m["args"][0]["instType"], "SPOT");
    }

    #[test]
    fn a_push_with_a_trade_is_a_fill_then_the_order_state() {
        let events = venue_events(&sample("ws_orders_fill"));
        // Three entries (one placed outside the OMS), each a fill and a state.
        assert_eq!(events.len(), 6);
        let VEvent::Fill(f) = &events[0] else { panic!("a fill first") };
        assert_eq!((f.exec_id.as_str(), f.venue_order_id.as_str(), f.symbol.as_str(), f.buy, f.qty, f.price), ("BTC-USDT:123456789:B", "312269865356374016", "BTC-USDT", true, 0.0004, 65000.0));
        assert_eq!((f.client_order_id.as_deref(), f.account_ref.as_str(), f.fee.clone()), (Some(ORDER_ID), "", None));
        let VEvent::Order(o) = &events[1] else { panic!("then the order") };
        assert_eq!((o.status.clone(), o.cum_qty, o.qty, o.limit_px, o.buy), (VenueOrderStatus::Working, 0.0004, 0.001, None, true));
        let VEvent::Order(done) = &events[3] else { panic!() };
        assert_eq!((done.status.clone(), done.cum_qty), (VenueOrderStatus::Filled, 0.001));
    }

    #[test]
    fn okx_fill_without_oms_id_is_kept() {
        let events = venue_events(&sample("ws_orders_fill"));
        let VEvent::Fill(f) = &events[4] else { panic!("the app's fill") };
        assert_eq!((f.exec_id.as_str(), f.client_order_id.as_deref()), ("BTC-USDT:123456791:B", Some("manual1")));
    }

    #[test]
    fn the_fill_fee_flips_okx_sign_so_a_charge_is_positive() {
        let mut m = sample("ws_orders_fill");
        for (i, fee) in ["-0.026", "0.01", "0"].iter().enumerate() {
            m["data"][i]["fillFee"] = (*fee).into();
            m["data"][i]["fillFeeCcy"] = "USDT".into();
        }
        let fees: Vec<_> = venue_events(&m).into_iter().filter_map(|e| match e { VEvent::Fill(f) => Some(f.fee), _ => None }).collect();
        assert_eq!(fees, [Some((0.026, "USDT".to_string())), Some((-0.01, "USDT".to_string())), None]);
    }

    #[test]
    fn lifecycle_pushes_map_to_order_states_and_only_a_trade_makes_a_fill() {
        let events = venue_events(&sample("ws_orders_lifecycle"));
        let states: Vec<_> = events.iter().filter_map(|e| match e { VEvent::Order(o) => Some(o.status.clone()), _ => None }).collect();
        assert_eq!(states, [VenueOrderStatus::Working, VenueOrderStatus::Canceled, VenueOrderStatus::Canceled]);
        let fills: Vec<_> = events.iter().filter_map(|e| match e { VEvent::Fill(f) => Some(f.exec_id.as_str()), _ => None }).collect();
        assert_eq!(fills, ["BTC-USDT:123456700:B"], "live and a plain cancel carry no trade");
    }

    #[test]
    fn a_malformed_fill_is_skipped_but_its_order_state_is_kept() {
        let mut m = sample("ws_orders_fill");
        m["data"][0].as_object_mut().unwrap().remove("fillSz");
        assert_eq!(venue_events(&m).len(), 5);
    }

    #[test]
    fn other_channels_are_ignored() {
        let v = serde_json::from_str(include_str!("testdata/ws_tickers.json")).unwrap();
        assert!(venue_events(&v).is_empty());
    }

    #[test]
    fn okx_order_update_carries_created_and_update_time() {
        let mut msg = sample("ws_orders_fill");
        msg["data"][0]["cTime"] = serde_json::json!("1700000001000");
        msg["data"][0]["uTime"] = serde_json::json!("1700000005000");
        let VEvent::Order(o) = venue_events(&msg).into_iter().find(|e| matches!(e, VEvent::Order(_))).unwrap() else { unreachable!() };
        assert_eq!(o.created_at.map(|t| t.timestamp_millis()), Some(1700000001000));
        assert_eq!(o.ts.map(|t| t.timestamp_millis()), Some(1700000005000));
    }

    #[test]
    fn okx_stream_fill_carries_executed_at() {
        let mut msg = sample("ws_orders_fill");
        msg["data"][0]["fillTime"] = serde_json::json!("1700000003000");
        msg["data"][0]["uTime"] = serde_json::json!("1700000005000");
        let VEvent::Fill(f) = &venue_events(&msg)[0] else { panic!("a fill") };
        assert_eq!(f.executed_at.map(|t| t.timestamp_millis()), Some(1700000003000));
        // Without fillTime the update time stands in.
        msg["data"][0].as_object_mut().unwrap().remove("fillTime");
        let VEvent::Fill(f) = &venue_events(&msg)[0] else { panic!("a fill") };
        assert_eq!(f.executed_at.map(|t| t.timestamp_millis()), Some(1700000005000));
    }
}
