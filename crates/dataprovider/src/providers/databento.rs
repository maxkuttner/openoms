//! Databento implementation of [`UniverseSource`].
//!
//! Uses the official `databento` crate: fetches the `definition` schema for
//! equity, option (OPRA.PILLAR), and future universes, estimates cost via
//! `metadata.get_cost` (free), and returns neutral [`InstrumentDef`] rows for the
//! seeder to upsert.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Datelike;
use databento::{
    dbn::{self, InstrumentClass, InstrumentDefMsg, Schema, UNDEF_TIMESTAMP},
    historical::{
        metadata::GetCostParams, timeseries::GetRangeParams, Client as HistoricalClient, DateRange,
        DateTimeRange,
    },
    Symbols,
};
use thiserror::Error;
use tokio::sync::Mutex;

use crate::error::ProviderError;
use crate::instrument::{DerivativeDef, Identifiers, InstrumentDef, OptionKind};
use crate::provider::DataProvider;
use crate::universe::{Category, CostEstimate, SType, UniverseSource, UniverseSpec};

#[derive(Debug, Error)]
pub enum DatabentoError {
    #[error("DATABENTO_API_KEY not set")]
    MissingKey,
    #[error("databento sdk error: {0}")]
    Sdk(String),
}

impl From<DatabentoError> for ProviderError {
    fn from(e: DatabentoError) -> Self {
        ProviderError::Request(e.to_string())
    }
}

impl From<databento::Error> for DatabentoError {
    fn from(e: databento::Error) -> Self {
        DatabentoError::Sdk(e.to_string())
    }
}

/// Window (in days) of the definition schema we snapshot before the dataset's
/// end date. Definitions are near-static so a small window is enough to catch
/// the latest listing state.
const DEFAULT_WINDOW_DAYS: i64 = 3;

pub struct DatabentoClient {
    api_key: String,
    /// Universes this client can seed. Loaded from `public.instrument_universe`
    /// by the caller and injected before `discover()` is called.
    catalog: Arc<Mutex<Vec<UniverseSpec>>>,
}

impl DatabentoClient {
    pub fn from_env() -> Result<Self, DatabentoError> {
        let api_key = std::env::var("DATABENTO_API_KEY").map_err(|_| DatabentoError::MissingKey)?;
        Ok(Self {
            api_key,
            catalog: Arc::new(Mutex::new(Vec::new())),
        })
    }

    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            catalog: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Seed the provider's catalog (typically from the DB).
    pub async fn set_catalog(&self, catalog: Vec<UniverseSpec>) {
        *self.catalog.lock().await = catalog;
    }

    fn build_client(&self) -> Result<HistoricalClient, DatabentoError> {
        Ok(HistoricalClient::builder().key(&self.api_key)?.build()?)
    }

    /// Compute the (start, end) date window for a dataset — a small window
    /// ending at its last available date.
    async fn window_for(&self, dataset: &str) -> Result<DateRange, DatabentoError> {
        let mut client = self.build_client()?;
        let range = client.metadata().get_dataset_range(dataset).await?;
        let end = range.end.date();
        let start = end.saturating_sub(time::Duration::days(DEFAULT_WINDOW_DAYS));
        Ok(DateRange::from((start, end)))
    }

    async fn dt_window_for(&self, dataset: &str) -> Result<DateTimeRange, DatabentoError> {
        let dr = self.window_for(dataset).await?;
        Ok(dr.into())
    }

    /// Free cost for a single leg (dataset + symbols + stype).
    async fn cost_leg(
        &self,
        dataset: &str,
        symbols: Symbols,
        stype: dbn::SType,
    ) -> Result<f64, ProviderError> {
        let mut client = self.build_client().map_err(ProviderError::from)?;
        let dt_range = self.dt_window_for(dataset).await.map_err(ProviderError::from)?;
        let params = GetCostParams::builder()
            .dataset(dataset)
            .symbols(symbols)
            .schema(Schema::Definition)
            .date_time_range(dt_range)
            .stype_in(stype)
            .build();
        match client.metadata().get_cost(&params).await {
            Ok(v) => Ok(v),
            Err(e) if is_no_data_err(&e) => Ok(0.0),
            Err(e) => Err(ProviderError::Request(e.to_string())),
        }
    }

