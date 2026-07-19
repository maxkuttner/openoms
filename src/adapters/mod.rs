pub mod alpaca;
pub mod binance;
pub mod ibkr;

use std::collections::HashMap;
use std::sync::Arc;

use dataprovider::InstrumentDef;

use crate::adapters::alpaca::AlpacaAdapter;

/// Broker-agnostic order request passed to any adapter.
pub struct BrokerOrderRequest {
    /// Our internal order UUID — sent as client_order_id so brokers echo it back on updates.
    pub order_id: String,
    /// Broker-specific symbol resolved from broker_instrument (e.g. "AAPL" for Alpaca).
    pub symbol: String,
    /// Broker-native instrument id from broker_instrument (Alpaca asset UUID, IBKR conId).
    /// Preferred over `symbol` for routing when present — immutable across ticker renames.
    pub native_id: Option<String>,
    pub quantity: f64,
    pub side: String,       // "buy" | "sell"
    pub order_type: String, // "market" | "limit"
    pub time_in_force: String,
    pub limit_price: Option<f64>,
    /// The broker's own account identifier (account.external_account_ref).
    pub external_account_ref: String,
}

/// Successful response from a broker after order submission.
pub struct BrokerOrderResponse {
    /// The broker's identifier for the placed order — stored in the OrderRouted event.
    pub external_order_id: String,
}

/// A position as reported by a broker/custodian — the custodian side of reconciliation.
pub struct BrokerHolding {
    /// Broker symbol (e.g. "SPY").
    pub symbol: String,
    /// Broker-native instrument id (Alpaca asset UUID), when present.
    pub native_id: Option<String>,
    /// Signed quantity: + long, - short.
    pub qty: f64,
}

/// One entry of a broker's tradeable catalog: the canonical Symbol@Venue
/// instrument record (→ `public.instrument` / `instrument_derivative`) plus the
/// broker's own routing handle (→ `public.broker_instrument`). Returned by an
/// adapter's [`InstrumentProvider`] impl; broker sync creates both rows from it in
/// one pass, so the broker is the authoritative source of the instrument.
///
/// `definition.native_id` carries the broker's immutable id when it exposes one
/// (Alpaca asset UUID for equities; None for options, which route by symbol).
pub struct BrokerInstrument {
    /// Canonical instrument definition — master catalog fields.
    pub definition: InstrumentDef,
    /// The broker's order-entry handle. Usually equals `definition.symbol`; kept
    /// separate for brokers whose routing symbol differs from the canonical symbol.
    pub broker_symbol: String,
    /// The broker's own exchange label for the routing handle, when it exposes one.
    pub broker_exchange: Option<String>,
    pub is_tradeable: bool,
    pub min_quantity: Option<f64>,
    pub max_quantity: Option<f64>,
    pub min_notional: Option<f64>,
    pub max_notional: Option<f64>,
}

/// A broker/exchange adapter that can enumerate its own tradeable catalog. Broker
/// sync (`oms setup sync-broker`) drives this to seed the master instrument catalog
/// broker-first. Adapters without a catalog endpoint (e.g. the IBKR stub) simply
/// don't implement it.
#[async_trait::async_trait]
pub trait InstrumentProvider: Send + Sync {
    /// The broker's tradeable catalog as canonical instrument records + routing
    /// handles. `option_underlyings` scopes the (very large) option chain; empty =
    /// list equities / spot pairs only.
    async fn list_instruments(
        &self,
        option_underlyings: &[String],
    ) -> Result<Vec<BrokerInstrument>, BrokerError>;
}

#[derive(Debug)]
pub enum BrokerError {
    Network(String),
    BrokerRejected(String),
    NotConfigured(String),
}

impl std::fmt::Display for BrokerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BrokerError::Network(msg) => write!(f, "network error: {msg}"),
            BrokerError::BrokerRejected(msg) => write!(f, "broker rejected: {msg}"),
            BrokerError::NotConfigured(msg) => write!(f, "not configured: {msg}"),
        }
    }
}

impl std::error::Error for BrokerError {}

/// Implemented by every broker adapter. Uses async-trait for dyn-compatibility.
#[async_trait::async_trait]
pub trait BrokerAdapter: Send + Sync {
    async fn submit_order(&self, req: &BrokerOrderRequest) -> Result<BrokerOrderResponse, BrokerError>;
    /// `symbol` is the broker-native symbol for the order — required by venues
    /// (e.g. Binance) that scope cancels by symbol; ignored by those that don't
    /// (e.g. Alpaca).
    async fn cancel_order(&self, external_order_id: &str, symbol: &str) -> Result<(), BrokerError>;
    /// Custodian-side holdings for reconciliation. Adapters that don't support it
    /// return `NotConfigured` (the default) and are skipped by recon.
    async fn get_positions(&self) -> Result<Vec<BrokerHolding>, BrokerError> {
        Err(BrokerError::NotConfigured(
            "positions not supported by this adapter".to_string(),
        ))
    }
}

/// Registry keyed by (broker_code, environment) — e.g. ("ALPACA", "PAPER").
/// Built once at startup and shared read-only via Arc.
pub struct BrokerRegistry {
    adapters: HashMap<(String, String), Arc<dyn BrokerAdapter>>,
    alpaca_adapters: HashMap<String, Arc<AlpacaAdapter>>,
}

impl BrokerRegistry {
    pub fn new() -> Self {
        Self {
            adapters: HashMap::new(),
            alpaca_adapters: HashMap::new(),
        }
    }

    pub fn register(
        &mut self,
        broker_code: &str,
        environment: &str,
        adapter: Arc<dyn BrokerAdapter>,
    ) {
        self.adapters
            .insert((broker_code.to_string(), environment.to_string()), adapter);
    }

    pub fn register_alpaca(&mut self, environment: &str, adapter: Arc<AlpacaAdapter>) {
        self.alpaca_adapters.insert(environment.to_string(), adapter.clone());
        self.adapters.insert(("ALPACA".to_string(), environment.to_string()), adapter);
    }

    pub fn get(&self, broker_code: &str, environment: &str) -> Option<Arc<dyn BrokerAdapter>> {
        self.adapters
            .get(&(broker_code.to_string(), environment.to_string()))
            .cloned()
    }

    pub fn get_alpaca(&self, environment: &str) -> Option<Arc<AlpacaAdapter>> {
        self.alpaca_adapters.get(environment).cloned()
    }
}
