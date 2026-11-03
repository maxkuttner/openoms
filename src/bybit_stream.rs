//! Bybit spot private stream (v5 WebSocket) — real-time order and execution
//! push, mirroring the Binance and Alpaca streams (no polling).
//!
//! Connect `wss://stream[-testnet].bybit.com/v5/private`, authenticate with
//! `{"op":"auth","args":[api_key, expires, sign("GET/realtime{expires}")]}`, then
//! subscribe to `order.spot` and `execution.spot`. Fills come **only** from
//! `execution.spot` `Trade` entries (one per fill, partials included), keyed by
//! Bybit's `execId`; `order.spot` supplies cancels and rejects. Taking fills from
//! both topics would double-count them: the execution path does not dedupe by
//! execution id. Everything is applied through the shared
//! [`crate::execution::process_execution_report`].

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::adapters::bybit::BybitAdapter;
use crate::domain::orders::commands::ExecutionReport;
use crate::execution::process_execution_report;
use crate::kafka::KafkaClient;
use crate::recon_orders::run_order_reconcile;
use crate::stream_health::StreamHandle;
use crate::stream_supervisor::{supervise, Session, StreamResult};

const VENUE: &str = "BYBIT";

/// Bybit drops a private connection that sends nothing for 30s and asks for a
/// ping every 20s; each beat also sweeps for missed terminal reports.
const HEARTBEAT_SECS: u64 = 20;

/// How long the auth signature stays valid.
const AUTH_EXPIRY_MS: i64 = 10_000;

struct BybitSession {
    pool: PgPool,
    kafka: Option<KafkaClient>,
    adapter: Arc<BybitAdapter>,
    health: StreamHandle,
    position_changed_tx: Option<mpsc::Sender<()>>,
}

#[async_trait::async_trait]
impl Session for BybitSession {
    async fn run_once(&mut self) -> StreamResult {
        connect_and_run(&self.adapter, &self.pool, &self.kafka, &self.health, self.position_changed_tx.as_ref()).await
    }
}

pub async fn run(
    environment: &'static str,
    pool: PgPool,
    kafka: Option<KafkaClient>,
    adapter: Arc<BybitAdapter>,
    health: StreamHandle,
    position_changed_tx: Option<mpsc::Sender<()>>,
) {
    info!(env = environment, "starting Bybit private stream");
    let session = BybitSession { pool, kafka, adapter, health: health.clone(), position_changed_tx };
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

/// The OMS execution reports one pushed message carries, by OMS order id.
/// Entries for orders the OMS didn't place (no UUID `orderLinkId`) are dropped.
pub(crate) fn reports(msg: &serde_json::Value) -> Vec<(Uuid, ExecutionReport)> {
    let data = msg["data"].as_array().cloned().unwrap_or_default();
    let execution = topic_is(msg, "execution");
    let order = topic_is(msg, "order");
    data.iter()
        .filter_map(|d| {
            let id = Uuid::parse_str(d["orderLinkId"].as_str()?).ok()?;
            let num = |k: &str| d[k].as_str().and_then(|s| s.parse::<f64>().ok());
            let report = if execution {
                if d["execType"].as_str() != Some("Trade") {
                    return None;
                }
                ExecutionReport::Fill {
                    execution_id: d["execId"].as_str()?.to_string(),
                    fill_qty: num("execQty")?,
                    fill_price: num("execPrice")?,
                    venue: VENUE.to_string(),
                }
            } else if order {
                let reason = d["rejectReason"].as_str().filter(|r| !r.is_empty() && *r != "EC_NoError");
                match d["orderStatus"].as_str()? {
                    "Cancelled" | "PartiallyFilledCanceled" | "Deactivated" => ExecutionReport::Canceled {
                        reason: reason.map(String::from),
                        venue: Some(VENUE.to_string()),
                    },
                    "Rejected" => ExecutionReport::Reject {
                        reason: reason.unwrap_or("Rejected").to_string(),
                        venue: Some(VENUE.to_string()),
                    },
                    // New / PartiallyFilled / Filled: fills arrive on execution.spot.
                    _ => return None,
                }
            } else {
                return None;
            };
            Some((id, report))
        })
        .collect()
}

async fn connect_and_run(
    adapter: &BybitAdapter,
    pool: &PgPool,
    kafka: &Option<KafkaClient>,
    health: &StreamHandle,
    position_changed_tx: Option<&mpsc::Sender<()>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
                run_order_reconcile(pool, kafka, adapter, VENUE, "bybit", position_changed_tx).await;
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
            Err(e) => { warn!(error = %e, raw = %text, "Bybit WS: parse failed"); continue; }
        };

        if v.get("topic").is_some() {
            for (order_id, report) in reports(&v) {
                if let Err(e) = process_execution_report(pool, kafka, order_id, report, "bybit", position_changed_tx).await {
                    error!(order_id = %order_id, error = %e, "Bybit WS: failed to apply execution report");
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
                    // Snapshot-on-(re)connect: heal anything that resolved during the outage.
                    run_order_reconcile(pool, kafka, adapter, VENUE, "bybit", position_changed_tx).await;
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

    const ORDER_ID: &str = "3f6b1c2e-8a4d-4e5f-9b21-1c2d3e4f5a6b";

    fn sample(name: &str) -> serde_json::Value {
        let text = match name {
            "ws_execution" => include_str!("adapters/testdata/bybit/ws_execution.json"),
            "ws_order_cancelled" => include_str!("adapters/testdata/bybit/ws_order_cancelled.json"),
            "ws_order_rejected" => include_str!("adapters/testdata/bybit/ws_order_rejected.json"),
            "ws_order_filled" => include_str!("adapters/testdata/bybit/ws_order_filled.json"),
            other => panic!("no sample {other}"),
        };
        serde_json::from_str(text).unwrap()
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
    fn fill_uses_exec_id_qty_and_price() {
        let r = reports(&sample("ws_execution"));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0.to_string(), ORDER_ID);
        match &r[0].1 {
            ExecutionReport::Fill { execution_id, fill_qty, fill_price, venue } => {
                assert_eq!(execution_id, "2100000000007764263");
                assert_eq!(*fill_qty, 0.001);
                assert_eq!(*fill_price, 65000.0);
                assert_eq!(venue, "BYBIT");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn status_partially_filled_canceled_and_deactivated_are_cancels_rejected_is_a_reject() {
        let cancels = reports(&sample("ws_order_cancelled"));
        assert_eq!(cancels.len(), 3, "Cancelled, PartiallyFilledCanceled and Deactivated; New is nothing");
        for (_, report) in &cancels {
            assert!(
                matches!(report, ExecutionReport::Canceled { reason: None, venue: Some(v) } if v == "BYBIT"),
                "{report:?}"
            );
        }
        let rejects = reports(&sample("ws_order_rejected"));
        assert_eq!(rejects.len(), 1);
        assert!(
            matches!(&rejects[0].1, ExecutionReport::Reject { reason, .. } if reason == "EC_InsufficientBalance"),
            "{:?}",
            rejects[0].1
        );
    }

    #[test]
    fn a_non_uuid_order_link_id_is_ignored() {
        // ws_order_rejected.json's second entry was placed in the Bybit app, not by the OMS.
        let r = reports(&sample("ws_order_rejected"));
        assert!(r.iter().all(|(id, _)| id.to_string() == ORDER_ID));
    }

    #[test]
    fn order_updates_never_produce_fills() {
        // Fills come only from execution.spot; order.spot PartiallyFilled / Filled
        // would double-count them (the execution path does not dedupe by exec id).
        assert!(reports(&sample("ws_order_filled")).is_empty());
    }
}
