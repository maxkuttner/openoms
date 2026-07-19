//! Binance spot top-of-book, as a [`LiveQuoteFeed`].
//!
//! Subscribes to `<pair>@bookTicker` — best bid/ask pushed on every change. This
//! is a public stream: no key, no signing, nothing in common with the authenticated
//! WS-API the order stream uses (`binance_stream.rs`).
//!
//! **Production, deliberately, even though orders route to testnet.** Testnet runs
//! its own synthetic book, so marking against it would value positions at prices no
//! one can trade. Real prices make the mark meaningful; P&L against a testnet fill
//! is meaningless either way, so the choice is between a useful number and a
//! useless one.

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

/// Public market-data endpoint. Raw (`/ws`) rather than combined (`/stream`), so
/// symbols can be added mid-session with a SUBSCRIBE frame.
const WS_URL: &str = "wss://stream.binance.com:9443/ws";
const SOURCE_CODE: &str = "BINANCE";

/// Override the endpoint — for pointing at a mock, or at testnet's book if you ever
/// want marks consistent with testnet fills rather than with the real market.
/// Setting it to an unreachable host is also how the failover path gets exercised
/// without unplugging anything.
fn ws_url() -> String {
    std::env::var("BINANCE_FEED_WS_URL").unwrap_or_else(|_| WS_URL.to_string())
}

pub struct BinanceFeed;

impl DataProvider for BinanceFeed {
    fn code(&self) -> &'static str {
        SOURCE_CODE
    }
}

impl FeedSymbology for BinanceFeed {
    /// Only the pairs Binance itself lists. Scoped to the venue because this feed
    /// is the exchange's own book — unlike Bybit, it has no claim on a pair listed
    /// somewhere else.
    fn candidates(&self) -> InstrumentFilter {
        InstrumentFilter {
            instrument_class: Some("SPOT"),
            asset_class: Some("CRYPTO"),
            venue: Some("BINANCE"),
        }
    }

    /// Identity: the master symbol *is* the pair, and matches the uppercase `s`
    /// field Binance sends. (The lowercasing for the subscribe frame happens at
    /// subscribe time in `run_session` — it is wire framing, not identity.)
    fn to_feed_symbol(&self, symbol: &str) -> Option<String> {
        Some(symbol.to_string())
    }
}

#[async_trait::async_trait]
impl LiveQuoteFeed for BinanceFeed {
    async fn run_session(
        &self,
        symbols: HashMap<String, i64>,
        out: &mpsc::Sender<Quote>,
        add_rx: &mut mpsc::Receiver<SymbolAdds>,
        health: &dyn FeedHealth,
    ) -> Result<(), ProviderError> {
        // Keys are the pair as Binance reports it in `s` (uppercase, e.g. SOLUSDT).
        let mut sym_to_id = symbols;

        let url = ws_url();
        let (mut ws, _) = connect_async(&url)
            .await
            .map_err(|e| ProviderError::Request(e.to_string()))?;
        info!(url = %url, "Binance feed: connected");

        let mut req_id: u64 = 1;
        subscribe(&mut ws, sym_to_id.keys().cloned().collect(), &mut req_id).await?;
        health.on_connected();
        info!(count = sym_to_id.len(), "Binance feed: live");

        loop {
            let text = tokio::select! {
                adds = add_rx.recv() => {
                    match adds {
                        Some(adds) => {
                            let pairs: Vec<String> = adds.keys().cloned().collect();
                            info!(count = pairs.len(), "Binance feed: subscribing new symbols");
                            sym_to_id.extend(adds);
                            subscribe(&mut ws, pairs, &mut req_id).await?;
                        }
                        // Driver gone; nothing more will be added, but keep streaming.
                        None => std::future::pending::<()>().await,
                    }
                    continue;
                }
                msg = ws.next() => {
                    let Some(msg) = msg else { break };
                    health.on_event();
                    match msg.map_err(|e| ProviderError::Request(e.to_string()))? {
                        Message::Text(t) => t,
                        // Binance pings every few minutes and closes the socket if we
                        // do not pong. tungstenite does not answer for us.
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
                    warn!(error = %e, raw = %text, "Binance feed: parse failed");
                    continue;
                }
            };

            // SUBSCRIBE acks look like {"result":null,"id":1} — no `s` field.
            let Some(pair) = v["s"].as_str() else { continue };
            let Some(&instrument_id) = sym_to_id.get(pair) else { continue };

            // bookTicker sends prices as strings; a malformed one is not a reason to
            // drop the session, but it must not become a 0.0 mark either.
            let (Some(bid), Some(ask)) = (num(&v["b"]), num(&v["a"])) else {
                warn!(pair, raw = %text, "Binance feed: unparseable price, skipping");
                continue;
            };

            let quote = Quote {
                instrument_id,
                bid,
                ask,
                bid_size: num(&v["B"]).unwrap_or(0.0) as u32,
                ask_size: num(&v["A"]).unwrap_or(0.0) as u32,
                ts_recv: Utc::now(),
                source_code: SOURCE_CODE,
            };
            if out.send(quote).await.is_err() {
                warn!("Binance feed: mark router gone, ending session");
                return Ok(());
            }
        }

        Ok(())
    }
}

/// Binance wants lowercase stream names (`solusdt@bookTicker`) but reports the pair
/// uppercase in `s`. Our xref stores the uppercase form, so only the request side
/// is lowered.
async fn subscribe<S>(ws: &mut S, pairs: Vec<String>, req_id: &mut u64) -> Result<(), ProviderError>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::fmt::Display,
{
    if pairs.is_empty() {
        return Ok(());
    }
    let params: Vec<String> = pairs
        .iter()
        .map(|p| format!("{}@bookTicker", p.to_lowercase()))
        .collect();
    let msg = serde_json::json!({ "method": "SUBSCRIBE", "params": params, "id": req_id });
    *req_id += 1;
    ws.send(Message::Text(msg.to_string()))
        .await
        .map_err(|e| ProviderError::Request(e.to_string()))
}

/// Binance encodes every number as a JSON string.
fn num(v: &serde_json::Value) -> Option<f64> {
    v.as_str()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_binance_string_numbers() {
        let v: serde_json::Value = serde_json::from_str(r#"{"b":"25.35190000"}"#).unwrap();
        assert_eq!(num(&v["b"]), Some(25.3519));
    }

    #[test]
    fn missing_or_malformed_price_is_none() {
        let v: serde_json::Value = serde_json::from_str(r#"{"b":"","a":null}"#).unwrap();
        assert_eq!(num(&v["b"]), None);
        assert_eq!(num(&v["a"]), None);
        assert_eq!(num(&v["nope"]), None);
    }

    /// A real bookTicker frame must yield both sides.
    #[test]
    fn decodes_a_book_ticker_frame() {
        let raw = r#"{"u":400900217,"s":"SOLUSDT","b":"25.35190000","B":"31.21000000","a":"25.36520000","A":"40.66000000"}"#;
        let v: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert_eq!(v["s"].as_str(), Some("SOLUSDT"));
        assert_eq!(num(&v["b"]), Some(25.3519));
        assert_eq!(num(&v["a"]), Some(25.3652));
    }
}
