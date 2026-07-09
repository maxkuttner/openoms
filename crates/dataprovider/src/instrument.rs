//! Provider-neutral instrument records produced by a [`crate::UniverseSource`]
//! and mutated in place by [`crate::Enricher`]s.

use serde::{Deserialize, Serialize};

/// Extensible bag of external identifiers for an instrument. Enrichers fill the
/// fields they own (OpenFIGI → `figi`/`composite_figi`); the seeder persists
/// whatever is set. Add a field here to support a new identifier type.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Identifiers {
    pub figi: Option<String>,
    pub composite_figi: Option<String>,
    pub isin: Option<String>,
    pub cusip: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OptionKind {
    Call,
    Put,
}

/// Provider-neutral instrument record. The seeder maps this into
/// `public.instrument` + (optionally) `public.instrument_derivative` +
/// `oms.instrument_xref` PROVIDER row.
#[derive(Debug, Clone)]
pub struct InstrumentDef {
    pub symbol: String,
    pub venue: String,
    pub currency: String,
    pub asset_class: String,
    pub instrument_class: String,
    pub name: Option<String>,
    pub price_precision: i32,
    pub price_increment: f64,
    pub size_increment: f64,
    pub lot_size: Option<f64>,
    pub contract_size: f64,
    pub native_id: Option<String>,
    pub provider_exchange: Option<String>,
    pub derivative: Option<DerivativeDef>,
    /// Enrichment output — filled by the enricher pipeline, empty at fetch time.
    pub identifiers: Identifiers,
}

#[derive(Debug, Clone)]
pub struct DerivativeDef {
    pub underlying_symbol: String,
    pub option_kind: Option<OptionKind>,
    pub strike_price: Option<f64>,
    pub expiry_date: Option<chrono::NaiveDate>,
    pub activation_date: Option<chrono::NaiveDate>,
}
