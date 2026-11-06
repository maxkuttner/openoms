//! OKX spot top-of-book, as a [`LiveQuoteFeed`].
//!
//! Subscribes to the public `tickers` channel on OKX's production v5 stream — no
//! key, no signing. Production prices even for Demo Trading positions: a real
//! price is the only useful mark. It marks `OKX` instruments only: OKX pairs
//! (`BTC-USDT`) are their own instruments, and no other feed prices them.
//!
//! OKX closes a connection that has been silent for 30s; the keepalive is the
//! text frame "ping", answered with "pong".

use std::collections::HashMap;

use chrono::Utc;
use dataprovider::{
    DataProvider, FeedHealth, FeedSymbology, InstrumentFilter, LiveQuoteFeed, ProviderError, Quote,
    SymbolAdds,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};

const WS_URL: &str = "wss://ws.okx.com/ws/v5/public";
const SOURCE_CODE: &str = "OKX";
const PING_SECS: u64 = 20;

pub struct OkxFeed;

impl DataProvider for OkxFeed {
    fn code(&self) -> &'static str {
        SOURCE_CODE
    }
}

impl FeedSymbology for OkxFeed {
    /// OKX's own pairs only — the venue OKX seeds them under.
    fn candidates(&self) -> InstrumentFilter {
        InstrumentFilter {
            instrument_class: Some("SPOT"),
            asset_class: Some("CRYPTO"),
            venue: Some("OKX"),
        }
    }

    /// Identity: the master symbol is the `instId`.
    fn to_feed_symbol(&self, symbol: &str) -> Option<String> {
        Some(symbol.to_string())
    }
}

/// One pair's best bid and ask from a tickers push.
#[derive(Debug, PartialEq)]
pub(crate) struct Top {
    pub inst_id: String,
    pub bid: f64,
    pub ask: f64,
    pub bid_size: f64,
    pub ask_size: f64,
}

/// The `tickers` subscribe request for `pairs`.
pub(crate) fn subscribe_message(pairs: &[String]) -> serde_json::Value {
    let args: Vec<_> = pairs.iter().map(|p| serde_json::json!({"channel": "tickers", "instId": p})).collect();
    serde_json::json!({"op": "subscribe", "args": args})
}

/// The quotes one tickers push carries; a ticker with either side missing is
/// skipped rather than published as a half-quote.
pub(crate) fn tops(msg: &serde_json::Value) -> Vec<Top> {
    if msg["arg"]["channel"].as_str() != Some("tickers") {
        return Vec::new();
    }
    let data = msg["data"].as_array().cloned().unwrap_or_default();
    data.iter()
        .filter_map(|t| {
            let num = |k: &str| t[k].as_str().and_then(|s| s.parse::<f64>().ok());
            Some(Top {
                inst_id: t["instId"].as_str()?.to_string(),
                bid: num("bidPx").filter(|p| *p > 0.0)?,
                ask: num("askPx").filter(|p| *p > 0.0)?,
                bid_size: num("bidSz").unwrap_or(0.0),
                ask_size: num("askSz").unwrap_or(0.0),
            })
        })
        .collect()
}

async fn subscribe<S>(ws: &mut S, pairs: Vec<String>) -> Result<(), ProviderError>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::fmt::Display,
{
    if pairs.is_empty() {
        return Ok(());
    }
    ws.send(Message::Text(subscribe_message(&pairs).to_string()))
        .await
        .map_err(|e| ProviderError::Request(e.to_string()))
}

