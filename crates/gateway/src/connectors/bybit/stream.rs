//! Bybit spot private stream (v5 WebSocket) — real-time order and execution
//! push, mirroring the Binance and Alpaca streams (no polling).
//!
//! Connect `wss://stream[-testnet].bybit.com/v5/private`, authenticate with
//! `{"op":"auth","args":[api_key, expires, sign("GET/realtime{expires}")]}`, then
//! subscribe to `order.spot` and `execution.spot`. Fills come **only** from
//! `execution.spot` `Trade` entries (one per fill, partials included), keyed by
//! Bybit's `execId`; `order.spot` supplies cancels and rejects. Taking fills from
//! both topics would double-count them: the execution path does not dedupe by
//! execution id. Everything goes to the order engine as execution events
//! ([`crate::order_engine`]), which books it.
//!
//! The two topics are not ordered against each other, so a terminal `order.spot`
//! update for an order with fills (`cumExecQty > 0`) can overtake those fills.
//! Closing the order on it would make the terminal guard drop the late Trades;
//! instead it is resolved like a missed terminal in reconcile
//! ([`crate::contract::ExecKind::Terminal`]): book whatever Bybit filled
//! that the OMS hasn't yet, then close.

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};
use uuid::Uuid;

use crate::connectors::bybit::adapter::BybitAdapter;
use crate::contract::VenueReport;
use crate::contract::{EngineHandle, ExecEvent, ExecKind, ExecSource};
use crate::contract::BrokerOrderState;
use crate::stream_health::StreamHandle;
use crate::stream_supervisor::{supervise, Session, StreamResult};

const VENUE: &str = "BYBIT";

/// Bybit drops a private connection that sends nothing for 30s and asks for a
/// ping every 20s; each beat also sweeps for missed terminal reports.
const HEARTBEAT_SECS: u64 = 20;

/// How long the auth signature stays valid.
const AUTH_EXPIRY_MS: i64 = 10_000;

struct BybitSession {
    environment: &'static str,
    adapter: Arc<BybitAdapter>,
    health: StreamHandle,
    engine: EngineHandle,
}

#[async_trait::async_trait]
impl Session for BybitSession {
    async fn run_once(&mut self) -> StreamResult {
        connect_and_run(self.environment, &self.adapter, &self.health, &self.engine).await
    }
}

pub async fn run(
    environment: &'static str,
    adapter: Arc<BybitAdapter>,
    health: StreamHandle,
    engine: EngineHandle,
) {
    info!(env = environment, "starting Bybit private stream");
    let session = BybitSession { environment, adapter, health: health.clone(), engine };
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

/// What one pushed entry means for its OMS order.
#[derive(Debug)]
pub(crate) enum Update {
    Report(VenueReport),
    /// A terminal order update for an order with fills: apply through reconcile
    /// so fills it may have overtaken are booked before the close.
    Terminal(BrokerOrderState),
}

/// The updates one pushed message carries, by OMS order id. Entries for orders
/// the OMS didn't place (no UUID `orderLinkId`) are dropped.
pub(crate) fn reports(msg: &serde_json::Value) -> Vec<(Uuid, Update)> {
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
                Update::Report(VenueReport::Fill {
                    execution_id: d["execId"].as_str()?.to_string(),
                    fill_qty: num("execQty")?,
                    fill_price: num("execPrice")?,
                    venue: VENUE.to_string(),
                })
            } else if order {
                let status = d["orderStatus"].as_str()?;
                let terminal = matches!(status, "Cancelled" | "PartiallyFilledCanceled" | "Deactivated" | "Rejected");
                if terminal && num("cumExecQty").unwrap_or(0.0) > 0.0 {
                    return Some((id, Update::Terminal(crate::connectors::bybit::adapter::parse_order_state(d))));
                }
                let reason = d["rejectReason"].as_str().filter(|r| !r.is_empty() && *r != "EC_NoError");
                Update::Report(match status {
                    "Cancelled" | "PartiallyFilledCanceled" | "Deactivated" => VenueReport::Canceled {
                        reason: reason.map(String::from),
                        venue: Some(VENUE.to_string()),
                    },
                    "Rejected" => VenueReport::Reject {
                        reason: reason.unwrap_or("Rejected").to_string(),
                        venue: Some(VENUE.to_string()),
                    },
                    // New / PartiallyFilled / Filled: fills arrive on execution.spot.
                    _ => return None,
                })
            } else {
                return None;
            };
            Some((id, report))
        })
        .collect()
}

