use futures_util::{SinkExt, StreamExt};
use sqlx::{PgPool, Row};
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};
use uuid::Uuid;

use std::sync::Arc;
use tokio::sync::mpsc;

use crate::adapters::alpaca::AlpacaAdapter;
use crate::domain::orders::commands::ExecutionReport;
use crate::execution::process_execution_report;
use crate::kafka::KafkaClient;
use crate::stream_health::StreamHandle;

/// Ping + missed-fill sweep cadence; see the Binance stream for the rationale.
const HEARTBEAT_SECS: u64 = 30;

pub async fn run(
    environment: &'static str,
    api_key: String,
    api_secret: String,
    pool: PgPool,
    kafka: Option<KafkaClient>,
    adapter: Arc<AlpacaAdapter>,
    health: StreamHandle,
    position_changed_tx: Option<mpsc::Sender<()>>,
) {
    info!(env = environment, "starting Alpaca trade-update stream");
    reconcile_routed_orders(&pool, &kafka, &adapter, position_changed_tx.as_ref()).await;

    let ws_url = if environment == "LIVE" {
        "wss://api.alpaca.markets/stream"
    } else {
        "wss://paper-api.alpaca.markets/stream"
    };

    let mut backoff_secs: u64 = 1;

    loop {
        health.set_connecting();
        info!(env = environment, attempt = backoff_secs, "Alpaca stream connecting");
        match connect_and_run(ws_url, &api_key, &api_secret, &pool, &kafka, &adapter, &health, position_changed_tx.as_ref()).await {
            Ok(()) => {
                warn!(env = environment, "Alpaca stream closed cleanly, reconnecting in {backoff_secs}s");
                health.set_down("stream closed");
            }
            Err(e) => {
                warn!(env = environment, error = %e, "Alpaca stream error, reconnecting in {backoff_secs}s");
                health.set_down(e.to_string());
            }
        }
        sleep(Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(30);
    }
}

async fn reconcile_routed_orders(
    pool: &PgPool,
    kafka: &Option<KafkaClient>,
    adapter: &AlpacaAdapter,
    position_changed_tx: Option<&mpsc::Sender<()>>,
) {
    info!("starting reconciliation of routed orders");

    let rows = match sqlx::query("SELECT order_id FROM order_state WHERE status = 'routed'")
        .fetch_all(pool)
        .await
    {
        Ok(r) => r,
        Err(e) => { error!(error = %e, "reconciliation: failed to query routed orders"); return; }
    };

    if rows.is_empty() {
        info!("reconciliation complete: no routed orders");
        return;
    }

    info!(count = rows.len(), "reconciliation: checking routed orders");

    for row in rows {
        let order_id: Uuid = row.get("order_id");

        // fetch the Alpaca order ID from the OrderRouted event payload
        let ext_row = sqlx::query(
            "SELECT payload_json->'payload'->>'external_order_id' AS ext_id \
             FROM order_event WHERE order_id = $1 AND event_type = 'order_routed' LIMIT 1",
        )
        .bind(order_id)
        .fetch_optional(pool)
        .await;

        let external_order_id: String = match ext_row {
            Ok(Some(r)) => match r.get::<Option<String>, _>("ext_id") {
                Some(id) if !id.is_empty() => id,
                _ => { warn!(order_id = %order_id, "reconciliation: no external_order_id found, skipping"); continue; }
            },
            Ok(None) => { warn!(order_id = %order_id, "reconciliation: OrderRouted event not found, skipping"); continue; }
            Err(e) => { error!(order_id = %order_id, error = %e, "reconciliation: DB error fetching external_order_id"); continue; }
        };

        let alpaca_order = match adapter.get_order(&external_order_id).await {
            Ok(o) => o,
            Err(e) => { warn!(order_id = %order_id, error = %e, "reconciliation: failed to fetch order from Alpaca, skipping"); continue; }
        };

        let status = alpaca_order["status"].as_str().unwrap_or("");
        let report = match status {
            "filled" | "partially_filled" => {
                let fill_qty: f64 = alpaca_order["filled_qty"]
                    .as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let fill_price: f64 = alpaca_order["filled_avg_price"]
                    .as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let execution_id = alpaca_order["id"].as_str().unwrap_or("").to_string();
                ExecutionReport::Fill { execution_id, fill_qty, fill_price, venue: "ALPACA".to_string() }
            }
            "canceled" => ExecutionReport::Canceled {
                reason: None,
                venue: Some("ALPACA".to_string()),
            },
            "rejected" | "expired" => {
                ExecutionReport::Reject {
                    reason: status.to_string(),
                    venue: Some("ALPACA".to_string()),
                }
            }
            other => {
                info!(order_id = %order_id, alpaca_status = other, "reconciliation: order still pending at Alpaca, skipping");
                continue;
            }
        };

        match process_execution_report(pool, kafka, order_id, report, "alpaca", position_changed_tx).await {
            Ok(()) => info!(order_id = %order_id, alpaca_status = status, "reconciliation: applied missed fill"),
            Err(e) => error!(order_id = %order_id, error = %e, "reconciliation: failed to apply execution report"),
        }
    }

    info!("reconciliation complete");
}

async fn connect_and_run(
    ws_url: &str,
    api_key: &str,
    api_secret: &str,
    pool: &PgPool,
    kafka: &Option<KafkaClient>,
    adapter: &AlpacaAdapter,
    health: &StreamHandle,
    position_changed_tx: Option<&mpsc::Sender<()>>,
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
                reconcile_routed_orders(pool, kafka, adapter, position_changed_tx).await;
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
                if let Err(e) = handle_trade_update(data, pool, kafka, position_changed_tx).await {
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
    pool: &PgPool,
    kafka: &Option<KafkaClient>,
    position_changed_tx: Option<&mpsc::Sender<()>>,
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

    match process_execution_report(pool, kafka, order_id, report, "alpaca", position_changed_tx).await {
        Ok(()) => {}
        Err(e) => {
            error!(order_id = %order_id, error = %e, "Alpaca stream: failed to process execution report");
        }
    }

    Ok(())
}

