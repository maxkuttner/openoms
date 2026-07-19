//! Bybit spot top-of-book, as a [`LiveQuoteFeed`].
//!
//! Subscribes to `orderbook.1.<PAIR>` on the v5 public spot stream — no key, no
//! signing. Like the Binance feed this is production data used to mark positions
//! held on a paper venue: a real price is the only useful one.
//!
//! Bybit exists here as a *second* source for the same crypto instruments, so a
//! Binance outage does not leave positions unmarked. It is ranked below Binance
//! (see `provider_feed_policy`) because we trade on Binance — its book is the one
//! our fills actually print against. Bybit's price for the same pair is a close
//! proxy (arbitrage holds them within a few bps), not the same book.

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

const WS_URL: &str = "wss://stream.bybit.com/v5/public/spot";
const SOURCE_CODE: &str = "BYBIT";

/// Bybit closes a connection that has been silent for 30s; its docs prescribe an
/// application-level `{"op":"ping"}` every 20s. This is *not* a WebSocket ping
/// frame — the protocol-level one does not reset their timer.
const PING_SECS: u64 = 20;

/// Best bid/ask carried across messages.
///
/// `orderbook.1` is a stateful topic: a `snapshot` replaces the book, a `delta`
/// carries only the side that changed. Emitting on a delta without remembering the
/// other side would publish a half-quote, so the last known level is kept per pair
/// and a quote is only emitted once both sides are known.
#[derive(Default, Clone, Copy)]
struct Top {
    bid: Option<(f64, f64)>,
    ask: Option<(f64, f64)>,
}

pub struct BybitFeed;

impl DataProvider for BybitFeed {
    fn code(&self) -> &'static str {
        SOURCE_CODE
    }
}

impl FeedSymbology for BybitFeed {
    /// Every crypto pair we know, on any venue — the 1:n case. Bybit is a data-only
    /// source here: under broker-first seeding it lists no instruments of its own,
    /// so its price for `BTCUSDT` marks the Binance-seeded `BTCUSDT`. Leaving
    /// `venue` open is what lets one feed symbol map to the pair on several venues.
    fn candidates(&self) -> InstrumentFilter {
        InstrumentFilter {
            instrument_class: Some("SPOT"),
            asset_class: Some("CRYPTO"),
            venue: None,
        }
    }

    /// Identity: the master symbol is the pair, matching the `s` field verbatim.
    fn to_feed_symbol(&self, symbol: &str) -> Option<String> {
        Some(symbol.to_string())
    }
}

