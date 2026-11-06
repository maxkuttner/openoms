//! One feed's session, driven by what the core holds.
//!
//! Owns everything that is the same regardless of vendor: work out which held
//! instruments this feed covers and what it calls them, hand them to the feed,
//! and push newly held ones in mid-session. The feed itself only speaks its wire
//! protocol. Paired with [`crate::stream_supervisor`], a new vendor is one
//! `impl LiveQuoteFeed`.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use dataprovider::{covers, FeedSymbology, InstrumentRef, Quote, SymbolAdds};
use tokio::sync::{mpsc, watch};
use tracing::info;

use crate::connectors::QuoteSource;
use crate::stream_health::StreamHandle;
use crate::stream_supervisor::{Session, StreamResult};

pub struct FeedSession {
    feed: Arc<dyn QuoteSource>,
    held: watch::Receiver<BTreeMap<i64, InstrumentRef>>,
    out: mpsc::Sender<Quote>,
    health: StreamHandle,
}

impl FeedSession {
    pub fn new(
        feed: Arc<dyn QuoteSource>,
        held: watch::Receiver<BTreeMap<i64, InstrumentRef>>,
        out: mpsc::Sender<Quote>,
        health: StreamHandle,
    ) -> Self {
        Self { feed, held, out, health }
    }
}

/// The held instruments `feed` prices, as `feed_symbol -> instrument_id`.
///
/// The mapping is 1:n (one feed symbol may price instruments on several venues),
/// so a `feed_symbol` collision keeps the last instrument id, in id order.
fn covered(feed: &dyn FeedSymbology, held: &BTreeMap<i64, InstrumentRef>) -> HashMap<String, i64> {
    held.values()
        .filter(|i| covers(feed, i))
        .filter_map(|i| feed.to_feed_symbol(&i.symbol).map(|s| (s, i.id)))
        .collect()
}

/// Wait for the held set to change. The sender lives as long as the gateway,
/// so a closed watch means shutdown: park rather than end the session.
async fn changed(held: &mut watch::Receiver<BTreeMap<i64, InstrumentRef>>) {
    if held.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
}

#[async_trait::async_trait]
impl Session for FeedSession {
    async fn run_once(&mut self) -> StreamResult {
        // Deliberately does not return to the supervisor while nothing is held:
        // an idle feed is not a closed stream, and reporting it as one would flap
        // the health badge and log a reconnect every time.
        let known = loop {
            let known = covered(self.feed.as_ref(), &self.held.borrow_and_update());
            if !known.is_empty() {
                break known;
            }
            changed(&mut self.held).await;
        };
        info!(source = self.feed.code(), count = known.len(), "quote feed: subscribing held set");

        let (add_tx, mut add_rx) = mpsc::channel::<SymbolAdds>(8);

        // The watcher never ends the session — only the feed decides that. Racing
        // them lets a new position be picked up without waiting for a reconnect.
        tokio::select! {
            r = self.feed.run_session(known.clone(), &self.out, &mut add_rx, &self.health) => r.map_err(Into::into),
            r = watch_held(self.feed.as_ref(), &mut self.held, &add_tx, known) => r,
        }
    }
}

