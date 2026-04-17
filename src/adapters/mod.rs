pub mod alpaca;
pub mod ibkr;

use std::collections::HashMap;
use std::sync::Arc;

/// Broker-agnostic order request passed to any adapter.
pub struct BrokerOrderRequest {
    /// Ticker symbol (e.g. "AAPL"). Used directly as the broker symbol.
    /// TODO: replace with an instrument master lookup when added.
    pub symbol: String,
    pub quantity: f64,
    pub side: String,       // "buy" | "sell"
    pub order_type: String, // "market" | "limit"
    pub time_in_force: String,
    pub limit_price: Option<f64>,
    /// The broker's own account identifier (oms_account.external_account_ref).
    pub external_account_ref: String,
}

/// Successful response from a broker after order submission.
pub struct BrokerOrderResponse {
    /// The broker's identifier for the placed order — stored in the OrderRouted event.
    pub external_order_id: String,
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

/// Implemented by every broker adapter. Uses async-trait for dyn-compatibility.
#[async_trait::async_trait]
pub trait BrokerAdapter: Send + Sync {
    async fn submit_order(&self, req: &BrokerOrderRequest) -> Result<BrokerOrderResponse, BrokerError>;
    async fn cancel_order(&self, external_order_id: &str) -> Result<(), BrokerError>;
}

/// Registry keyed by (broker_code, environment) — e.g. ("ALPACA", "PAPER").
/// Built once at startup and shared read-only via Arc.
pub struct BrokerRegistry {
    adapters: HashMap<(String, String), Arc<dyn BrokerAdapter>>,
}

impl BrokerRegistry {
    pub fn new() -> Self {
        Self {
            adapters: HashMap::new(),
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

    pub fn get(&self, broker_code: &str, environment: &str) -> Option<Arc<dyn BrokerAdapter>> {
        self.adapters
            .get(&(broker_code.to_string(), environment.to_string()))
            .cloned()
    }
}
