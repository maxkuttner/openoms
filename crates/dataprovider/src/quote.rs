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

/// Which instruments a feed is capable of pricing.
///
/// Declarative rather than a predicate so the seeding side can push it down into a
/// `WHERE` clause instead of scanning the whole catalog. Every `None` means "don't
/// constrain on this" — a feed that leaves `venue` open prices its symbol on every
/// venue that lists it, which is the 1:n case (one crypto pair, several exchanges).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstrumentFilter {
    pub instrument_class: Option<&'static str>,
    pub asset_class: Option<&'static str>,
    pub venue: Option<&'static str>,
}

impl InstrumentFilter {
    /// Append this filter to a `WHERE` clause already in progress, returning the
    /// values to bind in the order they must be bound.
    ///
    /// Placeholders are numbered from `first_placeholder`, so a caller that already
    /// binds parameters can append after them. Values are bound, never interpolated
    /// — the column names are the only thing written into the SQL, and those are
    /// `&'static str` chosen here rather than anything caller-supplied.
    ///
    /// Every condition is `AND`-ed, so the caller must supply a leading predicate
    /// (`WHERE true`, or a real one) for the SQL to be well-formed.
    pub fn push_conditions(&self, sql: &mut String, first_placeholder: usize) -> Vec<&'static str> {
        use std::fmt::Write;

        let mut binds = Vec::new();
        for (column, value) in [
            ("instrument_class", self.instrument_class),
            ("asset_class", self.asset_class),
            ("venue", self.venue),
        ] {
            if let Some(v) = value {
                binds.push(v);
                let _ = write!(sql, " AND {column} = ${}", first_placeholder + binds.len() - 1);
            }
        }
        binds
    }
}

/// How a feed names the instruments it prices.
///
/// The counterpart to [`LiveQuoteFeed`]: that trait moves a vendor's *data*, this one
/// translates its *symbology*. Both live on the same struct so a vendor's naming rules
/// sit next to the code that speaks its protocol, rather than in a central table of
/// every feed's quirks.
///
/// [`to_feed_symbol`](FeedSymbology::to_feed_symbol) is deliberately pure — no I/O, no
/// database — so the fiddly cases (OSI padding, suffixes, case) are unit-testable.
pub trait FeedSymbology: DataProvider {
    /// The subset of the master catalog this feed can price.
    fn candidates(&self) -> InstrumentFilter;

    /// Translate a master `instrument.symbol` into what this feed calls it.
    ///
    /// `None` means the feed cannot express that instrument; the caller skips it.
    /// Returning `None` is not an error — it is how a feed declines a symbol that
    /// passed [`candidates`](FeedSymbology::candidates) but is malformed for its
    /// symbology.
    fn to_feed_symbol(&self, symbol: &str) -> Option<String>;
}

/// A source of live quotes.
#[async_trait::async_trait]
pub trait LiveQuoteFeed: DataProvider {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_filter_adds_nothing() {
        let mut sql = String::from("WHERE true");
        let binds = InstrumentFilter::default().push_conditions(&mut sql, 1);
        assert_eq!(sql, "WHERE true");
        assert!(binds.is_empty());
    }

    /// Placeholders must count only the conditions actually emitted, so a filter
    /// that skips `asset_class` still numbers `venue` as `$2`, not `$3`.
    #[test]
    fn placeholders_are_contiguous_across_skipped_fields() {
        let f = InstrumentFilter {
            instrument_class: Some("OPTION"),
            asset_class: None,
            venue: Some("OPRA"),
        };
        let mut sql = String::from("WHERE true");
        let binds = f.push_conditions(&mut sql, 1);
        assert_eq!(sql, "WHERE true AND instrument_class = $1 AND venue = $2");
        assert_eq!(binds, vec!["OPTION", "OPRA"]);
    }

    /// A caller that already bound `$1` appends starting at `$2`; bind order must
    /// still match the returned vec.
    #[test]
    fn honours_a_placeholder_offset() {
        let f = InstrumentFilter { instrument_class: Some("SPOT"), asset_class: Some("CRYPTO"), venue: None };
        let mut sql = String::from("WHERE x = $1");
        let binds = f.push_conditions(&mut sql, 2);
        assert_eq!(sql, "WHERE x = $1 AND instrument_class = $2 AND asset_class = $3");
        assert_eq!(binds, vec!["SPOT", "CRYPTO"]);
    }
}
