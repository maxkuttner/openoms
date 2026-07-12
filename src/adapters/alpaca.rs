use reqwest::Client;
use serde::Serialize;
use tracing::info;

use super::{BrokerAdapter, BrokerError, BrokerHolding, BrokerInstrument, BrokerOrderRequest, BrokerOrderResponse};

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

    pub async fn get_order(&self, external_order_id: &str) -> Result<serde_json::Value, BrokerError> {
        let url = format!("{}/v2/orders/{}", self.base_url, external_order_id);
        let resp = self
            .client
            .get(&url)
            .header("APCA-API-KEY-ID", &self.api_key)
            .header("APCA-API-SECRET-KEY", &self.api_secret)
            .send()
            .await
            .map_err(|e| BrokerError::Network(e.to_string()))?;
        if resp.status().is_success() {
            resp.json().await.map_err(|e| BrokerError::Network(e.to_string()))
        } else {
            Err(BrokerError::BrokerRejected(resp.text().await.unwrap_or_default()))
        }
    }

    fn get_json(&self, url: &str) -> reqwest::RequestBuilder {
        self.client
            .get(url)
            .header("APCA-API-KEY-ID", &self.api_key)
            .header("APCA-API-SECRET-KEY", &self.api_secret)
    }

    /// Active, tradeable US-equity assets from Alpaca's catalog
    /// (`GET /v2/assets`). Broker symbology, not market data.
    pub async fn list_equity_instruments(&self) -> Result<Vec<BrokerInstrument>, BrokerError> {
        let url = format!("{}/v2/assets?status=active&asset_class=us_equity", self.base_url);
        let resp = self.get_json(&url).send().await.map_err(|e| BrokerError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(BrokerError::BrokerRejected(resp.text().await.unwrap_or_default()));
        }
        let assets: Vec<serde_json::Value> =
            resp.json().await.map_err(|e| BrokerError::Network(e.to_string()))?;
        Ok(assets
            .iter()
            .filter_map(|a| {
                Some(BrokerInstrument {
                    symbol: a["symbol"].as_str()?.to_string(),
                    exchange: a["exchange"].as_str().map(|s| s.to_string()),
                    native_id: a["id"].as_str().map(|s| s.to_string()), // Alpaca asset UUID
                    is_tradeable: a["tradable"].as_bool().unwrap_or(false),
                    min_quantity: a["min_order_size"].as_str().and_then(|s| s.parse().ok()),
                })
            })
            .collect())
    }

    /// Active option contracts for the given underlyings, following pagination
    /// (`GET /v2/options/contracts`). `symbol` is the compact OSI Alpaca's order
    /// API expects; options route by symbol so `native_id` is left None.
    pub async fn list_option_contracts(
        &self,
        underlyings: &[String],
    ) -> Result<Vec<BrokerInstrument>, BrokerError> {
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut url = format!(
                "{}/v2/options/contracts?status=active&limit=10000&underlying_symbols={}",
                self.base_url,
                underlyings.join(",")
            );
            if let Some(tok) = &page_token {
                url.push_str("&page_token=");
                url.push_str(tok);
            }
            let resp = self.get_json(&url).send().await.map_err(|e| BrokerError::Network(e.to_string()))?;
            if !resp.status().is_success() {
                return Err(BrokerError::BrokerRejected(resp.text().await.unwrap_or_default()));
            }
            let body: serde_json::Value =
                resp.json().await.map_err(|e| BrokerError::Network(e.to_string()))?;
            for c in body["option_contracts"].as_array().unwrap_or(&Vec::new()) {
                let Some(symbol) = c["symbol"].as_str() else { continue };
                out.push(BrokerInstrument {
                    symbol: symbol.to_string(),
                    exchange: Some("OPRA".to_string()),
                    native_id: None,
                    is_tradeable: c["tradable"].as_bool().unwrap_or(false),
                    min_quantity: Some(1.0), // options trade in whole contracts
                });
            }
            page_token = body["next_page_token"].as_str().map(|s| s.to_string());
            if page_token.is_none() {
                break;
            }
        }
        Ok(out)
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

        // Alpaca's `symbol` field accepts the ticker OR the asset_id; prefer the
        // immutable native_id (asset UUID) when present, falling back to the ticker.
        let routing_symbol = req.native_id.clone().unwrap_or_else(|| req.symbol.clone());

        let alpaca_order = AlpacaOrderRequest {
            client_order_id: req.order_id.clone(),
            symbol: routing_symbol,
            qty: req.quantity.to_string(),
            side: req.side.clone(),
            order_type: order_type_str.to_string(),
            time_in_force: req.time_in_force.clone(),
            limit_price: final_limit_price,
        };

        info!(url = %url, symbol = %req.symbol, by_native_id = req.native_id.is_some(), "submitting order to Alpaca");

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

    // Alpaca cancels by order id alone; the symbol is unused.
    async fn cancel_order(&self, external_order_id: &str, _symbol: &str) -> Result<(), BrokerError> {
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

    /// Custodian-side holdings for reconciliation: GET /v2/positions.
    async fn get_positions(&self) -> Result<Vec<BrokerHolding>, BrokerError> {
        let url = format!("{}/v2/positions", self.base_url);
        let resp = self
            .client
            .get(&url)
            .header("APCA-API-KEY-ID", &self.api_key)
            .header("APCA-API-SECRET-KEY", &self.api_secret)
            .send()
            .await
            .map_err(|e| BrokerError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(BrokerError::BrokerRejected(resp.text().await.unwrap_or_default()));
        }
        let rows: Vec<serde_json::Value> =
            resp.json().await.map_err(|e| BrokerError::Network(e.to_string()))?;
        // Alpaca returns qty as a signed string ("-5" for a short).
        let holdings = rows
            .iter()
            .map(|p| BrokerHolding {
                symbol: p["symbol"].as_str().unwrap_or_default().to_string(),
                native_id: p["asset_id"].as_str().map(|s| s.to_string()),
                qty: p["qty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
            })
            .collect();
        Ok(holdings)
    }
}
