//! The market-data feeds this build ships, as a lookup by feed code.
//!
//! The live path does not need this: `gateway::md`'s `FeedSession` is generic over its feed,
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
/// Every quote feed's symbology: Databento's OPRA feed (a data vendor with its
/// own credential, not yet a connector) and each connector's public feed.
pub fn all() -> Vec<&'static dyn FeedSymbology> {
    // Symbology only — this never runs a session, so the key is a placeholder.
    static OPRA: crate::opra_stream::DatabentoOpraFeed = crate::opra_stream::DatabentoOpraFeed::new(String::new());
    std::iter::once(&OPRA as &'static dyn FeedSymbology)
        .chain(gateway::connectors::all().iter().filter_map(|c| c.public_feed()).map(|f| f.symbology))
        .collect()
}

pub fn by_code(code: &str) -> Option<&'static dyn FeedSymbology> {
    all().into_iter().find(|f| f.code() == code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_feeds_are_the_four_crypto_venues() {
        let mut codes: Vec<&str> =
            gateway::connectors::all().iter().filter(|c| c.public_feed().is_some()).map(|c| c.manifest().code).collect();
        codes.sort_unstable();
        assert_eq!(codes, ["BINANCE", "BYBIT", "COINBASE", "OKX"]);
        for code in ["DATABENTO", "BINANCE", "BYBIT", "COINBASE", "OKX"] {
            assert!(by_code(code).is_some(), "{code} still resolves");
        }
    }

    #[test]
    fn every_feed_is_reachable_by_its_own_code() {
        for feed in all() {
            let found = by_code(feed.code()).expect("registered feed resolves by code");
            assert_eq!(found.code(), feed.code());
        }
    }

    /// Codes must be distinct or `by_code` silently resolves to the wrong feed,
    /// which would subscribe one vendor's symbols on another's session.
    #[test]
    fn feed_codes_are_unique() {
        let mut codes: Vec<&str> = all().iter().map(|f| f.code()).collect();
        codes.sort_unstable();
        let before = codes.len();
        codes.dedup();
        assert_eq!(codes.len(), before, "duplicate feed code in all()");
    }

    #[test]
    fn unknown_code_resolves_to_none() {
        assert!(by_code("NOPE").is_none());
    }
}
