//! OKX spot private stream (v5 WebSocket) — real-time order push, mirroring the
//! Bybit and Binance streams (no polling).
//!
//! Connect `wss://ws.okx.com/ws/v5/private` (Demo Trading: `wspap.okx.com`), log
//! in with `{"op":"login","args":[{apiKey, passphrase, timestamp, sign}]}` where
//! `sign` is the base64 HMAC of `{unix seconds}GET/users/self/verify`, then
//! subscribe to the `orders` channel for SPOT. Every push carries the order's
//! cumulative state (`accFillSz`, `avgPx`, `state`). The push's `tradeId` /
//! `fillSz` describe the last trade and can be repeated on later pushes (a cancel
//! after a partial fill may carry them again), and the execution path does not
//! dedupe by execution id — so a push is never booked as one trade. Instead the
//! OMS books whatever OKX has filled that it hasn't
//! ([`crate::recon_orders::apply_broker_progress`]), and `filled` / `canceled` /
//! `mmp_canceled` close the order through reconcile
//! ([`crate::recon_orders::apply_broker_terminal`]). Both are idempotent.

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use sqlx::PgPool;
use tokio::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};
use uuid::Uuid;

use crate::event_bus::EventBus;
use crate::adapters::okx::{oms_order_id, parse_order_state, OkxAdapter};
use crate::kafka::KafkaClient;
use crate::recon_orders::{apply_broker_progress, apply_broker_terminal, run_order_reconcile, BrokerOrderState};
use crate::stream_health::StreamHandle;
use crate::stream_supervisor::{supervise, Session, StreamResult};

const VENUE: &str = "OKX";

/// OKX drops a connection that sends nothing for 30s; each beat sends "ping"
/// and also sweeps for missed terminal reports.
const HEARTBEAT_SECS: u64 = 20;

struct OkxSession {
    pool: PgPool,
    kafka: Option<KafkaClient>,
    adapter: Arc<OkxAdapter>,
    health: StreamHandle,
    bus: Option<EventBus>,
}

#[async_trait::async_trait]
impl Session for OkxSession {
    async fn run_once(&mut self) -> StreamResult {
        connect_and_run(&self.adapter, &self.pool, &self.kafka, &self.health, self.bus.as_ref()).await
    }
}

