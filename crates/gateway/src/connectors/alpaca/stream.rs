use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

use crate::stream_health::StreamHandle;
use crate::stream_supervisor::{supervise, Session, StreamResult};
use crate::venue_event::{VenueEvent as VEvent, VenueFill, VenueOrderStatus, VenueOrderUpdate};

/// Ping cadence; a dead socket then surfaces, and the reconnect's `Connected`
/// makes the reconciler pull the gap.
const HEARTBEAT_SECS: u64 = 30;

struct AlpacaSession {
    ws_url: &'static str,
    api_key: String,
    api_secret: String,
    health: StreamHandle,
    tx: mpsc::Sender<VEvent>,
}

#[async_trait::async_trait]
impl Session for AlpacaSession {
    async fn run_once(&mut self) -> StreamResult {
        let result = connect_and_run(self.ws_url, &self.api_key, &self.api_secret, &self.health, &self.tx).await;
        let _ = self.tx.send(VEvent::Disconnected).await;
        result
    }
}

/// Venue events only: the reconciler books every fill (exactly once, through
/// ingest) and pulls what the stream missed.
pub async fn run(
    environment: &'static str,
    api_key: String,
    api_secret: String,
    health: StreamHandle,
    tx: mpsc::Sender<VEvent>,
) {
    info!(env = environment, "starting Alpaca trade-update stream");
    let ws_url = if environment == "LIVE" {
        "wss://api.alpaca.markets/stream"
    } else {
        "wss://paper-api.alpaca.markets/stream"
    };
    let name: &'static str = if environment == "LIVE" { "ALPACA/LIVE" } else { "ALPACA/PAPER" };
    let session = AlpacaSession { ws_url, api_key, api_secret, health: health.clone(), tx };
    supervise(name, health, session).await
}

async fn connect_and_run(
    ws_url: &str,
    api_key: &str,
    api_secret: &str,
    health: &StreamHandle,
    tx: &mpsc::Sender<VEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (mut ws, _) = connect_async(ws_url).await?;
    info!("Alpaca stream connected url={ws_url}");

    let auth = serde_json::json!({
        "action": "auth",
        "key": api_key,
        "secret": api_secret,
    });
    ws.send(Message::Text(auth.to_string())).await?;

    // Subscribe
    let listen = serde_json::json!({
        "action": "listen",
        "data": { "streams": ["trade_updates"] }
    });
    ws.send(Message::Text(listen.to_string())).await?;

    let mut heartbeat = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECS));
    heartbeat.tick().await; // consume the immediate first tick

    loop {
        let text = tokio::select! {
            _ = heartbeat.tick() => {
                ws.send(Message::Ping(Vec::new())).await?;
                continue;
            }
            msg = ws.next() => {
                let Some(msg) = msg else { break };
                health.record_event();
                match msg? {
                    Message::Text(t) => t,
                    Message::Binary(b) => {
                        match String::from_utf8(b) {
                            Ok(s) => s,
                            Err(_) => { warn!("Alpaca stream: received non-UTF8 binary frame, skipping"); continue; }
                        }
                    }
                    Message::Ping(p) => { ws.send(Message::Pong(p)).await?; continue; }
                    Message::Close(_) => break,
                    _ => continue,
                }
            }
        };

        let v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => { warn!(error = %e, raw = %text, "Alpaca stream: failed to parse message"); continue; }
        };

        // Alpaca wraps messages: {"stream":"...", "data":{...}}
        let stream = v["stream"].as_str().unwrap_or("");
        let data = &v["data"];

        match stream {
            "authorization" => {
                let status = data["status"].as_str().unwrap_or("");
                if status == "authorized" {
                    info!("Alpaca stream authenticated");
                } else {
                    error!(status, "Alpaca stream auth failed");
                    return Err(format!("auth failed: {status}").into());
                }
            }
            "listening" => {
                info!("Alpaca stream subscribed to trade_updates");
                health.set_live();
                if tx.send(VEvent::Connected).await.is_err() {
                    return Ok(());
                }
            }
            "trade_updates" => {
                for event in venue_events(data) {
                    if tx.send(event).await.is_err() {
                        return Ok(());
                    }
                }
            }
            other => {
                info!(stream = other, "Alpaca stream: unhandled stream type");
            }
        }
    }

    Ok(())
}

