//! Binance Spot user-data stream over the WebSocket API — real-time
//! `executionReport` push, mirroring the Alpaca stream (no polling).
//!
//! Binance retired the REST `listenKey` stream; the current mechanism is:
//! connect the WS API, `session.logon` with an Ed25519 signature, then
//! `userDataStream.subscribe`. Events arrive wrapped as
//! `{"subscriptionId":N,"event":{"e":"executionReport",...}}`. A `TRADE`
//! execution fires per fill (including partials), so fills reflect immediately
//! and incrementally through the shared [`crate::execution::process_execution_report`].

use std::sync::Arc;

use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use sqlx::{PgPool, Row};
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::adapters::binance::BinanceAdapter;
use crate::domain::orders::commands::ExecutionReport;
use crate::execution::process_execution_report;
use crate::kafka::KafkaClient;
use crate::stream_health::StreamHandle;
use crate::stream_supervisor::{supervise, Session, StreamResult};

const VENUE: &str = "BINANCE";

/// How often to ping the socket and sweep for missed terminal reports. A stalled
/// user-data subscription (fills stop arriving without a Close frame) is otherwise
/// invisible; the ping surfaces a dead socket and the sweep recovers the fill.
const HEARTBEAT_SECS: u64 = 30;

struct BinanceSession {
    pool: PgPool,
    kafka: Option<KafkaClient>,
    adapter: Arc<BinanceAdapter>,
    health: StreamHandle,
    position_changed_tx: Option<mpsc::Sender<()>>,
}

#[async_trait::async_trait]
impl Session for BinanceSession {
    async fn run_once(&mut self) -> StreamResult {
        connect_and_run(
            &self.adapter,
            &self.pool,
            &self.kafka,
            &self.health,
            self.position_changed_tx.as_ref(),
        )
        .await
    }
}

/// Credentials are not taken here: the adapter already holds them (it signs with
/// an Ed25519 key loaded at construction), which is why the old `_api_key` /
/// `_api_secret` parameters were unused.
pub async fn run(
    environment: &'static str,
    pool: PgPool,
    kafka: Option<KafkaClient>,
    adapter: Arc<BinanceAdapter>,
    health: StreamHandle,
    position_changed_tx: Option<mpsc::Sender<()>>,
) {
    info!(env = environment, "starting Binance user-data stream");
    // Catch up on anything missed while disconnected, then stream live.
    reconcile_routed_orders(&pool, &kafka, &adapter, position_changed_tx.as_ref()).await;

    let session = BinanceSession {
        pool,
        kafka,
        adapter,
        health: health.clone(),
        position_changed_tx,
    };
    supervise("BINANCE/PAPER", health, session).await
}

async fn connect_and_run(
    adapter: &BinanceAdapter,
    pool: &PgPool,
    kafka: &Option<KafkaClient>,
    health: &StreamHandle,
    position_changed_tx: Option<&mpsc::Sender<()>>,
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
            // Periodic liveness probe + catch-up sweep for missed terminal reports.
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
                if let Err(e) = handle_execution_report(&v["event"], pool, kafka, position_changed_tx).await {
                    error!(error = %e, "Binance WS: error processing executionReport");
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
                } else {
                    return Err(format!("userDataStream.subscribe failed: {}", v).into());
                }
            }
            _ => {}
        }
    }
    Ok(())
}