pub async fn run(
    environment: &'static str,
    pool: PgPool,
    kafka: Option<KafkaClient>,
    adapter: Arc<OkxAdapter>,
    health: StreamHandle,
    bus: Option<EventBus>,
) {
    info!(env = environment, "starting OKX private stream");
    let session = OkxSession { pool, kafka, adapter, health: health.clone(), bus };
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

/// What one pushed entry means for its OMS order.
#[derive(Debug)]
pub(crate) enum Update {
    /// OKX's cumulative filled size and average price for a working order.
    Progress { cum: f64, avg: f64 },
    /// A cancel for an order with fills: apply through reconcile so any fill the
    /// OMS missed is booked before the close.
    Terminal(BrokerOrderState),
}

/// The updates one `orders` push carries, by OMS order id. Entries for orders the
/// OMS didn't place (no UUID `clOrdId`) are dropped.
pub(crate) fn updates(msg: &serde_json::Value) -> Vec<(Uuid, Update)> {
    if msg["arg"]["channel"].as_str() != Some("orders") {
        return Vec::new();
    }
    let data = msg["data"].as_array().cloned().unwrap_or_default();
    data.iter()
        .filter_map(|d| {
            let id = oms_order_id(d["clOrdId"].as_str()?)?;
            let st = parse_order_state(d);
            let update = match d["state"].as_str()? {
                "filled" | "canceled" | "mmp_canceled" => Update::Terminal(st),
                _ if st.executed_qty > 0.0 => Update::Progress { cum: st.executed_qty, avg: st.avg_px },
                // live, nothing filled yet: nothing new.
                _ => return None,
            };
            Some((id, update))
        })
        .collect()
}

async fn connect_and_run(
    adapter: &OkxAdapter,
    pool: &PgPool,
    kafka: &Option<KafkaClient>,
    health: &StreamHandle,
    bus: Option<&EventBus>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (mut ws, _) = connect_async(adapter.private_ws_url()).await?;
    info!("OKX private WS connected");
    ws.send(Message::Text(login_message(adapter, chrono::Utc::now().timestamp()).to_string())).await?;

    let mut heartbeat = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECS));
    heartbeat.tick().await; // consume the immediate first tick

    loop {
        let text = tokio::select! {
            _ = heartbeat.tick() => {
                ws.send(Message::Text("ping".into())).await?;
                run_order_reconcile(pool, kafka, adapter, VENUE, "okx", bus).await;
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
            for (order_id, update) in updates(&v) {
                match update {
                    Update::Terminal(st) => {
                        apply_broker_terminal(pool, kafka, order_id, &st, VENUE, "okx", bus).await;
                    }
                    Update::Progress { cum, avg } => {
                        apply_broker_progress(pool, kafka, order_id, cum, avg, VENUE, "okx", bus).await;
                    }
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
                // Snapshot-on-(re)connect: heal anything that resolved during the outage.
                run_order_reconcile(pool, kafka, adapter, VENUE, "okx", bus).await;
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
    use crate::recon_orders::BrokerOrderOutcome;

    const ORDER_ID: &str = "3f6b1c2e-8a4d-4e5f-9b21-1c2d3e4f5a6b";

    fn sample(name: &str) -> serde_json::Value {
        let text = match name {
            "ws_orders_fill" => include_str!("adapters/testdata/okx/ws_orders_fill.json"),
            "ws_orders_lifecycle" => include_str!("adapters/testdata/okx/ws_orders_lifecycle.json"),
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

    // OKX repeats the last fill's tradeId/fillSz on later pushes for the same
    // order, so a push is read as OKX's cumulative state, never as one trade:
    // booking what the OMS is missing is idempotent across repeats.
    #[test]
    fn a_partial_fill_push_is_cumulative_progress() {
        let u = updates(&sample("ws_orders_fill"));
        assert_eq!(u.len(), 2, "the manual order is not ours");
        assert_eq!(u[0].0.to_string(), ORDER_ID);
        assert!(matches!(u[0].1, Update::Progress { cum, avg } if cum == 0.0004 && avg == 65000.0), "{:?}", u[0].1);
    }

    #[test]
    fn a_filled_push_closes_through_reconcile_with_okx_totals() {
        let u = updates(&sample("ws_orders_fill"));
        match &u[1].1 {
            Update::Terminal(st) => {
                assert_eq!(st.outcome, BrokerOrderOutcome::Filled);
                assert_eq!(st.executed_qty, 0.001);
                assert_eq!(st.avg_px, 65006.0);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn live_is_nothing_and_a_cancel_without_fills_closes_with_nothing_filled() {
        let u = updates(&sample("ws_orders_lifecycle"));
        assert_eq!(u.len(), 2, "live produces nothing");
        assert_eq!(u[0].0.to_string(), ORDER_ID);
        assert!(
            matches!(&u[0].1, Update::Terminal(st) if st.outcome == BrokerOrderOutcome::Canceled && st.executed_qty == 0.0),
            "{:?}",
            u[0].1
        );
    }

    #[test]
    fn a_cancel_repeating_its_last_fill_closes_with_okx_totals() {
        let u = updates(&sample("ws_orders_lifecycle"));
        let (id, update) = &u[1];
        assert_eq!(id.to_string(), "9a8b7c6d-0000-4000-8000-000000000002");
        match update {
            Update::Terminal(st) => {
                assert_eq!(st.outcome, BrokerOrderOutcome::Canceled);
                assert_eq!(st.executed_qty, 0.0005);
                assert_eq!(st.avg_px, 65000.0);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn other_channels_are_ignored() {
        let v = serde_json::from_str(include_str!("adapters/testdata/okx/ws_tickers.json")).unwrap();
        assert!(updates(&v).is_empty());
    }
}
