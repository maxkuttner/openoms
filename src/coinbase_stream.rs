//! Coinbase Advanced Trade private user stream — real-time order updates,
//! mirroring the Bybit and Binance streams (no polling).
//!
//! Connect `wss://advanced-trade-ws-user.coinbase.com` and, within 5 seconds,
//! subscribe to `user` (orders) and `heartbeats` (keeps an idle connection open),
//! each with a fresh JWT. Coinbase reports **cumulative** order state —
//! `cumulative_quantity` and `avg_price` — not one message per fill. A fill is
//! therefore "Coinbase cumulative − what the OMS has booked", priced against
//! the OMS's booked average ([`crate::recon_orders::apply_broker_progress`]), so
//! a repeated, replayed or reordered update books nothing and needs no memory of
//! earlier messages; the execution path has no execution-id dedupe to lean on. A terminal status goes through
//! [`crate::recon_orders::apply_broker_terminal`]: book what's missing, then close.

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use sqlx::PgPool;
use tokio::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};
use uuid::Uuid;

use crate::event_bus::EventBus;
use crate::adapters::coinbase::{order_outcome, CoinbaseAdapter};
use crate::kafka::KafkaClient;
use crate::recon_orders::{apply_broker_progress, apply_broker_terminal, run_order_reconcile, BrokerOrderState};
use crate::stream_health::StreamHandle;
use crate::stream_supervisor::{supervise, Session, StreamResult};

const VENUE: &str = "COINBASE";
const WS_URL: &str = "wss://advanced-trade-ws-user.coinbase.com";

/// How often to sweep for missed terminal states; the `heartbeats` channel keeps
/// the socket itself alive.
const RECONCILE_SECS: u64 = 20;

struct CoinbaseSession {
    pool: PgPool,
    kafka: Option<KafkaClient>,
    adapter: Arc<CoinbaseAdapter>,
    health: StreamHandle,
    bus: Option<EventBus>,
}

#[async_trait::async_trait]
impl Session for CoinbaseSession {
    async fn run_once(&mut self) -> StreamResult {
        connect_and_run(&self.adapter, &self.pool, &self.kafka, &self.health, self.bus.as_ref()).await
    }
}

pub async fn run(
    environment: &'static str,
    pool: PgPool,
    kafka: Option<KafkaClient>,
    adapter: Arc<CoinbaseAdapter>,
    health: StreamHandle,
    bus: Option<EventBus>,
) {
    info!(env = environment, "starting Coinbase user stream");
    let session = CoinbaseSession { pool, kafka, adapter, health: health.clone(), bus };
    supervise("COINBASE/LIVE", health, session).await
}

/// The two subscribe requests, each with its own fresh JWT (no `uri`).
pub(crate) fn subscribe_messages(adapter: &CoinbaseAdapter) -> [serde_json::Value; 2] {
    ["user", "heartbeats"].map(|channel| {
        serde_json::json!({ "type": "subscribe", "channel": channel, "jwt": adapter.jwt(None) })
    })
}

/// What one order entry means for its OMS order.
#[derive(Debug)]
pub(crate) enum Update {
    /// Still working, with something filled: book what the OMS hasn't yet.
    Progress { cum: f64, avg: f64 },
    /// Finished: book what's missing, then close.
    Terminal(BrokerOrderState),
}

/// The updates one `user` message carries, by OMS order id. Orders the OMS
/// didn't place (no UUID `client_order_id`) and open orders with nothing filled
/// are skipped.
pub(crate) fn updates(msg: &serde_json::Value) -> Vec<(Uuid, Update)> {
    if msg["channel"].as_str() != Some("user") {
        return Vec::new();
    }
    let events = msg["events"].as_array().cloned().unwrap_or_default();
    events
        .iter()
        .flat_map(|e| e["orders"].as_array().cloned().unwrap_or_default())
        .filter_map(|o| {
            let id = Uuid::parse_str(o["client_order_id"].as_str()?).ok()?;
            let num = |k: &str| o[k].as_str().and_then(|s| s.parse::<f64>().ok());
            let cum = num("cumulative_quantity").unwrap_or(0.0);
            let avg = num("avg_price").unwrap_or(0.0);
            let status = o["status"].as_str()?;
            let update = match status {
                "FILLED" | "CANCELLED" | "EXPIRED" | "FAILED" => Update::Terminal(BrokerOrderState {
                    outcome: order_outcome(status),
                    executed_qty: cum,
                    avg_px: avg,
                }),
                _ if cum > 0.0 => Update::Progress { cum, avg },
                _ => return None,
            };
            Some((id, update))
        })
        .collect()
}

