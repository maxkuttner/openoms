//! `dataprovider` — the extensible data-provider abstraction for instrument seeding.
//!
//! Composable capability traits, so a provider opts into exactly what it supports:
//!   - [`DataProvider`]    — base identity (`code()`).
//!   - [`Enricher`]        — fill the [`Identifiers`] bag from a metadata endpoint.
//!   - [`LiveQuoteFeed`]   — stream normalized top-of-book [`Quote`]s.
//!   - [`FeedSymbology`]   — translate a feed's own symbols to/from the master catalog.
//!
//! Instruments are seeded broker-first (a `BrokerAdapter`'s `InstrumentProvider` in
//! the `rustoms` app), not from a vendor definition feed, so this crate no longer
//! discovers instruments — it supplies the shapes ([`InstrumentDef`]) that seeding
//! produces, enrichment for identifiers, and the live quote/symbology traits the
//! feeds implement.
//!
//! Feeds live in the app (`opra_stream.rs`, `binance_feed.rs`, `bybit_feed.rs`) and
//! implement [`LiveQuoteFeed`] + [`FeedSymbology`] there, keeping each vendor's wire
//! format and naming rules together. Enricher: [`OpenFigiEnricher`] (FIGI).

mod enrich;
mod error;
mod instrument;
mod provider;
mod quote;

pub mod enrichers;

pub use enrich::{EnrichReport, Enricher};
pub use error::ProviderError;
pub use instrument::{DerivativeDef, Identifiers, InstrumentDef, OptionKind};
pub use provider::DataProvider;
pub use quote::{
    FeedHealth, FeedSymbology, InstrumentFilter, LiveQuoteFeed, NoFeedHealth, Quote, SymbolAdds,
};

pub use enrichers::openfigi::OpenFigiEnricher;

// Re-export the `symbology` surface the seeder needs so the app depends only on
// `dataprovider`.
pub use symbology::{
    openfigi_exch_code, Identifier, InMemoryCache, InstrumentQuery, OpenFigiClient, Resolution,
};
