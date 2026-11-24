//! `dataprovider` — the extensible data-provider abstraction for instrument seeding.
//!
//! Composable capability traits, so a provider opts into exactly what it supports:
//!   - [`DataProvider`]    — base identity (`code()`).
//!   - [`LiveQuoteFeed`]   — stream normalized top-of-book [`Quote`]s.
//!   - [`FeedSymbology`]   — translate a feed's own symbols to/from the master catalog.
//!
//! Instruments are seeded broker-first (a `BrokerAdapter`'s `InstrumentProvider` in
//! the `rustoms` app), not from a vendor definition feed, so this crate no longer
//! discovers instruments — it supplies the shapes ([`InstrumentDef`]) that seeding
//! produces and the live quote/symbology traits the
//! feeds implement.
//!
//! Feeds live in the app (`opra_stream.rs`, `binance_feed.rs`, `bybit_feed.rs`) and
//! implement [`LiveQuoteFeed`] + [`FeedSymbology`] there, keeping each vendor's wire
//! format and naming rules together.

mod error;
mod instrument;
mod provider;
mod quote;

pub use error::ProviderError;
pub use instrument::{DerivativeDef, InstrumentDef, OptionKind};
pub use provider::DataProvider;
pub use quote::{
    covers, FeedHealth, FeedSymbology, InstrumentFilter, InstrumentRef, LiveQuoteFeed, NoFeedHealth, Quote,
    SymbolAdds,
};