async fn connect_and_run(
    adapter: &CoinbaseAdapter,
    pool: &PgPool,
    kafka: &Option<KafkaClient>,
    health: &StreamHandle,
    bus: Option<&EventBus>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (mut ws, _) = connect_async(WS_URL).await?;
    info!("Coinbase user WS connected");
    // Coinbase drops a connection that hasn't subscribed within 5 seconds.
    for msg in subscribe_messages(adapter) {
        ws.send(Message::Text(msg.to_string())).await?;
    }

    let mut live = false;
    let mut sweep = tokio::time::interval(Duration::from_secs(RECONCILE_SECS));
    sweep.tick().await; // consume the immediate first tick

    loop {
        let text = tokio::select! {
            _ = sweep.tick() => {
                run_order_reconcile(pool, kafka, adapter, VENUE, "coinbase", bus).await;
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
            run_order_reconcile(pool, kafka, adapter, VENUE, "coinbase", bus).await;
        }
        for (order_id, update) in updates(&v) {
            match update {
                Update::Progress { cum, avg } => {
                    apply_broker_progress(pool, kafka, order_id, cum, avg, VENUE, "coinbase", bus).await;
                }
                Update::Terminal(st) => {
                    apply_broker_terminal(pool, kafka, order_id, &st, VENUE, "coinbase", bus).await;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recon_orders::{missing_qty, BrokerOrderOutcome};

    const ORDER_ID: &str = "3f6b1c2e-8a4d-4e5f-9b21-1c2d3e4f5a6b";

    fn sample(name: &str) -> serde_json::Value {
        let text = match name {
            "snapshot" => include_str!("adapters/testdata/coinbase/ws_user_snapshot.json"),
            "partial" => include_str!("adapters/testdata/coinbase/ws_user_partial.json"),
            "terminal" => include_str!("adapters/testdata/coinbase/ws_user_terminal.json"),
            other => panic!("no sample {other}"),
        };
        serde_json::from_str(text).unwrap()
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
    fn a_partial_fill_is_progress_with_cumulative_and_average() {
        let u = updates(&sample("partial"));
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].0.to_string(), ORDER_ID);
        assert!(matches!(u[0].1, Update::Progress { cum, avg } if cum == 0.0005 && avg == 65000.0), "{:?}", u[0].1);
    }

    #[test]
    fn an_open_order_with_nothing_filled_is_not_an_update() {
        // The snapshot's own order has cumulative 0; the other was placed outside the OMS.
        assert!(updates(&sample("snapshot")).is_empty());
    }

    #[test]
    fn filled_cancelled_expired_failed_are_terminal() {
        let u = updates(&sample("terminal"));
        let outcomes: Vec<_> = u
            .iter()
            .map(|(_, up)| match up {
                Update::Terminal(st) => (st.outcome, st.executed_qty),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            outcomes,
            [
                (BrokerOrderOutcome::Filled, 0.001),
                (BrokerOrderOutcome::Canceled, 0.0),
                (BrokerOrderOutcome::Canceled, 0.0002),
                (BrokerOrderOutcome::Rejected, 0.0),
            ]
        );
    }

    // The booking rule is "Coinbase cumulative − OMS cum_qty", so a repeated or
    // replayed update (same cumulative) books nothing.
    #[test]
    fn a_repeat_books_nothing() {
        assert_eq!(missing_qty(0.0, 0.0005), Some(0.0005));
        assert_eq!(missing_qty(0.0005, 0.0005), None);
        assert_eq!(missing_qty(0.001, 0.0005), None, "an older update after a newer one");
    }


    #[test]
    fn a_non_uuid_client_order_id_is_ignored() {
        let mut msg = sample("partial");
        msg["events"][0]["orders"][0]["client_order_id"] = "placed-in-the-coinbase-app".into();
        assert!(updates(&msg).is_empty());
    }
}
