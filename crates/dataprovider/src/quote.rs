//! Live quote capability — the "quote source" this crate's [`crate::DataProvider`]
//! docs have always named but never had.
//!
//! A feed's only job is to turn a vendor's wire format into [`Quote`]s and emit
//! them. It does not decide what to subscribe to (the caller passes a symbol set),
//! does not touch the database, and does not know who consumes the quotes — that
//! keeps vendor detail contained and lets a feed be tested with nothing but a
//! channel. Databento's session-scoped numeric ids, Binance's lowercase pair
//! strings: neither concept escapes into this interface, because neither means
//! anything to the other vendor.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use tokio::sync::mpsc;

use crate::error::ProviderError;
use crate::provider::DataProvider;

/// A normalized top-of-book quote, already resolved to our instrument id.
#[derive(Debug, Clone, Copy)]
pub struct Quote {
    pub instrument_id: i64,
    pub bid: f64,
    pub ask: f64,
    pub bid_size: u32,
    pub ask_size: u32,
    /// When we received it. This is the staleness clock — deliberately our clock,
    /// not the venue's, so a vendor with a skewed or absent timestamp cannot make
    /// a stale quote look fresh.
    pub ts_recv: DateTime<Utc>,
    /// Which feed produced this. A mark whose origin is unknown is not auditable,
    /// and failover cannot arbitrate between sources it cannot name.
    pub source_code: &'static str,
}

impl Quote {
    pub fn mid(&self) -> f64 {
        (self.bid + self.ask) / 2.0
    }
}

/// New symbols to subscribe mid-session, as `external_symbol -> instrument_id`.
/// Carries the id because the feed must map its own wire identity back to ours.
pub type SymbolAdds = HashMap<String, i64>;

/// Liveness reporting for a feed session.
///
/// A port, so this crate stays ignorant of how the host tracks health. Feeds must
/// report both: `on_connected` is the only thing that can distinguish "subscribed
/// and waiting on a quiet book" from "never got up", and `on_event` is the
/// freshness clock a failover policy reads to decide a source has gone stale.
pub trait FeedHealth: Send + Sync {
    /// Subscribed and the venue accepted the session.
    fn on_connected(&self);
    /// A frame arrived. Called per record, including ones we discard.
    fn on_event(&self);
}

/// A [`FeedHealth`] that reports nowhere — for tests and for hosts that don't
/// track liveness.
pub struct NoFeedHealth;

impl FeedHealth for NoFeedHealth {
    fn on_connected(&self) {}
    fn on_event(&self) {}
}

/// A source of live quotes.
#[async_trait::async_trait]
pub trait LiveQuoteFeed: DataProvider {
    /// The `instrument_class` values this feed can quote.
    ///
    /// `code()` alone is not coverage: a vendor spans many datasets, and a feed is
    /// one of them. Databento cross-references both equities and options, but the
    /// OPRA.PILLAR session can only quote options — handing it an equity symbol
    /// would at best return nothing and at worst have the gateway reject the whole
    /// subscription. This is the minimum scope a driver needs to pick the right
    /// held instruments; a per-feed dataset/schema config is the fuller answer when
    /// one vendor runs several feeds.
    fn covers(&self) -> &'static [&'static str];
    /// Connect, subscribe `symbols`, and emit quotes until the session ends.
    ///
    /// `Ok(())` means the venue closed the stream cleanly; the supervisor
    /// reconnects either way. `add_rx` delivers symbols that became interesting
    /// after the session started (a new position opened) — each feed subscribes
    /// them however its protocol allows.
    async fn run_session(
        &self,
        symbols: HashMap<String, i64>,
        out: &mpsc::Sender<Quote>,
        add_rx: &mut mpsc::Receiver<SymbolAdds>,
        health: &dyn FeedHealth,
    ) -> Result<(), ProviderError>;
}
