/// IBKR Client Portal Gateway adapter — stub implementation.
///
/// The IBKR Client Portal Gateway (CPG) is a Java process run locally or on a server.
/// Key characteristics:
///
/// - Base URL: typically `https://localhost:5000/v1/api/` (self-signed TLS cert)
/// - Auth: session-based — POST /iserver/auth/status, POST /iserver/reauthenticate
///   Sessions expire and require periodic refresh via a background task.
/// - Order submission: POST /iserver/account/{acctId}/orders
///   Body: { "orders": [{ "conid": <int>, "secType": "STK", "orderType": "MKT|LMT",
///           "quantity": <f64>, "side": "BUY|SELL", "tif": "DAY|GTC", "price": <f64?> }] }
/// - conid is IBKR's internal contract ID — NOT a ticker symbol.
///   Requires a prior lookup: GET /iserver/secdef/search?symbol=AAPL
/// - Order cancellation: DELETE /iserver/account/{acctId}/order/{orderId}
///
/// TODO: implement session management (periodic re-auth), conid lookup, and real HTTP calls.
use super::{BrokerAdapter, BrokerError, BrokerOrderRequest, BrokerOrderResponse};

pub struct IbkrAdapter {
    pub base_url: String,
    pub environment: String,
}

impl IbkrAdapter {
    pub fn new(base_url: String, environment: &str) -> Self {
        Self {
            base_url,
            environment: environment.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl BrokerAdapter for IbkrAdapter {
    async fn submit_order(&self, _req: &BrokerOrderRequest) -> Result<BrokerOrderResponse, BrokerError> {
        // TODO: implement session auth, conid lookup, and order submission
        Err(BrokerError::NotConfigured(
            "IBKR adapter not yet implemented".to_string(),
        ))
    }

    async fn cancel_order(&self, _external_order_id: &str) -> Result<(), BrokerError> {
        // TODO: implement session auth and order cancellation
        Err(BrokerError::NotConfigured(
            "IBKR adapter not yet implemented".to_string(),
        ))
    }
}
