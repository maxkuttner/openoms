//! The universe-discovery capability.
//!
//! A [`UniverseSource`] is a [`DataProvider`] that can enumerate seedable
//! universes, price them (free metadata call), and fetch their instrument
//! definitions as neutral [`InstrumentDef`]s. The seeder drives it; adding a
//! new vendor is a new `impl UniverseSource` — the seeding framework is untouched.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::ProviderError;
use crate::instrument::InstrumentDef;
use crate::provider::DataProvider;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Category {
    Equity,
    Option,
    Future,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SType {
    RawSymbol,
    Parent,
}

impl SType {
    pub fn as_databento_str(self) -> &'static str {
        match self {
            SType::RawSymbol => "raw_symbol",
            SType::Parent => "parent",
        }
    }
}

/// A seedable slice of a provider's coverage.
///
/// `symbols` empty ⇒ whole dataset (the provider translates this to its native
/// "all symbols" sentinel — e.g. `ALL_SYMBOLS` on Databento).
#[derive(Debug, Clone)]
pub struct UniverseSpec {
    pub code: String,
    pub description: Option<String>,
    pub category: Category,
    pub dataset: String,
    pub option_dataset: Option<String>,
    pub symbols: Vec<String>,
    pub stype_in: SType,
    pub include_options: bool,
}

#[derive(Debug, Clone)]
pub struct CostEstimate {
    pub universe_code: String,
    pub usd: f64,
    pub symbol_count: Option<usize>,
}

#[async_trait]
pub trait UniverseSource: DataProvider {
    /// Universes this provider can seed. Impls may consult the DB catalog, a
    /// static list, or the vendor's own metadata endpoint.
    async fn discover(&self) -> Result<Vec<UniverseSpec>, ProviderError>;

    /// Free dry-run price for a spec (metadata call on Databento).
    async fn estimate_cost(&self, spec: &UniverseSpec) -> Result<CostEstimate, ProviderError>;

    /// The actual instrument records for a spec.
    async fn fetch_definitions(
        &self,
        spec: &UniverseSpec,
    ) -> Result<Vec<InstrumentDef>, ProviderError>;
}
