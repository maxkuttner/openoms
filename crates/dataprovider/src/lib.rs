//! `dataprovider` — the extensible data-provider abstraction for instrument seeding.
//!
//! Composable capability traits, so a provider opts into exactly what it supports:
//!   - [`DataProvider`]   — base identity (`code()`).
//!   - [`UniverseSource`] — discover + price + fetch instrument definitions.
//!   - [`Enricher`]       — fill the [`Identifiers`] bag from a metadata endpoint.
//!
//! The seeder (in the `rustoms` app) drives a set of providers and a
//! `Vec<Box<dyn Enricher>>`. Adding a vendor = new `impl UniverseSource`; adding
//! an identifier/metadata source = new `impl Enricher`. The framework is untouched.
//!
//! First provider: [`DatabentoClient`] (definition schema for equities +
//! OPRA.PILLAR listed options). First enricher: [`OpenFigiEnricher`] (FIGI).

mod enrich;
mod error;
mod instrument;
mod provider;
mod universe;

pub mod enrichers;
pub mod providers;

pub use enrich::{EnrichReport, Enricher};
pub use error::ProviderError;
pub use instrument::{DerivativeDef, Identifiers, InstrumentDef, OptionKind};
pub use provider::DataProvider;
pub use universe::{Category, CostEstimate, SType, UniverseSource, UniverseSpec};

pub use enrichers::openfigi::OpenFigiEnricher;
pub use providers::databento::{DatabentoClient, DatabentoError};

// Re-export the `symbology` surface the seeder needs so the app depends only on
// `dataprovider`.
pub use symbology::{
    openfigi_exch_code, Identifier, InMemoryCache, InstrumentQuery, OpenFigiClient, Resolution,
};
