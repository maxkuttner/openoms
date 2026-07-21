//! The base provider identity trait.
//!
//! Every data source (quote feed, enricher, …) is first a [`DataProvider`] with a
//! stable `code()`. Capability traits such as [`crate::LiveQuoteFeed`] and
//! [`crate::FeedSymbology`] extend it, so a provider opts into exactly the
//! capabilities it supports without a single god-trait.

/// A named data provider. The `code()` is the feed's identity everywhere it is
/// referenced by name: `oms.provider_feed_policy` ranking, `Quote::source_code`,
/// the host's feed registry, and logs.
pub trait DataProvider: Send + Sync {
    /// Stable provider code, e.g. `"DATABENTO"`.
    fn code(&self) -> &'static str;
}
