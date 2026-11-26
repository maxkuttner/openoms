//! Bybit spot private stream (v5 WebSocket): real-time order and execution
//! push as venue events.
//!
//! Connect `wss://stream[-testnet].bybit.com/v5/private`, authenticate with
//! `{"op":"auth","args":[api_key, expires, sign("GET/realtime{expires}")]}`, then
//! subscribe to `order.spot` and `execution.spot`. Fills come **only** from
//! `execution.spot` `Trade` entries (one per fill, partials included), keyed by
//! Bybit's `execId`, the same id `/v5/execution/list` carries; `order.spot`
//! supplies the order's state. Whoever placed an order, its fills and state are
//! emitted; the reconciler pulls what the stream missed after a reconnect.
//!
//! The two topics are not ordered against each other, so a terminal `order.spot`
//! update can overtake the fills it closes; ingest resolves that by the order's
//! cumulative quantity.

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};

use crate::connectors::bybit::adapter::{fill_from_row, order_update_from_row, venue_status, BybitAdapter};
use crate::stream_health::StreamHandle;
use crate::stream_supervisor::{supervise, Session, StreamResult};
use crate::venue_event::VenueEvent as VEvent;

/// Bybit drops a private connection that sends nothing for 30s and asks for a
/// ping every 20s.
const HEARTBEAT_SECS: u64 = 20;

/// How long the auth signature stays valid.
const AUTH_EXPIRY_MS: i64 = 10_000;

struct BybitSession {
    adapter: Arc<BybitAdapter>,
    health: StreamHandle,
    tx: mpsc::Sender<VEvent>,
}

#[async_trait::async_trait]
impl Session for BybitSession {
    async fn run_once(&mut self) -> StreamResult {
        let result = connect_and_run(&self.adapter, &self.health, &self.tx).await;
        let _ = self.tx.send(VEvent::Disconnected).await;
        result
    }
}

pub async fn run(environment: &'static str, adapter: Arc<BybitAdapter>, health: StreamHandle, tx: mpsc::Sender<VEvent>) {
    info!(env = environment, "starting Bybit private stream");
    let session = BybitSession { adapter, health: health.clone(), tx };
    let name: &'static str = if environment == "LIVE" { "BYBIT/LIVE" } else { "BYBIT/PAPER" };
    supervise(name, health, session).await
}

/// The `auth` request for a connection, valid until `expires_ms`.
pub(crate) fn auth_message(adapter: &BybitAdapter, expires_ms: i64) -> serde_json::Value {
    serde_json::json!({
        "op": "auth",
        "args": [adapter.api_key(), expires_ms, adapter.sign(&format!("GET/realtime{expires_ms}"))],
    })
}

fn topic_is(msg: &serde_json::Value, name: &str) -> bool {
    msg["topic"].as_str().is_some_and(|t| t == name || t.strip_prefix(name).is_some_and(|rest| rest.starts_with('.')))
}

/// What one pushed message means: every `Trade` entry of `execution` is a fill
/// (its fee rides along), every `order` entry is the order's state. One API key
/// is one account, which ingest resolves by the connection alone.
pub(crate) fn venue_events(msg: &serde_json::Value) -> Vec<VEvent> {
    let data = msg["data"].as_array().map(Vec::as_slice).unwrap_or_default();
    let mut out = Vec::new();
    if topic_is(msg, "execution") {
        for d in data {
            match fill_from_row(d, "") {
                Ok(Some(f)) => out.push(VEvent::Fill(f)),
                Ok(None) => {}
                Err(missing) => warn!(exec_id = d["execId"].as_str().unwrap_or(""), missing, "Bybit WS: execution skipped"),
            }
        }
    } else if topic_is(msg, "order") {
        for d in data {
            match (venue_status(d), order_update_from_row(d, "")) {
                (_, Some(o)) => out.push(VEvent::Order(o)),
                (None, None) => warn!(?d, "Bybit WS: order entry with unknown status or no orderId"),
                (Some(_), None) => warn!(?d, "Bybit WS: order entry without orderId"),
            }
        }
    }
    out
}

