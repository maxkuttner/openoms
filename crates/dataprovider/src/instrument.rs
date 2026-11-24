//! Provider-neutral instrument records, produced by a broker adapter's
//! `InstrumentProvider` during broker-first seeding, which the catalog upsert
//! persists.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OptionKind {
    Call,
    Put,
}

/// Provider-neutral instrument record. The seeder maps this into
/// `public.instrument` + (optionally) `public.instrument_derivative`.
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
}

#[derive(Debug, Clone)]
pub struct DerivativeDef {
    pub underlying_symbol: String,
    pub option_kind: Option<OptionKind>,
    pub strike_price: Option<f64>,
    pub expiry_date: Option<chrono::NaiveDate>,
    pub activation_date: Option<chrono::NaiveDate>,
}