/// What one parsed update tells the order engine. Fills and order updates
/// arrive on separate, unordered topics, so a close is sent as a terminal state
/// the engine books against what it already has.
fn exec_event(order_id: Uuid, update: Update) -> ExecEvent {
    match update {
        Update::Report(report) => ExecEvent::from_report(order_id, VENUE, "bybit", report),
        Update::Terminal(st) => ExecEvent {
            order_id,
            venue: VENUE.to_string(),
            actor: "bybit",
            source: ExecSource::Session,
            kind: ExecKind::Terminal(st),
        },
    }
}

async fn connect_and_run(
    environment: &'static str,
    adapter: &Arc<BybitAdapter>,
    health: &StreamHandle,
    engine: &EngineHandle,
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
                engine.reconcile(VENUE, environment, "bybit", adapter.clone()).await;
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
            for (order_id, update) in reports(&v) {
                engine.exec(exec_event(order_id, update)).await;
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
                    engine.reconcile(VENUE, environment, "bybit", adapter.clone()).await;
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
    use crate::contract::BrokerOrderOutcome;

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

    #[test]
    fn bybit_closed_order_becomes_terminal_event() {
        let (order_id, update) = reports(&sample("ws_order_cancelled"))
            .into_iter()
            .find(|(_, u)| matches!(u, Update::Terminal(_)))
            .expect("the sample carries a terminal update");
        let ev = exec_event(order_id, update);
        assert_eq!(ev.order_id, order_id);
        assert_eq!(ev.actor, "bybit");
        let crate::contract::ExecKind::Terminal(st) = ev.kind else { panic!("a terminal event") };
        assert_eq!(st.outcome, BrokerOrderOutcome::Canceled);
    }

    #[test]
    fn bybit_execution_becomes_fill_event() {
        let (order_id, update) = reports(&sample("ws_execution")).remove(0);
        assert!(matches!(exec_event(order_id, update).kind, crate::contract::ExecKind::Fill { .. }));
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
            Update::Report(VenueReport::Fill { execution_id, fill_qty, fill_price, venue }) => {
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
        // Nothing filled: a plain cancel.
        for (_, update) in [&cancels[0], &cancels[2]] {
            assert!(
                matches!(update, Update::Report(VenueReport::Canceled { reason: None, venue: Some(v) }) if v == "BYBIT"),
                "{update:?}"
            );
        }
        // Filled before it was cancelled: resolved through reconcile (see below).
        assert!(matches!(&cancels[1].1, Update::Terminal(st) if st.outcome == BrokerOrderOutcome::Canceled), "{:?}", cancels[1].1);
        let rejects = reports(&sample("ws_order_rejected"));
        assert_eq!(rejects.len(), 1);
        assert!(
            matches!(&rejects[0].1, Update::Report(VenueReport::Reject { reason, .. }) if reason == "EC_InsufficientBalance"),
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

    // order.spot and execution.spot are separate topics with no ordering between
    // them: a PartiallyFilledCanceled can arrive before its Trades. Closing the order
    // on it directly would make the terminal guard drop those late Trades, so it
    // carries Bybit's own cumulative state for reconcile to book the fills first.
    #[test]
    fn a_cancel_that_overtakes_its_fills_carries_them() {
        let r = reports(&sample("ws_order_cancelled"));
        let (id, update) = &r[1];
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
    fn order_updates_never_produce_fills() {
        // Fills come only from execution.spot; order.spot PartiallyFilled / Filled
        // would double-count them (the execution path does not dedupe by exec id).
        assert!(reports(&sample("ws_order_filled")).is_empty());
    }
}
