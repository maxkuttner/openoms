use futures_util::{SinkExt, StreamExt};
use tokio::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};
use uuid::Uuid;

use std::sync::Arc;

use crate::adapters::alpaca::AlpacaAdapter;
use crate::domain::orders::commands::ExecutionReport;
use crate::order_engine::{EngineHandle, ExecEvent};
use crate::stream_health::StreamHandle;
use crate::stream_supervisor::{supervise, Session, StreamResult};

/// Ping + missed-fill sweep cadence; see the Binance stream for the rationale.
const HEARTBEAT_SECS: u64 = 30;

struct AlpacaSession {
    ws_url: &'static str,
    api_key: String,
    api_secret: String,
    adapter: Arc<AlpacaAdapter>,
    health: StreamHandle,
    engine: EngineHandle,
}

#[async_trait::async_trait]
impl Session for AlpacaSession {
    async fn run_once(&mut self) -> StreamResult {
        connect_and_run(
            self.ws_url,
            &self.api_key,
            &self.api_secret,
            &self.adapter,
            &self.health,
            &self.engine,
        )
        .await
    }
}

pub async fn run(
    environment: &'static str,
    api_key: String,
    api_secret: String,
    adapter: Arc<AlpacaAdapter>,
    health: StreamHandle,
    engine: EngineHandle,
) {
    info!(env = environment, "starting Alpaca trade-update stream");
    engine.reconcile("ALPACA", "alpaca", adapter.clone()).await;

    let ws_url = if environment == "LIVE" {
        "wss://api.alpaca.markets/stream"
    } else {
        "wss://paper-api.alpaca.markets/stream"
    };

    let name: &'static str = if environment == "LIVE" { "ALPACA/LIVE" } else { "ALPACA/PAPER" };
    let session = AlpacaSession {
        ws_url,
        api_key,
        api_secret,
        adapter,
        health: health.clone(),
        engine,
    };
    supervise(name, health, session).await
}

async fn connect_and_run(
    ws_url: &str,
    api_key: &str,
    api_secret: &str,
    adapter: &Arc<AlpacaAdapter>,
    health: &StreamHandle,
    engine: &EngineHandle,
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
                engine.reconcile("ALPACA", "alpaca", adapter.clone()).await;
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
            }
            "trade_updates" => {
                if let Err(e) = handle_trade_update(data, engine).await {
                    error!(error = %e, "Alpaca stream: error processing trade update");
                }
            }
            other => {
                info!(stream = other, "Alpaca stream: unhandled stream type");
            }
        }
    }

    Ok(())
}

async fn handle_trade_update(
    data: &serde_json::Value,
    engine: &EngineHandle,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let event = data["event"].as_str().unwrap_or("unknown");
    let order = &data["order"];
    let client_order_id = order["client_order_id"].as_str().unwrap_or("");

    info!(event, client_order_id, "alpaca trade update received");

    let report = match event {
        "fill" | "partial_fill" => {
            let fill_qty: f64 = data["qty"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0);
            let fill_price: f64 = data["price"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0);
            let execution_id = data["execution_id"]
                .as_str()
                .unwrap_or(client_order_id)
                .to_string();

            ExecutionReport::Fill {
                execution_id,
                fill_qty,
                fill_price,
                venue: "ALPACA".to_string(),
            }
        }
        "rejected" => {
            let reason = order["status"].as_str().unwrap_or("rejected").to_string();
            ExecutionReport::Reject {
                reason,
                venue: Some("ALPACA".to_string()),
            }
        }
        "canceled" => ExecutionReport::Canceled {
            reason: None,
            venue: Some("ALPACA".to_string()),
        },
        "expired" | "pending_new" | "new" | "accepted" | "done_for_day" => {
            return Ok(());
        }
        other => {
            info!(event = other, "Alpaca stream: unhandled event type, skipping");
            return Ok(());
        }
    };

    let order_id = match Uuid::parse_str(client_order_id) {
        Ok(id) => id,
        Err(_) => {
            warn!(client_order_id, "Alpaca stream: client_order_id is not a valid UUID, skipping");
            return Ok(());
        }
    };

    engine.exec(ExecEvent::from_report(order_id, "ALPACA", "alpaca", report)).await;
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::order_engine::{EngineHandle, ExecKind, ExecSource, GatewayMsg};

    #[tokio::test]
    async fn alpaca_fill_frame_becomes_fill_event() {
        let order_id = Uuid::new_v4();
        let data = serde_json::json!({
            "event": "partial_fill",
            "qty": "2",
            "price": "101.5",
            "execution_id": "ex-1",
            "order": { "client_order_id": order_id.to_string(), "status": "partially_filled" },
        });
        let (engine, mut rx) = EngineHandle::new();

        handle_trade_update(&data, &engine).await.unwrap();

        let Some(GatewayMsg::Exec(ev)) = rx.try_recv().ok() else { panic!("an exec event") };
        assert_eq!(ev.order_id, order_id);
        assert_eq!(ev.actor, "alpaca");
        assert_eq!(ev.source, ExecSource::Session);
        assert_eq!(ev.kind, ExecKind::Fill { execution_id: "ex-1".into(), qty: 2.0, price: 101.5 });
    }
}
