use std::collections::BTreeSet;

use dataprovider::{DerivativeDef, Identifiers, InstrumentDef, OptionKind};
use reqwest::Client;
use serde::Serialize;
use tracing::{info, warn};

use super::{
    BrokerAdapter, BrokerError, BrokerHolding, BrokerInstrument, BrokerOrderRequest,
    BrokerOrderResponse, InstrumentProvider,
};

/// Map Alpaca's `exchange` label to the ISO 10383 MIC used by `public.venue`.
///
/// `None` means we have no MIC for that label. The caller drops the instrument and
/// names the label, rather than passing the raw string through as a venue code — a
/// passthrough only fails later, anonymously, inside the catalog's FK filter.
fn alpaca_exchange_to_mic(exchange: &str) -> Option<&'static str> {
    Some(match exchange {
        "NASDAQ" => "XNAS",
        "NYSE" => "XNYS",
        "ARCA" | "NYSEARCA" => "ARCX",
        "AMEX" => "XASE",
        "BATS" => "BATS",
        "IEX" => "IEXG",
        "OTC" => "OTCM",
        _ => return None,
    })
}

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

    /// Active US-equity assets from Alpaca's catalog (`GET /v2/assets`) as canonical
    /// instrument records + routing handles. Alpaca is the source of the master
    /// instrument here: the ticker is the Symbol@Venue symbol, the exchange maps to
    /// the venue MIC, and the asset UUID is the routing native id.
    pub async fn list_equity_instruments(&self) -> Result<Vec<BrokerInstrument>, BrokerError> {
        let url = format!("{}/v2/assets?status=active&asset_class=us_equity", self.base_url);
        let resp = self.get_json(&url).send().await.map_err(|e| BrokerError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(BrokerError::BrokerRejected(resp.text().await.unwrap_or_default()));
        }
        let assets: Vec<serde_json::Value> =
            resp.json().await.map_err(|e| BrokerError::Network(e.to_string()))?;

        // Exchange labels we have no MIC for. Collected as distinct values so an
        // unmapped venue is reported by name once, not buried in a skip counter.
        let mut unresolved: BTreeSet<String> = BTreeSet::new();
        let instruments: Vec<BrokerInstrument> = assets
            .iter()
            .filter_map(|a| {
                let symbol = a["symbol"].as_str()?.to_string();
                let raw_exchange = a["exchange"].as_str();
                let venue = match raw_exchange.and_then(alpaca_exchange_to_mic) {
                    Some(mic) => mic.to_string(),
                    None => {
                        unresolved.insert(raw_exchange.unwrap_or("<missing>").to_string());
                        return None;
                    }
                };
                let definition = InstrumentDef {
                    symbol: symbol.clone(),
                    venue,
                    currency: "USD".to_string(),
                    asset_class: "EQUITY".to_string(),
                    instrument_class: "SPOT".to_string(),
                    name: a["name"].as_str().map(|s| s.to_string()),
                    price_precision: 2,
                    price_increment: 0.01,
                    size_increment: if a["fractionable"].as_bool().unwrap_or(false) { 0.001 } else { 1.0 },
                    lot_size: None,
                    contract_size: 1.0,
                    native_id: a["id"].as_str().map(|s| s.to_string()), // Alpaca asset UUID
                    provider_exchange: raw_exchange.map(|s| s.to_string()),
                    derivative: None,
                    identifiers: Identifiers::default(),
                };
                Some(BrokerInstrument {
                    broker_symbol: symbol,
                    broker_exchange: raw_exchange.map(|s| s.to_string()),
                    is_tradeable: a["tradable"].as_bool().unwrap_or(false),
                    min_quantity: a["min_order_size"].as_str().and_then(|s| s.parse().ok()),
                    max_quantity: None,
                    min_notional: None,
                    max_notional: None,
                    definition,
                })
            })
            .collect();

        if !unresolved.is_empty() {
            warn!(
                "alpaca: no venue MIC for exchange label(s) {unresolved:?} — {} asset(s) skipped; \
                 add the mapping in `alpaca_exchange_to_mic` or seed the venue",
                assets.len() - instruments.len()
            );
        }
        Ok(instruments)
    }

    /// Active option contracts for the given underlyings, following pagination
    /// (`GET /v2/options/contracts`). The compact OSI is both the master symbol and
    /// the order-entry handle; options route by symbol, so the routing native id is
    /// left None. Strike/expiry/kind come straight from Alpaca's contract fields.
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
                let Some(underlying) = c["underlying_symbol"].as_str() else { continue };
                let option_kind = match c["type"].as_str() {
                    Some("call") => Some(OptionKind::Call),
                    Some("put") => Some(OptionKind::Put),
                    _ => None,
                };
                let derivative = DerivativeDef {
                    underlying_symbol: underlying.to_string(),
                    option_kind,
                    strike_price: c["strike_price"].as_str().and_then(|s| s.parse().ok()),
                    expiry_date: c["expiration_date"]
                        .as_str()
                        .and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()),
                    activation_date: None,
                };
                let definition = InstrumentDef {
                    symbol: symbol.to_string(),
                    venue: "OPRA".to_string(),
                    currency: "USD".to_string(),
                    asset_class: "EQUITY".to_string(),
                    instrument_class: "OPTION".to_string(),
                    name: c["name"].as_str().map(|s| s.to_string()),
                    price_precision: 2,
                    price_increment: 0.01,
                    size_increment: 1.0,
                    lot_size: None,
                    contract_size: c["size"].as_str().and_then(|s| s.parse().ok()).unwrap_or(100.0),
                    native_id: None,
                    provider_exchange: Some("OPRA".to_string()),
                    derivative: Some(derivative),
                    identifiers: Identifiers::default(),
                };
                out.push(BrokerInstrument {
                    broker_symbol: symbol.to_string(),
                    broker_exchange: Some("OPRA".to_string()),
                    is_tradeable: c["tradable"].as_bool().unwrap_or(false),
                    min_quantity: Some(1.0), // options trade in whole contracts
                    max_quantity: None,
                    min_notional: None,
                    max_notional: None,
                    definition,
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
impl InstrumentProvider for AlpacaAdapter {
    async fn list_instruments(
        &self,
        option_underlyings: &[String],
    ) -> Result<Vec<BrokerInstrument>, BrokerError> {
        let mut out = self.list_equity_instruments().await?;
        if !option_underlyings.is_empty() {
            out.extend(self.list_option_contracts(option_underlyings).await?);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every exchange label Alpaca actually returns must map. If this fails, the
    /// catalog silently loses that venue's whole listing.
    #[test]
    fn maps_every_live_alpaca_exchange() {
        for label in ["NASDAQ", "NYSE", "ARCA", "AMEX", "BATS", "IEX", "OTC"] {
            assert!(
                alpaca_exchange_to_mic(label).is_some(),
                "no MIC for live Alpaca exchange {label:?}"
            );
        }
    }

    #[test]
    fn maps_to_iso_mics() {
        assert_eq!(alpaca_exchange_to_mic("NASDAQ"), Some("XNAS"));
        assert_eq!(alpaca_exchange_to_mic("NYSEARCA"), Some("ARCX"));
        assert_eq!(alpaca_exchange_to_mic("ARCA"), Some("ARCX"));
    }

    /// An unknown label is declined, not passed through. The old behaviour returned
    /// it verbatim, which then failed the venue FK anonymously inside the catalog.
    #[test]
    fn declines_unknown_exchange() {
        assert_eq!(alpaca_exchange_to_mic("MOONBASE"), None);
        assert_eq!(alpaca_exchange_to_mic(""), None);
    }

    /// A MIC is not an Alpaca label — it is declined too. Guards against assuming
    /// the passthrough was load-bearing for already-MIC inputs.
    #[test]
    fn declines_a_bare_mic() {
        assert_eq!(alpaca_exchange_to_mic("XNAS"), None);
    }
}