/// On each change to the held set, push what this feed newly covers. Removals
/// are not sent: feeds cannot unsubscribe mid-session, and the next reconnect
/// builds from the current set.
async fn watch_held(
    feed: &dyn FeedSymbology,
    held: &mut watch::Receiver<BTreeMap<i64, InstrumentRef>>,
    add_tx: &mpsc::Sender<SymbolAdds>,
    mut known: HashMap<String, i64>,
) -> StreamResult {
    loop {
        changed(held).await;
        let latest = covered(feed, &held.borrow_and_update());
        let adds: SymbolAdds = latest.into_iter().filter(|(sym, _)| !known.contains_key(sym)).collect();
        if adds.is_empty() {
            continue;
        }
        info!(source = feed.code(), count = adds.len(), "quote feed: subscribing newly-held instruments");
        known.extend(adds.iter().map(|(s, i)| (s.clone(), *i)));
        if add_tx.send(adds).await.is_err() {
            // The feed dropped its receiver: the session is ending anyway. Park
            // rather than return, so the feed's own result is what the supervisor sees.
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use dataprovider::{
        DataProvider, FeedHealth, FeedSymbology, InstrumentFilter, InstrumentRef, LiveQuoteFeed, ProviderError, Quote,
        SymbolAdds,
    };
    use tokio::sync::{mpsc, Notify};

    use crate::md::MarketDataGateway;
    use crate::stream_health::{StreamHealthRegistry, StreamKind};
    use crate::stream_supervisor::Session;

    #[derive(Debug, PartialEq)]
    enum Seen {
        Session(HashMap<String, i64>),
        Adds(SymbolAdds),
    }

    /// Covers venue "X" and calls a symbol by its lowercase; reports what it was
    /// asked to subscribe, and ends a session when `end` is notified.
    struct Fake {
        seen: mpsc::UnboundedSender<Seen>,
        end: Arc<Notify>,
    }

    impl DataProvider for Fake {
        fn code(&self) -> &'static str {
            "FAKE"
        }
    }

    impl FeedSymbology for Fake {
        fn candidates(&self) -> InstrumentFilter {
            InstrumentFilter { venue: Some("X"), ..Default::default() }
        }
        fn to_feed_symbol(&self, symbol: &str) -> Option<String> {
            Some(symbol.to_lowercase())
        }
    }

    #[async_trait::async_trait]
    impl LiveQuoteFeed for Fake {
        async fn run_session(
            &self,
            symbols: HashMap<String, i64>,
            _out: &mpsc::Sender<Quote>,
            add_rx: &mut mpsc::Receiver<SymbolAdds>,
            _health: &dyn FeedHealth,
        ) -> Result<(), ProviderError> {
            let _ = self.seen.send(Seen::Session(symbols));
            loop {
                tokio::select! {
                    adds = add_rx.recv() => match adds {
                        Some(adds) => { let _ = self.seen.send(Seen::Adds(adds)); }
                        None => std::future::pending::<()>().await,
                    },
                    _ = self.end.notified() => return Ok(()),
                }
            }
        }
    }

    fn held(id: i64, symbol: &str, venue: &str) -> InstrumentRef {
        InstrumentRef { id, symbol: symbol.into(), instrument_class: None, asset_class: None, venue: Some(venue.into()) }
    }

    fn symbols(pairs: &[(&str, i64)]) -> HashMap<String, i64> {
        pairs.iter().map(|(s, i)| (s.to_string(), *i)).collect()
    }

    /// A gateway, a session of the fake feed on it, and what the fake sees.
    fn setup() -> (MarketDataGateway, crate::md::session::FeedSession, mpsc::UnboundedReceiver<Seen>, Arc<Notify>) {
        let health = StreamHealthRegistry::new();
        let (md, _quotes) = MarketDataGateway::new(health.clone());
        let (seen, rx) = mpsc::unbounded_channel();
        let end = Arc::new(Notify::new());
        let session = md.session(Arc::new(Fake { seen, end: end.clone() }), health.handle("FAKE", "SPOT", StreamKind::Feed));
        (md, session, rx, end)
    }

    async fn next(rx: &mut mpsc::UnboundedReceiver<Seen>) -> Seen {
        tokio::time::timeout(Duration::from_secs(1), rx.recv()).await.expect("the feed saw nothing").unwrap()
    }

    #[tokio::test]
    async fn a_feed_waits_until_something_it_covers_is_held() {
        let (md, mut session, mut rx, _end) = setup();
        md.handle().subscribe("held", vec![held(1, "AAA", "Y")]);
        tokio::spawn(async move { session.run_once().await });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(rx.try_recv().is_err(), "no session while nothing it covers is held");

        md.handle().subscribe("held", vec![held(2, "ABC", "X")]);
        assert_eq!(next(&mut rx).await, Seen::Session(symbols(&[("abc", 2)])));
    }

    #[tokio::test]
    async fn an_instrument_held_mid_session_is_added() {
        let (md, mut session, mut rx, _end) = setup();
        md.handle().subscribe("held", vec![held(2, "ABC", "X")]);
        tokio::spawn(async move { session.run_once().await });
        assert_eq!(next(&mut rx).await, Seen::Session(symbols(&[("abc", 2)])));

        md.handle().subscribe("held", vec![held(3, "DEF", "X")]);
        assert_eq!(next(&mut rx).await, Seen::Adds(symbols(&[("def", 3)])));
    }

    /// Another owner's interest keeps an instrument subscribed when the held
    /// set drops it: a strategy trading a pair nobody holds keeps its prices.
    #[tokio::test]
    async fn a_strategy_interest_survives_the_held_set_dropping_it() {
        let (md, mut session, mut rx, end) = setup();
        md.handle().subscribe("held", vec![held(2, "ABC", "X")]);
        md.handle().subscribe("strategy:s", vec![held(2, "ABC", "X")]);
        tokio::spawn(async move {
            let _ = session.run_once().await;
            let _ = session.run_once().await;
        });
        assert_eq!(next(&mut rx).await, Seen::Session(symbols(&[("abc", 2)])));

        md.handle().unsubscribe("held", &[2]);
        end.notify_one();
        assert_eq!(next(&mut rx).await, Seen::Session(symbols(&[("abc", 2)])));
    }

    #[tokio::test]
    async fn an_unsubscribed_instrument_is_not_resubscribed_on_reconnect() {
        let (md, mut session, mut rx, end) = setup();
        md.handle().subscribe("held", vec![held(2, "ABC", "X"), held(3, "DEF", "X")]);
        tokio::spawn(async move {
            let _ = session.run_once().await;
            let _ = session.run_once().await;
        });
        assert_eq!(next(&mut rx).await, Seen::Session(symbols(&[("abc", 2), ("def", 3)])));

        md.handle().unsubscribe("held", &[3]);
        end.notify_one();
        assert_eq!(next(&mut rx).await, Seen::Session(symbols(&[("abc", 2)])));
    }
}