#[async_trait::async_trait]
impl LiveQuoteFeed for OkxFeed {
    async fn run_session(
        &self,
        symbols: HashMap<String, i64>,
        out: &mpsc::Sender<Quote>,
        add_rx: &mut mpsc::Receiver<SymbolAdds>,
        health: &dyn FeedHealth,
    ) -> Result<(), ProviderError> {
        let mut sym_to_id = symbols;

        let (mut ws, _) = connect_async(WS_URL)
            .await
            .map_err(|e| ProviderError::Request(e.to_string()))?;
        info!(url = WS_URL, "OKX feed: connected");

        subscribe(&mut ws, sym_to_id.keys().cloned().collect()).await?;
        health.on_connected();
        info!(count = sym_to_id.len(), "OKX feed: live");

        let mut ping = interval(Duration::from_secs(PING_SECS));
        ping.tick().await; // consume the immediate first tick

        loop {
            let text = tokio::select! {
                _ = ping.tick() => {
                    ws.send(Message::Text("ping".to_string()))
                        .await
                        .map_err(|e| ProviderError::Request(e.to_string()))?;
                    continue;
                }
                adds = add_rx.recv() => {
                    match adds {
                        Some(adds) => {
                            let pairs: Vec<String> = adds.keys().cloned().collect();
                            info!(count = pairs.len(), "OKX feed: subscribing new symbols");
                            sym_to_id.extend(adds);
                            subscribe(&mut ws, pairs).await?;
                        }
                        None => std::future::pending::<()>().await,
                    }
                    continue;
                }
                msg = ws.next() => {
                    let Some(msg) = msg else { break };
                    health.on_event();
                    match msg.map_err(|e| ProviderError::Request(e.to_string()))? {
                        Message::Text(t) => t,
                        Message::Ping(p) => {
                            ws.send(Message::Pong(p))
                                .await
                                .map_err(|e| ProviderError::Request(e.to_string()))?;
                            continue;
                        }
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
                Err(e) => {
                    warn!(error = %e, raw = %text, "OKX feed: parse failed");
                    continue;
                }
            };
            if v["event"].as_str() == Some("error") {
                warn!(raw = %text, "OKX feed: request rejected");
                continue;
            }

            for top in tops(&v) {
                let Some(&instrument_id) = sym_to_id.get(&top.inst_id) else { continue };
                let quote = Quote {
                    instrument_id,
                    bid: top.bid,
                    ask: top.ask,
                    bid_size: top.bid_size as u32,
                    ask_size: top.ask_size as u32,
                    ts_recv: Utc::now(),
                    source_code: SOURCE_CODE,
                };
                if out.send(quote).await.is_err() {
                    warn!("OKX feed: mark router gone, ending session");
                    return Ok(());
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> serde_json::Value {
        serde_json::from_str(include_str!("testdata/ws_tickers.json")).unwrap()
    }

    #[test]
    fn a_tickers_push_is_a_quote_from_best_bid_and_ask() {
        let q = tops(&sample());
        assert_eq!(
            q,
            [Top { inst_id: "BTC-USDT".into(), bid: 65500.0, ask: 65500.1, bid_size: 0.25, ask_size: 0.4 }]
        );
    }

    #[test]
    fn an_empty_side_emits_nothing() {
        let v = serde_json::json!({"arg":{"channel":"tickers","instId":"X-USDT"},"data":[{"instId":"X-USDT","bidPx":"","askPx":"1","bidSz":"","askSz":"1"}]});
        assert!(tops(&v).is_empty());
    }

    #[test]
    fn okx_feed_covers_only_okx_venue() {
        let f = OkxFeed.candidates();
        assert_eq!(f.venue, Some("OKX"));
        assert_eq!(f.instrument_class, Some("SPOT"));
        assert_eq!(OkxFeed.to_feed_symbol("BTC-USDT").as_deref(), Some("BTC-USDT"));
    }

    #[test]
    fn subscribe_asks_for_tickers_per_pair() {
        let m = subscribe_message(&["BTC-USDT".to_string(), "ETH-USDT".to_string()]);
        assert_eq!(m["op"], "subscribe");
        assert_eq!(m["args"][0], serde_json::json!({"channel": "tickers", "instId": "BTC-USDT"}));
        assert_eq!(m["args"][1]["instId"], "ETH-USDT");
    }
}