async fn handle_execution_report(
    event: &serde_json::Value,
    pool: &PgPool,
    kafka: &Option<KafkaClient>,
    position_changed_tx: Option<&mpsc::Sender<()>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let exec_type = event["x"].as_str().unwrap_or("");
    // On a cancel, Binance puts the cancel-request's id in `c` and the original
    // order's client id in `C`; for fills the order id is in `c`. Prefer `C`.
    let client_order_id = match event["C"].as_str() {
        Some(c) if !c.is_empty() => c,
        _ => event["c"].as_str().unwrap_or(""),
    };

    let report = match exec_type {
        "TRADE" => {
            // Per-fill event (incl. partials): l/L are the last executed qty/price.
            let fill_qty: f64 = event["l"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let fill_price: f64 = event["L"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let execution_id = event["t"]
                .as_i64()
                .map(|t| t.to_string())
                .or_else(|| event["i"].as_i64().map(|i| i.to_string()))
                .unwrap_or_else(|| client_order_id.to_string());
            ExecutionReport::Fill { execution_id, fill_qty, fill_price, venue: VENUE.to_string() }
        }
        "CANCELED" => ExecutionReport::Canceled { reason: None, venue: Some(VENUE.to_string()) },
        "REJECTED" | "EXPIRED" => ExecutionReport::Reject {
            reason: event["X"].as_str().unwrap_or(exec_type).to_string(),
            venue: Some(VENUE.to_string()),
        },
        // NEW / REPLACED / … — no OMS state change.
        _ => return Ok(()),
    };

    let order_id = match Uuid::parse_str(client_order_id) {
        Ok(id) => id,
        Err(_) => {
            warn!(client_order_id, "Binance WS: client id is not a UUID, skipping");
            return Ok(());
        }
    };

    if let Err(e) = process_execution_report(pool, kafka, order_id, report, "binance", position_changed_tx).await {
        error!(order_id = %order_id, error = %e, "Binance WS: failed to apply execution report");
    }
    Ok(())
}

/// One-time startup catch-up: for each `routed` Binance order, fetch its state via
/// REST and apply the terminal report if it already resolved while we were down.
async fn reconcile_routed_orders(
    pool: &PgPool,
    kafka: &Option<KafkaClient>,
    adapter: &BinanceAdapter,
    position_changed_tx: Option<&mpsc::Sender<()>>,
) {
    let rows = match sqlx::query(
        "SELECT os.order_id, os.instrument_id \
         FROM order_state os \
         JOIN broker_connection bc ON bc.code = os.broker_connection_code \
         WHERE os.status = 'routed' AND bc.broker_code = 'BINANCE'",
    )
    .fetch_all(pool)
    .await
    {
        Ok(r) => r,
        Err(e) => { error!(error = %e, "Binance reconcile: query failed"); return; }
    };
    if rows.is_empty() {
        info!("Binance reconcile: no routed orders");
        return;
    }

    for row in rows {
        let order_id: Uuid = row.get("order_id");
        let instrument_id_num: i64 = row.get::<String, _>("instrument_id").parse().unwrap_or_default();

        let symbol: Option<String> = sqlx::query_scalar(
            "SELECT external_symbol FROM oms.instrument_xref \
             WHERE instrument_id = $1 AND source_type = 'BROKER' AND source_code = 'BINANCE' LIMIT 1",
        )
        .bind(instrument_id_num)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();
        let Some(symbol) = symbol else { continue };

        let ext_id: Option<String> = sqlx::query_scalar(
            "SELECT payload_json->'payload'->>'external_order_id' \
             FROM order_event WHERE order_id = $1 AND event_type = 'order_routed' LIMIT 1",
        )
        .bind(order_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();
        let Some(ext_id) = ext_id.filter(|s| !s.is_empty()) else { continue };

        let order = match adapter.get_order(&symbol, &ext_id).await {
            Ok(o) => o,
            Err(e) => { warn!(order_id = %order_id, error = %e, "Binance reconcile: get_order failed"); continue; }
        };

        let status = order["status"].as_str().unwrap_or("");
        let report = match status {
            "FILLED" => {
                let executed: f64 = order["executedQty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let quote: f64 = order["cummulativeQuoteQty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let avg_price = if executed > 0.0 { quote / executed } else { 0.0 };
                ExecutionReport::Fill { execution_id: ext_id.clone(), fill_qty: executed, fill_price: avg_price, venue: VENUE.to_string() }
            }
            "CANCELED" => ExecutionReport::Canceled { reason: None, venue: Some(VENUE.to_string()) },
            "REJECTED" | "EXPIRED" => ExecutionReport::Reject { reason: status.to_string(), venue: Some(VENUE.to_string()) },
            _ => continue,
        };

        match process_execution_report(pool, kafka, order_id, report, "binance", position_changed_tx).await {
            Ok(()) => info!(order_id = %order_id, binance_status = status, "Binance reconcile: applied missed report"),
            Err(e) => error!(order_id = %order_id, error = %e, "Binance reconcile: apply failed"),
        }
    }
}
