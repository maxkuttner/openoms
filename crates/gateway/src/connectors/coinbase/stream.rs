//! Coinbase Advanced Trade private user stream: real-time order state as venue
//! events.
//!
//! Connect `wss://advanced-trade-ws-user.coinbase.com` and, within 5 seconds,
//! subscribe to `user` (orders) and `heartbeats` (keeps an idle connection open),
//! each with a fresh JWT. Coinbase reports **cumulative** order state
//! (`cumulative_quantity`, `avg_price`), with no trade id and no per-fill size,
//! so the stream emits no fills: any id or size derived from it would differ from
//! the `trade_id` of `orders/historical/fills`, and one fill would be booked
//! twice. Every order entry is an order update instead; when its cumulative
//! quantity is ahead of what the OMS booked, ingest reports a fill gap and the
//! reconciler pulls the fills by `trade_id`.

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};

use crate::connectors::coinbase::adapter::{order_update_from_push, CoinbaseAdapter};
use crate::stream_health::StreamHandle;
use crate::stream_supervisor::{supervise, Session, StreamResult};
use crate::venue_event::VenueEvent as VEvent;

const WS_URL: &str = "wss://advanced-trade-ws-user.coinbase.com";

struct CoinbaseSession {
    adapter: Arc<CoinbaseAdapter>,
    health: StreamHandle,
    tx: mpsc::Sender<VEvent>,
}

#[async_trait::async_trait]
impl Session for CoinbaseSession {
    async fn run_once(&mut self) -> StreamResult {
        let result = connect_and_run(&self.adapter, &self.health, &self.tx).await;
        let _ = self.tx.send(VEvent::Disconnected).await;
        result
    }
}

pub async fn run(environment: &'static str, adapter: Arc<CoinbaseAdapter>, health: StreamHandle, tx: mpsc::Sender<VEvent>) {
    info!(env = environment, "starting Coinbase user stream");
    let session = CoinbaseSession { adapter, health: health.clone(), tx };
    supervise("COINBASE/LIVE", health, session).await
}

/// The two subscribe requests, each with its own fresh JWT (no `uri`).
pub(crate) fn subscribe_messages(adapter: &CoinbaseAdapter) -> [serde_json::Value; 2] {
    ["user", "heartbeats"].map(|channel| {
        serde_json::json!({ "type": "subscribe", "channel": channel, "jwt": adapter.jwt(None) })
    })
}

/// The order states one `user` message carries, whoever placed the orders
/// (a client id is carried as-is). One Coinbase key is one account, which ingest
/// resolves by the connection alone.
pub(crate) fn venue_events(msg: &serde_json::Value) -> Vec<VEvent> {
    if msg["channel"].as_str() != Some("user") {
        return Vec::new();
    }
    msg["events"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|e| e["orders"].as_array().into_iter().flatten())
        .filter_map(|o| {
            let update = order_update_from_push(o, "");
            if update.is_none() {
                warn!(?o, "Coinbase WS: order entry with unknown status or no order_id");
            }
            update.map(VEvent::Order)
        })
        .collect()
}