    /// Fetch + map one definition leg into neutral rows.
    async fn fetch_leg(
        &self,
        dataset: &str,
        symbols: Symbols,
        stype: dbn::SType,
        out: &mut Vec<InstrumentDef>,
    ) -> Result<(), ProviderError> {
        let mut client = self.build_client().map_err(ProviderError::from)?;
        let dt_range = self.dt_window_for(dataset).await.map_err(ProviderError::from)?;
        let params = GetRangeParams::builder()
            .dataset(dataset)
            .symbols(symbols)
            .schema(Schema::Definition)
            .date_time_range(dt_range)
            .stype_in(stype)
            .build();
        match client.timeseries().get_range(&params).await {
            Ok(mut dec) => {
                while let Some(rec) = dec
                    .decode_record::<InstrumentDefMsg>()
                    .await
                    .map_err(|e| ProviderError::Request(e.to_string()))?
                {
                    if let Some(mapped) = map_def(rec) {
                        out.push(mapped);
                    }
                }
                Ok(())
            }
            Err(e) if is_no_data_err(&e) => Ok(()),
            Err(e) => Err(ProviderError::Request(e.to_string())),
        }
    }
}

fn dbn_stype(s: SType) -> dbn::SType {
    match s {
        SType::RawSymbol => dbn::SType::RawSymbol,
        SType::Parent => dbn::SType::Parent,
    }
}

fn dbn_symbols(spec: &UniverseSpec) -> Symbols {
    if spec.symbols.is_empty() {
        Symbols::All
    } else if matches!(spec.stype_in, SType::Parent) {
        // when the stype is parent, we need to add the .OPT suffix to the symbols
        Symbols::Symbols(spec.symbols.iter().map(|s| format!("{s}.OPT")).collect())
    } else {
        // else just raw symbols
        Symbols::Symbols(spec.symbols.clone())
    }
}

/// Symbols for the parent/options leg (`.OPT` suffix).
fn opt_symbols(spec: &UniverseSpec) -> Symbols {
    // Return the symbols for the parent/underlying
    // in databento the corresponding symbols are suffixed with `.OPT`
    // Note: Most of the time it does not make sense to fetch options data for all available
    // symbols -> so use with care.
    if spec.symbols.is_empty() {
        Symbols::All
    } else {
        Symbols::Symbols(spec.symbols.iter().map(|s| format!("{s}.OPT")).collect())
    }
}

fn is_no_data_err(e: &databento::Error) -> bool {
    let msg = e.to_string();
    msg.contains("data_no_data_found_for_request") || msg.contains("no_data")
}

impl DataProvider for DatabentoClient {
    fn code(&self) -> &'static str {
        "DATABENTO"
    }
}

#[async_trait]
impl UniverseSource for DatabentoClient {
    async fn discover(&self) -> Result<Vec<UniverseSpec>, ProviderError> {
        // Return the catalog of universes as a vector of UniverseSpec
        Ok(self.catalog.lock().await.clone())
    }

    async fn estimate_cost(&self, spec: &UniverseSpec) -> Result<CostEstimate, ProviderError> {
        // Calculate the cost estimate for fetching the universe from Databento
        let mut total = match spec.category {
            // Options
            Category::Option => {
                self.cost_leg(&spec.dataset, opt_symbols(spec), dbn::SType::Parent)
                    .await?
            }
            // Equity
            _ => {
                self.cost_leg(&spec.dataset, dbn_symbols(spec), dbn_stype(spec.stype_in))
                    .await?
            }
        };

        // Optional: options leg attached to an equity universe.
        // This needed if you have a custom universe like "SPY and its option chain"
        if !matches!(spec.category, Category::Option)
            && spec.include_options
            && !spec.symbols.is_empty()
        {
            if let Some(od) = &spec.option_dataset {
                total += self
                    .cost_leg(od, opt_symbols(spec), dbn::SType::Parent)
                    .await?;
            }
        }

        Ok(CostEstimate {
            universe_code: spec.code.clone(),
            usd: total,
            symbol_count: if spec.symbols.is_empty() {
                None
            } else {
                Some(spec.symbols.len())
            },
        })
    }

    async fn fetch_definitions(
        &self,
        spec: &UniverseSpec,
    ) -> Result<Vec<InstrumentDef>, ProviderError> {
        let mut out: Vec<InstrumentDef> = Vec::new();

        match spec.category {
            // OPRA.PILLAR listed-option universe: one parent leg on `spec.dataset`.
            Category::Option => {
                self.fetch_leg(&spec.dataset, opt_symbols(spec), dbn::SType::Parent, &mut out)
                    .await?;
            }
            // Equity / future: the primary definition leg.
            _ => {
                self.fetch_leg(
                    &spec.dataset,
                    dbn_symbols(spec),
                    dbn_stype(spec.stype_in),
                    &mut out,
                )
                .await?;

                // Optional options leg (definition schema on the OPRA/parent dataset).
                if spec.include_options && !spec.symbols.is_empty() {
                    if let Some(od) = &spec.option_dataset {
                        self.fetch_leg(od, opt_symbols(spec), dbn::SType::Parent, &mut out)
                            .await?;
                    }
                }
            }
        }

        // Latest record per (symbol, venue) wins — definitions may repeat.
        Ok(dedupe_latest(out))
    }
}

