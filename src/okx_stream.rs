//! OKX spot private stream (v5 WebSocket) — real-time order push, mirroring the
//! Bybit and Binance streams (no polling).
//!
//! Connect `wss://ws.okx.com/ws/v5/private` (Demo Trading: `wspap.okx.com`), log
//! in with `{"op":"login","args":[{apiKey, passphrase, timestamp, sign}]}` where
//! `sign` is the base64 HMAC of `{unix seconds}GET/users/self/verify`, then
//! subscribe to the `orders` channel for SPOT. That one channel carries both
//! fills and state changes, in order per order: an entry with a `tradeId` is one
//! fill (`fillSz` at `fillPx`, keyed by the trade id), and `canceled` /
//! `mmp_canceled` close the order. Applied through the shared
//! [`crate::execution::process_execution_report`].
//!
//! A cancel for an order with fills goes through reconcile
//! ([`crate::recon_orders::apply_broker_terminal`]) rather than closing it
//! directly: whatever OKX filled that the OMS hasn't booked (a fill missed while
//! disconnected) is booked first, then the order closes.

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::adapters::okx::{oms_order_id, parse_order_state, OkxAdapter};
use crate::domain::orders::commands::ExecutionReport;
use crate::execution::process_execution_report;
use crate::kafka::KafkaClient;
use crate::recon_orders::{apply_broker_terminal, run_order_reconcile, BrokerOrderState};
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
    position_changed_tx: Option<mpsc::Sender<()>>,
}

#[async_trait::async_trait]
impl Session for OkxSession {
    async fn run_once(&mut self) -> StreamResult {
        connect_and_run(&self.adapter, &self.pool, &self.kafka, &self.health, self.position_changed_tx.as_ref()).await
    }
}

pub async fn run(
    environment: &'static str,
    pool: PgPool,
    kafka: Option<KafkaClient>,
    adapter: Arc<OkxAdapter>,
    health: StreamHandle,
    position_changed_tx: Option<mpsc::Sender<()>>,
) {
    info!(env = environment, "starting OKX private stream");
    let session = OkxSession { pool, kafka, adapter, health: health.clone(), position_changed_tx };
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
    Report(ExecutionReport),
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
            let num = |k: &str| d[k].as_str().and_then(|s| s.parse::<f64>().ok());
            let trade_id = d["tradeId"].as_str().filter(|t| !t.is_empty());
            if let (Some(trade_id), Some(qty)) = (trade_id, num("fillSz").filter(|q| *q > 0.0)) {
                return Some((
                    id,
                    Update::Report(ExecutionReport::Fill {
                        execution_id: trade_id.to_string(),
                        fill_qty: qty,
                        fill_price: num("fillPx")?,
                        venue: VENUE.to_string(),
                    }),
                ));
            }
            match d["state"].as_str()? {
                "canceled" | "mmp_canceled" if num("accFillSz").unwrap_or(0.0) > 0.0 => {
                    Some((id, Update::Terminal(parse_order_state(d))))
                }
                "canceled" | "mmp_canceled" => Some((
                    id,
                    Update::Report(ExecutionReport::Canceled { reason: None, venue: Some(VENUE.to_string()) }),
                )),
                // live / partially_filled / filled without a trade: nothing new.
                _ => None,
            }
        })
        .collect()
}

async fn connect_and_run(
    adapter: &OkxAdapter,
    pool: &PgPool,
    kafka: &Option<KafkaClient>,
    health: &StreamHandle,
    position_changed_tx: Option<&mpsc::Sender<()>>,
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
                run_order_reconcile(pool, kafka, adapter, VENUE, "okx", position_changed_tx).await;
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
                    Update::Report(report) => {
                        if let Err(e) = process_execution_report(pool, kafka, order_id, report, "okx", position_changed_tx).await {
                            error!(order_id = %order_id, error = %e, "OKX WS: failed to apply execution report");
                        }
                    }
                    Update::Terminal(st) => {
                        apply_broker_terminal(pool, kafka, order_id, &st, VENUE, "okx", position_changed_tx).await;
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
                run_order_reconcile(pool, kafka, adapter, VENUE, "okx", position_changed_tx).await;
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

    #[test]
    fn each_trade_is_one_fill_keyed_by_trade_id() {
        let u = updates(&sample("ws_orders_fill"));
        assert_eq!(u.len(), 2, "the manual order is not ours");
        let fills: Vec<_> = u
            .iter()
            .map(|(id, up)| match up {
                Update::Report(ExecutionReport::Fill { execution_id, fill_qty, fill_price, venue }) => {
                    assert_eq!(id.to_string(), ORDER_ID);
                    assert_eq!(venue, "OKX");
                    (execution_id.clone(), *fill_qty, *fill_price)
                }
                other => panic!("{other:?}"),
            })
            .collect();
        // The `filled` push carries the last fill; it is booked as a fill, not a close.
        assert_eq!(fills, [("123456789".to_string(), 0.0004, 65000.0), ("123456790".to_string(), 0.0006, 65010.0)]);
    }

    #[test]
    fn live_is_nothing_and_a_cancel_without_fills_is_a_plain_cancel() {
        let u = updates(&sample("ws_orders_lifecycle"));
        assert_eq!(u.len(), 2, "live produces nothing");
        assert_eq!(u[0].0.to_string(), ORDER_ID);
        assert!(
            matches!(&u[0].1, Update::Report(ExecutionReport::Canceled { reason: None, venue: Some(v) }) if v == "OKX"),
            "{:?}",
            u[0].1
        );
    }

    #[test]
    fn a_cancel_after_fills_goes_through_reconcile_with_okx_state() {
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
