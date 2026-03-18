use crate::models::{OrderRequest, OrderType};
use reqwest::Client;
use serde::Serialize;
use tracing::info;

#[derive(Serialize)]
struct AlpacaOrderRequest {
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
}

impl AlpacaAdapter {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
        }
    }

    pub async fn submit_order(
        &self,
        order: &OrderRequest,
        api_key: &str,
        api_secret: &str,
        is_paper: bool,
    ) -> Result<String, String> {
        let base_url = if is_paper {
            "https://paper-api.alpaca.markets"
        } else {
            "https://api.alpaca.markets"
        };

        let url = format!("{}/v2/orders", base_url);

        // Map internal request to Alpaca format

        let (order_type_str, final_limit_price) = match order.order_type {
            OrderType::Market => ("market", None), // Force None for Market orders
            OrderType::Limit => ("limit", order.price.map(|p| p.to_string())),
        };

        let alpaca_order = AlpacaOrderRequest {
            symbol: order.symbol.clone(),
            qty: order.quantity.to_string(),
            side: format!("{:?}", order.side).to_lowercase(),
            order_type: order_type_str.to_string(),
            time_in_force: "day".to_string(),
            limit_price: final_limit_price, // Use the cleaned price
        };

        info!(
            "Submitting order to Alpaca {} KEY {} SECRET {}",
            url, api_key, api_secret
        );
        let response = self
            .client
            .post(&url)
            .header("APCA-API-KEY-ID", api_key)
            .header("APCA-API-SECRET-KEY", api_secret)
            .json(&alpaca_order)
            .send()
            .await
            .map_err(|e| format!("Network Error: {}", e))?;

        if response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            Ok(body)
        } else {
            let err_body = response.text().await.unwrap_or_default();
            Err(format!("Alpaca Error: {}", err_body))
        }
    }
}