async fn connect_and_run(
    adapter: &Arc<BybitAdapter>,
    health: &StreamHandle,
    tx: &mpsc::Sender<VEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    health.set_heartbeat_secs(HEARTBEAT_SECS);
    let (mut ws, _) = connect_async(adapter.private_ws_url()).await?;
    info!("Bybit private WS connected");
    let expires = chrono::Utc::now().timestamp_millis() + AUTH_EXPIRY_MS;
    ws.send(Message::Text(auth_message(adapter, expires).to_string())).await?;

    let mut heartbeat = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECS));
    heartbeat.tick().await; // consume the immediate first tick

    loop {
        let text = tokio::select! {
            _ = heartbeat.tick() => {
                ws.send(Message::Text(serde_json::json!({"op": "ping"}).to_string())).await?;
                continue;
            }
            msg = ws.next() => {
                let Some(msg) = msg else { break };
                health.record_check();
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
            Err(e) => { warn!(error = %e, raw = %text, "Bybit WS: parse failed"); continue; }
        };

        if v.get("topic").is_some() {
            let events = venue_events(&v);
            if !events.is_empty() {
                health.record_event();
            }
            for event in events {
                if tx.send(event).await.is_err() {
                    return Ok(());
                }
            }
            continue;
        }

        // Otherwise an op response: {"op":"auth"|"subscribe"|"pong", "success":bool, "ret_msg":..}
        match v["op"].as_str() {
            Some("auth") => {
                if v["success"].as_bool() == Some(true) {
                    info!("Bybit private WS authenticated");
                    let sub = serde_json::json!({"op": "subscribe", "args": ["order.spot", "execution.spot"]});
                    ws.send(Message::Text(sub.to_string())).await?;
                } else {
                    return Err(format!("Bybit auth failed: {}", v["ret_msg"]).into());
                }
            }
            Some("subscribe") => {
                if v["success"].as_bool() == Some(true) {
                    info!("Bybit private WS subscribed to order.spot and execution.spot");
                    health.set_live();
                    // The reconciler pulls fills and the open-order snapshot for the outage.
                    if tx.send(VEvent::Connected).await.is_err() {
                        return Ok(());
                    }
                } else {
                    return Err(format!("Bybit subscribe failed: {}", v["ret_msg"]).into());
                }
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
            "ws_execution" => include_str!("testdata/ws_execution.json"),
            "ws_order_cancelled" => include_str!("testdata/ws_order_cancelled.json"),
            "ws_order_rejected" => include_str!("testdata/ws_order_rejected.json"),
            "ws_order_filled" => include_str!("testdata/ws_order_filled.json"),
            other => panic!("no sample {other}"),
        };
        serde_json::from_str(text).unwrap()
    }

    fn orders(msg: &serde_json::Value) -> Vec<crate::venue_event::VenueOrderUpdate> {
        venue_events(msg).into_iter().map(|e| match e {
            VEvent::Order(o) => o,
            other => panic!("only orders expected, got {other:?}"),
        }).collect()
    }

    #[test]
    fn execution_trade_is_a_fill_with_its_fee() {
        let events = venue_events(&sample("ws_execution"));
        assert_eq!(events.len(), 1);
        let VEvent::Fill(f) = &events[0] else { panic!("a fill") };
        assert_eq!((f.exec_id.as_str(), f.venue_order_id.as_str(), f.symbol.as_str(), f.buy), ("2100000000007764263", "1820000000000000002", "BTCUSDT", true));
        assert_eq!((f.qty, f.price, f.account_ref.as_str()), (0.001, 65000.0, ""));
        assert_eq!(f.client_order_id.as_deref(), Some(ORDER_ID));
        // No feeCurrency on the push: a buy pays in the base coin.
        assert_eq!(f.fee, Some((0.000001, "BTC".to_string())));
        assert!(f.executed_at.is_some());
    }

    #[test]
    fn bybit_fill_without_oms_id_is_kept() {
        let mut msg = sample("ws_execution");
        msg["data"][0]["orderLinkId"] = serde_json::json!("");
        let VEvent::Fill(f) = &venue_events(&msg)[0] else { panic!("a fill") };
        assert_eq!(f.client_order_id, None);
        assert_eq!(f.exec_id, "2100000000007764263");
    }

    #[test]
    fn a_malformed_execution_is_skipped_not_fatal() {
        let mut msg = sample("ws_execution");
        msg["data"][0].as_object_mut().unwrap().remove("execQty");
        assert!(venue_events(&msg).is_empty());
    }

    #[test]
    fn order_entries_map_to_order_states_for_every_order() {
        let o = orders(&sample("ws_order_cancelled"));
        assert_eq!(o.len(), 4, "the app's orders too; every status is known");
        assert_eq!(o[0].status, VenueOrderStatus::Canceled);
        assert_eq!((o[1].status.clone(), o[1].cum_qty, o[1].venue_order_id.as_str()), (VenueOrderStatus::Canceled, 0.0005, "2"));
        assert_eq!(o[2].status, VenueOrderStatus::Canceled);
        assert_eq!(o[3].status, VenueOrderStatus::Working);
        let r = orders(&sample("ws_order_rejected"));
        assert!(matches!(&r[0].status, VenueOrderStatus::Rejected { reason } if reason == "EC_InsufficientBalance"));
    }

    #[test]
    fn order_updates_never_produce_fills() {
        // Fills come only from execution.spot; a Filled order is a state, not a fill.
        let o = orders(&sample("ws_order_filled"));
        assert!(!o.is_empty());
        assert!(o.iter().any(|o| o.status == VenueOrderStatus::Filled));
    }

    #[test]
    fn auth_signs_get_realtime_with_the_expiry() {
        let a = BybitAdapter::new("key".into(), "secret".into(), "PAPER");
        let m = auth_message(&a, 1700000005000);
        assert_eq!(m["op"], "auth");
        assert_eq!(m["args"][0], "key");
        assert_eq!(m["args"][1], 1700000005000_i64);
        assert_eq!(m["args"][2], a.sign("GET/realtime1700000005000"));
    }

    #[test]
    fn bybit_order_update_carries_created_and_update_time() {
        let mut msg = sample("ws_order_filled");
        msg["data"][0]["createdTime"] = serde_json::json!("1700000001000");
        msg["data"][0]["updatedTime"] = serde_json::json!("1700000005000");
        let o = &orders(&msg)[0];
        assert_eq!(o.created_at.map(|t| t.timestamp_millis()), Some(1700000001000));
        assert_eq!(o.ts.map(|t| t.timestamp_millis()), Some(1700000005000));
    }

    #[test]
    fn bybit_stream_fill_carries_executed_at() {
        let VEvent::Fill(f) = &venue_events(&sample("ws_execution"))[0] else { panic!("a fill") };
        assert_eq!(f.executed_at.map(|t| t.timestamp_millis()), Some(1700000000000));
    }
}
