//! The market-data feeds this build ships, as a lookup by feed code.
//!
//! The live path does not need this: `QuoteFeedSession` is generic over its feed,
//! so it reaches [`dataprovider::FeedSymbology`] through the concrete type. This
//! registry exists for the callers that only hold a *code* — a `provider_feed_policy`
//! row, a preflight report — and must find the symbology behind it.
//!
//! Unit structs, so these are `'static` references to promoted constants: no
//! allocation, no state, nothing to initialise at boot.

use dataprovider::FeedSymbology;

/// Every feed with a symbology, whether or not it is currently spawned. Spawning is
/// conditional on credentials (see `serve`); coverage is not, so
/// [`crate::preflight`] can still report what a feed *would* price.
pub static ALL: &[&dyn FeedSymbology] = &[
    &crate::opra_stream::DatabentoOpraFeed,
    &crate::binance_feed::BinanceFeed,
    &crate::bybit_feed::BybitFeed,
];

/// The symbology for a feed code, or `None` if this build has no such feed —
/// which is the case for a `provider_feed_policy` row naming a feed that was
/// removed or never implemented.
pub fn by_code(code: &str) -> Option<&'static dyn FeedSymbology> {
    ALL.iter().copied().find(|f| f.code() == code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_feed_is_reachable_by_its_own_code() {
        for feed in ALL {
            let found = by_code(feed.code()).expect("registered feed resolves by code");
            assert_eq!(found.code(), feed.code());
        }
    }

    /// Codes must be distinct or `by_code` silently resolves to the wrong feed,
    /// which would subscribe one vendor's symbols on another's session.
    #[test]
    fn feed_codes_are_unique() {
        let mut codes: Vec<&str> = ALL.iter().map(|f| f.code()).collect();
        codes.sort_unstable();
        let before = codes.len();
        codes.dedup();
        assert_eq!(codes.len(), before, "duplicate feed code in ALL");
    }

    #[test]
    fn unknown_code_resolves_to_none() {
        assert!(by_code("NOPE").is_none());
    }
}