fn dedupe_latest(defs: Vec<InstrumentDef>) -> Vec<InstrumentDef> {
    use std::collections::HashMap;
    let mut latest: HashMap<(String, String), InstrumentDef> = HashMap::new();
    for d in defs {
        latest.insert((d.symbol.clone(), d.venue.clone()), d);
    }
    latest.into_values().collect()
}

/// Databento fixed-point (1e-9) → f64 with heuristic pretty-vs-raw detection.
fn from_fixed(v: i64) -> Option<f64> {
    if v == i64::MIN || v == i64::MAX {
        return None;
    }
    let f = v as f64;
    if f.abs() >= 1e6 {
        Some(f / 1e9)
    } else {
        Some(f)
    }
}

fn decimals_of(tick: f64) -> i32 {
    if tick <= 0.0 {
        return 2;
    }
    let (mut d, mut x) = (0i32, tick);
    while (x - x.round()).abs() > 1e-12 && d < 12 {
        x *= 10.0;
        d += 1;
    }
    d
}

fn ns_to_date(ns: u64) -> Option<chrono::NaiveDate> {
    if ns == UNDEF_TIMESTAMP || ns == 0 {
        return None;
    }
    let secs = (ns / 1_000_000_000) as i64;
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
        .map(|dt| dt.date_naive())
        .filter(|d| d.year() <= 2100)
}

fn map_def(rec: &InstrumentDefMsg) -> Option<InstrumentDef> {
    let iclass = rec.instrument_class().ok()?;
    let (instrument_class, opt_kind): (&str, Option<OptionKind>) = match iclass {
        InstrumentClass::Stock => ("SPOT", None),
        InstrumentClass::Call => ("OPTION", Some(OptionKind::Call)),
        InstrumentClass::Put => ("OPTION", Some(OptionKind::Put)),
        InstrumentClass::Future => ("FUTURE", None),
        _ => return None,
    };

    let symbol = rec.raw_symbol().ok()?.to_string();
    let exchange = rec.exchange().ok()?.to_string();
    if symbol.is_empty() || exchange.is_empty() {
        return None;
    }
    let venue = exchange.clone();

    let currency = rec.currency().ok().map(|s| s.to_string()).unwrap_or_else(|| "USD".into());
    let currency = currency.trim().to_uppercase();

    let tick = from_fixed(rec.min_price_increment).filter(|v| *v > 0.0).unwrap_or(0.01);
    // `contract_multiplier` is UNDEF (`i32::MAX`) for OPRA options and 0 when absent.
    let contract = if rec.contract_multiplier != 0 && rec.contract_multiplier != i32::MAX {
        rec.contract_multiplier as f64
    } else {
        from_fixed(rec.unit_of_measure_qty)
            .filter(|v| *v > 0.0)
            // Listed US options standardize on a 100-share multiplier.
            .unwrap_or(if instrument_class == "OPTION" { 100.0 } else { 1.0 })
    };
    let lot = if rec.min_lot_size_round_lot > 0 {
        Some(rec.min_lot_size_round_lot as f64)
    } else {
        None
    };

    let derivative = if instrument_class == "OPTION" {
        let underlying = rec.underlying().ok().unwrap_or("").trim().to_string();
        if underlying.is_empty() {
            None
        } else {
            Some(DerivativeDef {
                underlying_symbol: underlying,
                option_kind: opt_kind,
                strike_price: from_fixed(rec.strike_price),
                expiry_date: ns_to_date(rec.expiration),
                activation_date: ns_to_date(rec.activation),
            })
        }
    } else {
        None
    };

    // Options require a derivative row to be seeded.
    if instrument_class == "OPTION" && derivative.is_none() {
        return None;
    }

    let native_id = if rec.hd.instrument_id != 0 {
        Some(rec.hd.instrument_id.to_string())
    } else {
        None
    };

    Some(InstrumentDef {
        symbol: symbol.clone(),
        venue,
        currency,
        asset_class: "EQUITY".into(),
        instrument_class: instrument_class.into(),
        name: Some(symbol),
        price_precision: decimals_of(tick),
        price_increment: tick,
        size_increment: lot.unwrap_or(1.0),
        lot_size: lot,
        contract_size: if contract > 0.0 { contract } else { 1.0 },
        native_id,
        provider_exchange: Some(exchange),
        derivative,
        identifiers: Identifiers::default(),
    })
}