#[async_trait::async_trait]
impl LiveQuoteFeed for BybitFeed {
    async fn run_session(
        &self,
        symbols: HashMap<String, i64>,
        out: &mpsc::Sender<Quote>,
        add_rx: &mut mpsc::Receiver<SymbolAdds>,
        health: &dyn FeedHealth,
    ) -> Result<(), ProviderError> {
        let mut sym_to_id = symbols;
        let mut books: HashMap<String, Top> = HashMap::new();

        let (mut ws, _) = connect_async(WS_URL)
            .await
            .map_err(|e| ProviderError::Request(e.to_string()))?;
        info!(url = WS_URL, "Bybit feed: connected");

        subscribe(&mut ws, sym_to_id.keys().cloned().collect()).await?;
        health.on_connected();
        info!(count = sym_to_id.len(), "Bybit feed: live");

        let mut ping = interval(Duration::from_secs(PING_SECS));
        ping.tick().await; // consume the immediate first tick

        loop {
            let text = tokio::select! {
                _ = ping.tick() => {
                    ws.send(Message::Text(r#"{"op":"ping"}"#.to_string()))
                        .await
                        .map_err(|e| ProviderError::Request(e.to_string()))?;
                    continue;
                }
                adds = add_rx.recv() => {
                    match adds {
                        Some(adds) => {
                            let pairs: Vec<String> = adds.keys().cloned().collect();
                            info!(count = pairs.len(), "Bybit feed: subscribing new symbols");
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

            let v: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, raw = %text, "Bybit feed: parse failed");
                    continue;
                }
            };

            // Subscribe acks and pongs carry `op`, not `topic`.
            if v.get("topic").is_none() {
                if v["success"] == serde_json::json!(false) {
                    warn!(raw = %text, "Bybit feed: request rejected");
                }
                continue;
            }

            let data = &v["data"];
            let Some(pair) = data["s"].as_str() else { continue };
            let Some(&instrument_id) = sym_to_id.get(pair) else { continue };

            let is_snapshot = v["type"].as_str() == Some("snapshot");
            let book = books.entry(pair.to_string()).or_default();
            if is_snapshot {
                // A snapshot is the whole book: a side absent from it is genuinely
                // gone, so do not carry the previous level forward.
                *book = Top::default();
            }
            if let Some(bid) = level(&data["b"]) {
                book.bid = bid;
            }
            if let Some(ask) = level(&data["a"]) {
                book.ask = ask;
            }

            let (Some((bid, bid_sz)), Some((ask, ask_sz))) = (book.bid, book.ask) else {
                continue; // only one side known so far
            };

            let quote = Quote {
                instrument_id,
                bid,
                ask,
                bid_size: bid_sz as u32,
                ask_size: ask_sz as u32,
                ts_recv: Utc::now(),
                source_code: SOURCE_CODE,
            };
            if out.send(quote).await.is_err() {
                warn!("Bybit feed: mark router gone, ending session");
                return Ok(());
            }
        }

        Ok(())
    }
}

async fn subscribe<S>(ws: &mut S, pairs: Vec<String>) -> Result<(), ProviderError>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::fmt::Display,
{
    if pairs.is_empty() {
        return Ok(());
    }
    let args: Vec<String> = pairs.iter().map(|p| format!("orderbook.1.{p}")).collect();
    let msg = serde_json::json!({ "op": "subscribe", "args": args });
    ws.send(Message::Text(msg.to_string()))
        .await
        .map_err(|e| ProviderError::Request(e.to_string()))
}

/// Read one side of an `orderbook.1` update.
///
/// Returns `None` when the side is absent (a delta that didn't touch it — keep the
/// previous level) and `Some(None)` when it was explicitly removed (size "0"),
/// which must clear the level rather than leave a stale price standing.
#[allow(clippy::option_option)]
fn level(side: &serde_json::Value) -> Option<Option<(f64, f64)>> {
    let arr = side.as_array()?;
    let first = arr.first()?;
    let price: f64 = first.get(0)?.as_str()?.parse().ok()?;
    let size: f64 = first.get(1)?.as_str()?.parse().ok()?;
    Some(if size == 0.0 { None } else { Some((price, size)) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(s: &str) -> serde_json::Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn reads_a_level() {
        let v = json(r#"{"b":[["74.72","138.8075"]]}"#);
        assert_eq!(level(&v["b"]), Some(Some((74.72, 138.8075))));
    }

    #[test]
    fn absent_side_leaves_previous_level_alone() {
        // A delta that didn't touch this side: [] means "no change", not "no bid".
        let v = json(r#"{"b":[]}"#);
        assert_eq!(level(&v["b"]), None);
    }

    #[test]
    fn zero_size_clears_the_level() {
        // Bybit deletes a level by sending size "0" — carrying the old price
        // forward here would leave a stale bid standing after it was pulled.
        let v = json(r#"{"b":[["74.72","0"]]}"#);
        assert_eq!(level(&v["b"]), Some(None));
    }

    #[test]
    fn decodes_a_real_snapshot_frame() {
        let raw = r#"{"topic":"orderbook.1.SOLUSDT","ts":1784287273269,"type":"snapshot","data":{"s":"SOLUSDT","b":[["74.72","138.8075"]],"a":[["74.73","16.3301"]],"u":8666785,"seq":155684298279}}"#;
        let v = json(raw);
        assert_eq!(v["data"]["s"].as_str(), Some("SOLUSDT"));
        assert_eq!(v["type"].as_str(), Some("snapshot"));
        assert_eq!(level(&v["data"]["b"]), Some(Some((74.72, 138.8075))));
        assert_eq!(level(&v["data"]["a"]), Some(Some((74.73, 16.3301))));
    }
}
