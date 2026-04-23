use reqwest::Client;
use serde::Serialize;
use tracing::info;

use super::{BrokerAdapter, BrokerError, BrokerOrderRequest, BrokerOrderResponse};

#[derive(Serialize)]
struct AlpacaOrderRequest {
    client_order_id: String,
    symbol: String,
    qty: String, // Alpaca expects strings for numeric precision
    side: String,
    #[serde(rename = "type")]
    order_type: String,
    time_in_force: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit_price: Option<String>,
}

#[derive(Clone)]
pub struct AlpacaAdapter {
    client: Client,
    api_key: String,
    api_secret: String,
    base_url: &'static str,
}

impl AlpacaAdapter {
    pub fn new(api_key: String, api_secret: String, environment: &str) -> Self {
        let base_url = if environment == "LIVE" {
            "https://api.alpaca.markets"
        } else {
            "https://paper-api.alpaca.markets"
        };
        Self {
            client: Client::new(),
            api_key,
            api_secret,
            base_url,
        }
    }
}

#[async_trait::async_trait]
impl BrokerAdapter for AlpacaAdapter {
    async fn submit_order(&self, req: &BrokerOrderRequest) -> Result<BrokerOrderResponse, BrokerError> {
        let url = format!("{}/v2/orders", self.base_url);

        let (order_type_str, final_limit_price) = match req.order_type.as_str() {
            "market" => ("market", None),
            "limit" => ("limit", req.limit_price.map(|p| p.to_string())),
            other => return Err(BrokerError::NotConfigured(format!("unsupported order type: {other}"))),
        };

        let alpaca_order = AlpacaOrderRequest {
            client_order_id: req.order_id.clone(),
            symbol: req.symbol.clone(),
            qty: req.quantity.to_string(),
            side: req.side.clone(),
            order_type: order_type_str.to_string(),
            time_in_force: req.time_in_force.clone(),
            limit_price: final_limit_price,
        };

        info!(url = %url, symbol = %req.symbol, "submitting order to Alpaca");

        let response = self
            .client
            .post(&url)
            .header("APCA-API-KEY-ID", &self.api_key)
            .header("APCA-API-SECRET-KEY", &self.api_secret)
            .json(&alpaca_order)
            .send()
            .await
            .map_err(|e| BrokerError::Network(e.to_string()))?;

        if response.status().is_success() {
            let body: serde_json::Value = response
                .json()
                .await
                .map_err(|e| BrokerError::Network(e.to_string()))?;
            let external_order_id = body["id"]
                .as_str()
                .unwrap_or("")
                .to_string();
            Ok(BrokerOrderResponse { external_order_id })
        } else {
            let err_body = response.text().await.unwrap_or_default();
            Err(BrokerError::BrokerRejected(err_body))
        }
    }

    async fn cancel_order(&self, external_order_id: &str) -> Result<(), BrokerError> {
        let url = format!("{}/v2/orders/{}", self.base_url, external_order_id);

        let response = self
            .client
            .delete(&url)
            .header("APCA-API-KEY-ID", &self.api_key)
            .header("APCA-API-SECRET-KEY", &self.api_secret)
            .send()
            .await
            .map_err(|e| BrokerError::Network(e.to_string()))?;

        if response.status().is_success() || response.status().as_u16() == 204 {
            Ok(())
        } else {
            let err_body = response.text().await.unwrap_or_default();
            Err(BrokerError::BrokerRejected(err_body))
        }
    }
}