/// What one `trade_updates` frame means: a fill is a `Fill` (execution id,
/// qty and price of this execution) plus the order's state with its cumulative
/// quantity; lifecycle events are order states only. Whoever placed the order,
/// its client id is carried as-is. One key is one account, which ingest resolves
/// by the connection alone.
pub(crate) fn venue_events(data: &serde_json::Value) -> Vec<VEvent> {
    let event = data["event"].as_str().unwrap_or("");
    let order = &data["order"];
    let num = |v: &serde_json::Value| v.as_str().and_then(|s| s.parse::<f64>().ok());
    let time = |v: &serde_json::Value| v.as_str().and_then(|s| s.parse::<DateTime<Utc>>().ok());
    let status = match event {
        "new" | "accepted" | "partial_fill" => VenueOrderStatus::Working,
        "fill" => VenueOrderStatus::Filled,
        "canceled" => VenueOrderStatus::Canceled,
        "expired" => VenueOrderStatus::Expired,
        "rejected" => VenueOrderStatus::Rejected { reason: data["reason"].as_str().or(order["reject_reason"].as_str()).or(order["status"].as_str()).unwrap_or("rejected").to_string() },
        other => {
            info!(event = other, "Alpaca stream: unhandled event type, skipping");
            return Vec::new();
        }
    };
    let venue_order_id = order["id"].as_str().unwrap_or("").to_string();
    let client_order_id = order["client_order_id"].as_str().filter(|c| !c.is_empty()).map(str::to_string);
    let symbol = order["symbol"].as_str().unwrap_or("").to_string();
    let buy = order["side"].as_str() == Some("buy");
    let ts = time(&data["timestamp"]);
    let mut out = Vec::new();
    if matches!(event, "fill" | "partial_fill") {
        match (num(&order["filled_qty"]), num(&data["qty"]), num(&data["price"])) {
            (Some(cum), Some(qty), Some(price)) if !venue_order_id.is_empty() => out.push(VEvent::Fill(VenueFill {
                exec_id: super::fill_exec_id(&venue_order_id, cum),
                venue_order_id: venue_order_id.clone(),
                client_order_id: client_order_id.clone(),
                account_ref: String::new(),
                symbol: symbol.clone(),
                buy,
                qty,
                price,
                fee: None,
                executed_at: ts,
            })),
            _ => warn!(?data, "Alpaca stream: fill without order id, cumulative qty, qty or price"),
        }
    }
    let order_type = order["type"].as_str().unwrap_or("").to_string();
    out.push(VEvent::Order(VenueOrderUpdate {
        venue_order_id,
        client_order_id,
        account_ref: String::new(),
        symbol,
        buy,
        limit_px: if order_type == "market" { None } else { num(&order["limit_price"]) },
        order_type,
        qty: num(&order["qty"]).unwrap_or(0.0),
        status,
        cum_qty: num(&order["filled_qty"]).unwrap_or(0.0),
        ts,
        created_at: time(&order["created_at"]),
    }));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(event: &str, order_status: &str, filled: &str) -> serde_json::Value {
        serde_json::json!({
            "event": event, "qty": "2", "price": "101.5", "timestamp": "2026-11-18T14:30:00.123456Z",
            "order": { "id": "ord-1", "client_order_id": "c-1", "symbol": "AAPL", "side": "buy", "type": "limit",
                       "qty": "5", "limit_price": "101.5", "filled_qty": filled, "status": order_status },
        })
    }

    #[test]
    fn trade_update_fill_maps_to_venue_fill() {
        let events = venue_events(&update("partial_fill", "partially_filled", "2"));
        let VEvent::Fill(f) = &events[0] else { panic!("a fill first") };
        assert_eq!((f.exec_id.as_str(), f.venue_order_id.as_str(), f.symbol.as_str(), f.buy), ("ord-1:2", "ord-1", "AAPL", true));
        assert_eq!((f.qty, f.price, f.fee.clone(), f.account_ref.as_str()), (2.0, 101.5, None, ""));
        assert_eq!(f.client_order_id.as_deref(), Some("c-1"));
        assert!(f.executed_at.is_some());
        let VEvent::Order(o) = &events[1] else { panic!("then the order") };
        assert_eq!((o.status.clone(), o.cum_qty, o.qty, o.limit_px), (VenueOrderStatus::Working, 2.0, 5.0, Some(101.5)));
        let done = venue_events(&update("fill", "filled", "5"));
        let VEvent::Order(o) = &done[1] else { panic!() };
        assert_eq!((o.status.clone(), o.cum_qty), (VenueOrderStatus::Filled, 5.0));
    }

    #[test]
    fn dashboard_order_without_uuid_is_kept() {
        let mut data = update("fill", "filled", "5");
        data["order"]["client_order_id"] = serde_json::json!("e8c1f0ab-not-ours-web");
        let events = venue_events(&data);
        let VEvent::Fill(f) = &events[0] else { panic!() };
        assert_eq!(f.client_order_id.as_deref(), Some("e8c1f0ab-not-ours-web"));
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn lifecycle_events_map_to_order_status() {
        let st = |e: &str, s: &str| match &venue_events(&update(e, s, "0"))[..] {
            [VEvent::Order(o)] => o.status.clone(),
            other => panic!("one order update, got {other:?}"),
        };
        assert_eq!(st("new", "new"), VenueOrderStatus::Working);
        assert_eq!(st("accepted", "accepted"), VenueOrderStatus::Working);
        assert_eq!(st("canceled", "canceled"), VenueOrderStatus::Canceled);
        assert_eq!(st("expired", "expired"), VenueOrderStatus::Expired);
        assert!(matches!(st("rejected", "rejected"), VenueOrderStatus::Rejected { .. }));
        assert!(venue_events(&update("pending_new", "pending_new", "0")).is_empty());
        assert!(venue_events(&update("done_for_day", "done_for_day", "0")).is_empty());
    }

    #[test]
    fn stream_and_pull_produce_the_same_exec_id_for_one_fill() {
        let VEvent::Fill(s) = &venue_events(&update("partial_fill", "partially_filled", "10.000"))[0] else { panic!() };
        let activity = serde_json::json!([{ "id": "20261118143000000::x", "order_id": "ord-1", "cum_qty": "10", "qty": "2", "price": "101.5",
            "side": "buy", "symbol": "AAPL", "transaction_time": "2026-11-18T14:30:00Z" }]);
        let p = crate::connectors::alpaca::adapter::activities_page(&activity, "", 100, chrono::DateTime::<Utc>::MIN_UTC);
        assert_eq!(s.exec_id, p.fills[0].exec_id);
        assert_eq!(s.exec_id, "ord-1:10");
        assert_eq!(super::super::fill_exec_id("o", 10.0), super::super::fill_exec_id("o", "10.0".parse().unwrap()));
    }

    #[test]
    fn alpaca_order_update_carries_created_and_update_time() {
        let mut data = update("new", "new", "0");
        data["order"]["created_at"] = serde_json::json!("2026-11-18T14:29:59.500Z");
        let VEvent::Order(o) = &venue_events(&data)[0] else { panic!("an order") };
        assert_eq!(o.created_at, Some("2026-11-18T14:29:59.500Z".parse().unwrap()));
        assert_eq!(o.ts, Some("2026-11-18T14:30:00.123456Z".parse().unwrap()));
    }

    #[test]
    fn alpaca_stream_fill_carries_executed_at() {
        let VEvent::Fill(f) = &venue_events(&update("fill", "filled", "5"))[0] else { panic!("a fill") };
        assert_eq!(f.executed_at, Some("2026-11-18T14:30:00.123456Z".parse().unwrap()));
    }
}
