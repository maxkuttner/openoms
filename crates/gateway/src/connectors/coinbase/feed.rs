//! Coinbase spot top-of-book, as a [`LiveQuoteFeed`].
//!
//! Subscribes to the public `ticker` channel on Coinbase's Advanced Trade market
//! data stream — no key, no JWT — plus `heartbeats`, without which Coinbase closes
//! a quiet connection after 60–90 seconds. It marks `COINBASE` instruments only:
//! Coinbase products (`BTC-USD`) are their own instruments, and no other feed can
//! price them.

use std::collections::HashMap;

use chrono::Utc;
use dataprovider::{
    DataProvider, FeedHealth, FeedSymbology, InstrumentFilter, LiveQuoteFeed, ProviderError, Quote,
    SymbolAdds,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};

const WS_URL: &str = "wss://advanced-trade-ws.coinbase.com";
const SOURCE_CODE: &str = "COINBASE";

pub struct CoinbaseFeed;

impl DataProvider for CoinbaseFeed {
    fn code(&self) -> &'static str {
        SOURCE_CODE
    }
}

impl FeedSymbology for CoinbaseFeed {
    /// Coinbase's own products only — the venue Coinbase seeds them under.
    fn candidates(&self) -> InstrumentFilter {
        InstrumentFilter {
            instrument_class: Some("SPOT"),
            asset_class: Some("CRYPTO"),
            venue: Some("COINBASE"),
        }
    }

    /// Identity: the master symbol is the product id.
    fn to_feed_symbol(&self, symbol: &str) -> Option<String> {
        Some(symbol.to_string())
    }
}

/// One product's best bid and ask from a ticker event.
#[derive(Debug, PartialEq)]
pub(crate) struct Top {
    pub product_id: String,
    pub bid: f64,
    pub ask: f64,
    pub bid_size: f64,
    pub ask_size: f64,
}

/// The `ticker` and `heartbeats` subscribe requests.
pub(crate) fn subscribe_messages(products: &[String]) -> [serde_json::Value; 2] {
    [
        serde_json::json!({ "type": "subscribe", "channel": "ticker", "product_ids": products }),
        serde_json::json!({ "type": "subscribe", "channel": "heartbeats" }),
    ]
}

/// The quotes one `ticker` message carries; a ticker with either side missing
/// is skipped rather than published as a half-quote.
pub(crate) fn tops(msg: &serde_json::Value) -> Vec<Top> {
    if msg["channel"].as_str() != Some("ticker") {
        return Vec::new();
    }
    let events = msg["events"].as_array().cloned().unwrap_or_default();
    events
        .iter()
        .flat_map(|e| e["tickers"].as_array().cloned().unwrap_or_default())
        .filter_map(|t| {
            let num = |k: &str| t[k].as_str().and_then(|s| s.parse::<f64>().ok());
            Some(Top {
                product_id: t["product_id"].as_str()?.to_string(),
                bid: num("best_bid").filter(|p| *p > 0.0)?,
                ask: num("best_ask").filter(|p| *p > 0.0)?,
                bid_size: num("best_bid_quantity").unwrap_or(0.0),
                ask_size: num("best_ask_quantity").unwrap_or(0.0),
            })
        })
        .collect()
}

async fn subscribe<S>(ws: &mut S, products: Vec<String>) -> Result<(), ProviderError>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::fmt::Display,
{
    if products.is_empty() {
        return Ok(());
    }
    for msg in subscribe_messages(&products) {
        ws.send(Message::Text(msg.to_string()))
            .await
            .map_err(|e| ProviderError::Request(e.to_string()))?;
    }
    Ok(())
}

#[async_trait::async_trait]
impl LiveQuoteFeed for CoinbaseFeed {
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
        info!(url = WS_URL, "Coinbase feed: connected");

        subscribe(&mut ws, sym_to_id.keys().cloned().collect()).await?;
        health.on_connected();
        info!(count = sym_to_id.len(), "Coinbase feed: live");

        loop {
            let text = tokio::select! {
                adds = add_rx.recv() => {
                    match adds {
                        Some(adds) => {
                            let products: Vec<String> = adds.keys().cloned().collect();
                            info!(count = products.len(), "Coinbase feed: subscribing new symbols");
                            sym_to_id.extend(adds);
                            subscribe(&mut ws, products).await?;
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

            let v: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, raw = %text, "Coinbase feed: parse failed");
                    continue;
                }
            };
            if v["type"].as_str() == Some("error") {
                warn!(raw = %text, "Coinbase feed: request rejected");
                continue;
            }

            for top in tops(&v) {
                let Some(&instrument_id) = sym_to_id.get(&top.product_id) else { continue };
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
                    warn!("Coinbase feed: mark router gone, ending session");
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
        serde_json::from_str(include_str!("testdata/ws_ticker.json")).unwrap()
    }

    #[test]
    fn a_ticker_event_is_a_quote_from_best_bid_and_ask() {
        let q = tops(&sample());
        let btc = q.iter().find(|t| t.product_id == "BTC-USD").unwrap();
        assert_eq!((btc.bid, btc.ask), (77550.86, 77550.87));
        assert_eq!((btc.bid_size, btc.ask_size), (0.25, 0.4));
    }

    #[test]
    fn an_empty_side_emits_nothing() {
        assert!(tops(&sample()).iter().all(|t| t.product_id != "ETH-EUR"));
    }

    #[test]
    fn coinbase_feed_covers_only_coinbase_venue() {
        let f = CoinbaseFeed.candidates();
        assert_eq!(f.venue, Some("COINBASE"));
        assert_eq!(f.instrument_class, Some("SPOT"));
        assert_eq!(CoinbaseFeed.to_feed_symbol("BTC-USD").as_deref(), Some("BTC-USD"));
    }

    #[test]
    fn subscribe_asks_for_ticker_and_heartbeats() {
        let msgs = subscribe_messages(&["BTC-USD".to_string()]);
        assert_eq!(msgs[0]["channel"], "ticker");
        assert_eq!(msgs[0]["product_ids"][0], "BTC-USD");
        assert_eq!(msgs[1]["channel"], "heartbeats");
    }
}