async fn connect_and_run(
    adapter: &Arc<CoinbaseAdapter>,
    health: &StreamHandle,
    tx: &mpsc::Sender<VEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (mut ws, _) = connect_async(WS_URL).await?;
    info!("Coinbase user WS connected");
    // Coinbase drops a connection that hasn't subscribed within 5 seconds.
    for msg in subscribe_messages(adapter) {
        ws.send(Message::Text(msg.to_string())).await?;
    }

    let mut live = false;
    // The `heartbeats` channel keeps the socket alive, so nothing else wakes the loop.
    while let Some(msg) = ws.next().await {
        health.record_event();
        let text = match msg? {
            Message::Text(t) => t,
            Message::Binary(b) => match String::from_utf8(b) {
                Ok(s) => s,
                Err(_) => continue,
            },
            Message::Ping(p) => { ws.send(Message::Pong(p)).await?; continue; }
            Message::Close(_) => break,
            _ => continue,
        };

        let v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => { warn!(error = %e, raw = %text, "Coinbase WS: parse failed"); continue; }
        };
        if v["type"].as_str() == Some("error") {
            return Err(format!("Coinbase user stream error: {}", v["message"]).into());
        }
        if v["channel"].as_str() != Some("user") {
            continue; // heartbeats, subscriptions acks
        }
        if !live {
            // The first user message is the snapshot: the subscription is up.
            info!("Coinbase user stream live");
            health.set_live();
            live = true;
            // The reconciler pulls fills and the open-order snapshot for the outage.
            if tx.send(VEvent::Connected).await.is_err() {
                return Ok(());
            }
        }
        for event in venue_events(&v) {
            if tx.send(event).await.is_err() {
                return Ok(());
            }
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
            "snapshot" => include_str!("testdata/ws_user_snapshot.json"),
            "partial" => include_str!("testdata/ws_user_partial.json"),
            "terminal" => include_str!("testdata/ws_user_terminal.json"),
            other => panic!("no sample {other}"),
        };
        serde_json::from_str(text).unwrap()
    }

    fn orders(msg: &serde_json::Value) -> Vec<crate::venue_event::VenueOrderUpdate> {
        venue_events(msg)
            .into_iter()
            .map(|e| match e {
                VEvent::Order(o) => o,
                other => panic!("only order states, got {other:?}"),
            })
            .collect()
    }

    fn adapter() -> CoinbaseAdapter {
        let pem = p256::SecretKey::random(&mut rand::thread_rng())
            .to_sec1_pem(p256::pkcs8::LineEnding::LF)
            .unwrap()
            .to_string();
        CoinbaseAdapter::new("organizations/o/apiKeys/k".into(), &pem).unwrap()
    }

    #[test]
    fn subscribe_messages_carry_a_fresh_jwt_and_no_uri() {
        let msgs = subscribe_messages(&adapter());
        let channels: Vec<_> = msgs.iter().map(|m| m["channel"].as_str().unwrap()).collect();
        assert_eq!(channels, ["user", "heartbeats"]);
        for m in &msgs {
            assert_eq!(m["type"], "subscribe");
            let jwt = m["jwt"].as_str().unwrap();
            let claims: serde_json::Value = serde_json::from_slice(
                &base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, jwt.split('.').nth(1).unwrap())
                    .unwrap(),
            )
            .unwrap();
            assert!(claims.get("uri").is_none());
        }
    }

    #[test]
    fn a_partial_fill_is_the_orders_cumulative_state_and_never_a_fill() {
        let o = orders(&sample("partial"));
        assert_eq!(o.len(), 1);
        assert_eq!((o[0].venue_order_id.as_str(), o[0].client_order_id.as_deref(), o[0].symbol.as_str(), o[0].buy), ("1", Some(ORDER_ID), "BTC-USD", true));
        assert_eq!((o[0].status.clone(), o[0].cum_qty, o[0].qty), (VenueOrderStatus::Working, 0.0005, 0.001));
    }

    #[test]
    fn coinbase_fill_without_oms_id_is_kept() {
        // The snapshot's second order was placed in the Coinbase app: its own client id is carried.
        let o = orders(&sample("snapshot"));
        assert_eq!(o.len(), 2);
        assert_eq!((o[1].client_order_id.as_deref(), o[1].cum_qty), (Some("placed-in-the-coinbase-app"), 0.5));
    }

    #[test]
    fn every_status_maps_to_an_order_state() {
        let o = orders(&sample("terminal"));
        let st: Vec<_> = o.iter().map(|o| (o.status.clone(), o.cum_qty)).collect();
        assert_eq!(
            st,
            [
                (VenueOrderStatus::Filled, 0.001),
                (VenueOrderStatus::Canceled, 0.0),
                (VenueOrderStatus::Expired, 0.0002),
                (VenueOrderStatus::Rejected { reason: "INSUFFICIENT_FUNDS".into() }, 0.0),
            ]
        );
    }

    #[test]
    fn other_channels_are_ignored() {
        let v = serde_json::from_str(include_str!("testdata/ws_ticker.json")).unwrap();
        assert!(venue_events(&v).is_empty());
    }

    #[test]
    fn coinbase_order_update_carries_created_time() {
        let mut msg = sample("partial");
        msg["events"][0]["orders"][0]["creation_time"] = serde_json::json!("2026-11-03T12:00:00.250Z");
        let o = &orders(&msg)[0];
        assert_eq!(o.created_at, Some("2026-11-03T12:00:00.250Z".parse().unwrap()));
    }
}
