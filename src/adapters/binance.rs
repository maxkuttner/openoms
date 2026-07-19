//! Binance Spot broker adapter (testnet by default).
//!
//! Auth: an API key sent as the `X-MBX-APIKEY` header, plus an HMAC-SHA256
//! `signature` over the exact query string (signed with the secret) on SIGNED
//! endpoints. Every SIGNED request carries a `timestamp` (ms). Order/cancel/
//! account are all SIGNED.
//!
//! Symbol mapping: the OMS instrument's broker xref stores the Binance pair
//! (e.g. `BTCUSDT`) as `external_symbol` — used verbatim as the order `symbol` —
//! and the base asset (e.g. `BTC`) as `external_native_id`, which is what account
//! balances report and reconciliation matches on.

use base64::Engine;
use dataprovider::{Identifiers, InstrumentDef};
use ed25519_dalek::pkcs8::DecodePrivateKey;
use ed25519_dalek::{Signer, SigningKey};
use reqwest::{Client, Method};
use tracing::info;

use super::{
    BrokerAdapter, BrokerError, BrokerHolding, BrokerInstrument, BrokerOrderRequest,
    BrokerOrderResponse, InstrumentProvider,
};

/// Read a Binance symbol filter's numeric field (e.g. LOT_SIZE.stepSize).
fn filter_val(filters: &[serde_json::Value], filter_type: &str, field: &str) -> Option<f64> {
    filters
        .iter()
        .find(|f| f["filterType"].as_str() == Some(filter_type))
        .and_then(|f| f[field].as_str())
        .and_then(|s| s.parse().ok())
}

/// Percent-encode the non-unreserved characters of a base64 string (`+`, `/`, `=`)
/// so an Ed25519 signature is safe inside a URL query string.
fn pct_encode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '+' => "%2B".to_string(),
            '/' => "%2F".to_string(),
            '=' => "%3D".to_string(),
            other => other.to_string(),
        })
        .collect()
}

#[derive(Clone)]
pub struct BinanceAdapter {
    client: Client,
    api_key: String,
    signing_key: SigningKey,
    base_url: &'static str,
    ws_url: &'static str,
}

impl BinanceAdapter {
    /// `private_key_pem` is the Ed25519 private key (PKCS#8 PEM) whose public key
    /// is registered on Binance for `api_key`.
    pub fn new(api_key: String, private_key_pem: &str, environment: &str) -> Result<Self, String> {
        let (base_url, ws_url) = if environment == "LIVE" {
            ("https://api.binance.com", "wss://ws-api.binance.com:443/ws-api/v3")
        } else {
            ("https://testnet.binance.vision", "wss://ws-api.testnet.binance.vision/ws-api/v3")
        };
        let signing_key = SigningKey::from_pkcs8_pem(private_key_pem)
            .map_err(|e| format!("invalid Ed25519 private key PEM: {e}"))?;
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        Ok(Self { client, api_key, signing_key, base_url, ws_url })
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    pub fn ws_url(&self) -> &'static str {
        self.ws_url
    }

    /// Ed25519-sign a payload, base64-encoded — used for REST `signature` and the
    /// WS-API `session.logon` signature.
    pub fn sign(&self, payload: &str) -> String {
        let sig = self.signing_key.sign(payload.as_bytes());
        base64::engine::general_purpose::STANDARD.encode(sig.to_bytes())
    }

    /// Send a SIGNED request: append `timestamp`, sign the query, append the
    /// signature, attach the API-key header. Params values here (symbols, numeric
    /// quantities, UUID client ids) contain no characters needing URL-encoding.
    async fn signed(
        &self,
        method: Method,
        endpoint: &str,
        params: &[(&str, String)],
    ) -> Result<reqwest::Response, BrokerError> {
        let ts = chrono::Utc::now().timestamp_millis();
        let mut query: String = params
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        if !query.is_empty() {
            query.push('&');
        }
        query.push_str(&format!("timestamp={ts}"));
        // The base64 Ed25519 signature can contain +, /, = — percent-encode it so
        // it survives the query string intact.
        let signature = pct_encode(&self.sign(&query));
        let url = format!("{}{}?{}&signature={}", self.base_url, endpoint, query, signature);

        self.client
            .request(method, &url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send()
            .await
            .map_err(|e| BrokerError::Network(e.to_string()))
    }

    /// GET /api/v3/exchangeInfo — the full spot catalog as canonical instrument
    /// records + routing handles. Public (unsigned). Binance is the source of the
    /// master crypto instrument: the pair is the Symbol@Venue symbol, venue =
    /// BINANCE, currency = the quote asset, and the base asset is the routing native
    /// id (what account balances / recon match on). Only `TRADING` spot pairs.
    pub async fn list_spot_instruments(&self) -> Result<Vec<BrokerInstrument>, BrokerError> {
        let url = format!("{}/api/v3/exchangeInfo", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| BrokerError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(BrokerError::BrokerRejected(resp.text().await.unwrap_or_default()));
        }
        let body: serde_json::Value =
            resp.json().await.map_err(|e| BrokerError::Network(e.to_string()))?;
        let symbols = body["symbols"].as_array().cloned().unwrap_or_default();
        Ok(symbols
            .iter()
            .filter_map(|s| {
                let pair = s["symbol"].as_str()?.to_string();
                let base = s["baseAsset"].as_str()?.to_string();
                let quote = s["quoteAsset"].as_str()?.to_string();
                let is_spot = s["isSpotTradingAllowed"].as_bool().unwrap_or(true);
                if !is_spot {
                    return None;
                }
                let filters: Vec<serde_json::Value> =
                    s["filters"].as_array().cloned().unwrap_or_default();
                let definition = InstrumentDef {
                    symbol: pair.clone(),
                    venue: "BINANCE".to_string(),
                    currency: quote,
                    asset_class: "CRYPTO".to_string(),
                    instrument_class: "SPOT".to_string(),
                    name: None,
                    price_precision: s["quoteAssetPrecision"].as_i64().unwrap_or(8) as i32,
                    price_increment: filter_val(&filters, "PRICE_FILTER", "tickSize").unwrap_or(0.0),
                    size_increment: filter_val(&filters, "LOT_SIZE", "stepSize").unwrap_or(0.0),
                    lot_size: None,
                    contract_size: 1.0,
                    native_id: Some(base), // base asset — what recon matches balances on
                    provider_exchange: Some("BINANCE".to_string()),
                    derivative: None,
                    identifiers: Identifiers::default(),
                };
                Some(BrokerInstrument {
                    broker_symbol: pair,
                    broker_exchange: Some("BINANCE".to_string()),
                    is_tradeable: s["status"].as_str() == Some("TRADING"),
                    min_quantity: filter_val(&filters, "LOT_SIZE", "minQty"),
                    max_quantity: filter_val(&filters, "LOT_SIZE", "maxQty"),
                    min_notional: filter_val(&filters, "NOTIONAL", "minNotional")
                        .or_else(|| filter_val(&filters, "MIN_NOTIONAL", "minNotional")),
                    max_notional: None,
                    definition,
                })
            })
            .collect())
    }

    /// GET /api/v3/order — fetch one order's state (for startup reconciliation of
    /// routed orders). Needs the symbol alongside the order id.
    pub async fn get_order(
        &self,
        symbol: &str,
        external_order_id: &str,
    ) -> Result<serde_json::Value, BrokerError> {
        let params = [
            ("symbol", symbol.to_string()),
            ("orderId", external_order_id.to_string()),
        ];
        let resp = self.signed(Method::GET, "/api/v3/order", &params).await?;
        if resp.status().is_success() {
            resp.json().await.map_err(|e| BrokerError::Network(e.to_string()))
        } else {
            Err(BrokerError::BrokerRejected(resp.text().await.unwrap_or_default()))
        }
    }
}

#[async_trait::async_trait]
impl BrokerAdapter for BinanceAdapter {
    async fn submit_order(&self, req: &BrokerOrderRequest) -> Result<BrokerOrderResponse, BrokerError> {
        let side = req.side.to_uppercase(); // "buy"/"sell" -> BUY/SELL
        let mut params: Vec<(&str, String)> = vec![
            ("symbol", req.symbol.clone()), // the Binance pair, e.g. BTCUSDT
            ("side", side),
            ("newClientOrderId", req.order_id.clone()), // our UUID, echoed on fills
            ("quantity", req.quantity.to_string()),
        ];

        match req.order_type.as_str() {
            "market" => params.push(("type", "MARKET".to_string())),
            "limit" => {
                let price = req
                    .limit_price
                    .ok_or_else(|| BrokerError::BrokerRejected("limit order missing limit_price".into()))?;
                // Day is meaningless on a 24/7 venue — treat as GTC.
                let tif = match req.time_in_force.to_uppercase().as_str() {
                    "IOC" => "IOC",
                    "FOK" => "FOK",
                    _ => "GTC",
                };
                params.push(("type", "LIMIT".to_string()));
                params.push(("timeInForce", tif.to_string()));
                params.push(("price", price.to_string()));
            }
            other => return Err(BrokerError::NotConfigured(format!("unsupported order type: {other}"))),
        }

        info!(symbol = %req.symbol, side = %req.side, "submitting order to Binance");

        let resp = self.signed(Method::POST, "/api/v3/order", &params).await?;
        if resp.status().is_success() {
            let body: serde_json::Value =
                resp.json().await.map_err(|e| BrokerError::Network(e.to_string()))?;
            // Binance returns orderId as a JSON number.
            let external_order_id = body["orderId"]
                .as_i64()
                .map(|id| id.to_string())
                .or_else(|| body["orderId"].as_str().map(|s| s.to_string()))
                .unwrap_or_default();
            Ok(BrokerOrderResponse { external_order_id })
        } else {
            Err(BrokerError::BrokerRejected(resp.text().await.unwrap_or_default()))
        }
    }

    async fn cancel_order(&self, external_order_id: &str, symbol: &str) -> Result<(), BrokerError> {
        // Binance requires the symbol to cancel a specific order.
        let params = [
            ("symbol", symbol.to_string()),
            ("orderId", external_order_id.to_string()),
        ];
        let resp = self.signed(Method::DELETE, "/api/v3/order", &params).await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(BrokerError::BrokerRejected(resp.text().await.unwrap_or_default()))
        }
    }

    /// GET /api/v3/account — spot balances. Each non-zero asset becomes a holding
    /// keyed by the asset code (the base-asset used in the broker xref), so
    /// reconciliation resolves it to the instrument. Spot is long-only, so qty is
    /// always positive.
    async fn get_positions(&self) -> Result<Vec<BrokerHolding>, BrokerError> {
        let resp = self.signed(Method::GET, "/api/v3/account", &[]).await?;
        if !resp.status().is_success() {
            return Err(BrokerError::BrokerRejected(resp.text().await.unwrap_or_default()));
        }
        let body: serde_json::Value =
            resp.json().await.map_err(|e| BrokerError::Network(e.to_string()))?;
        let balances = body["balances"].as_array().cloned().unwrap_or_default();
        let holdings = balances
            .iter()
            .filter_map(|b| {
                let asset = b["asset"].as_str()?.to_string();
                let free: f64 = b["free"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let locked: f64 = b["locked"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let qty = free + locked;
                if qty > 0.0 {
                    Some(BrokerHolding { symbol: asset.clone(), native_id: Some(asset), qty })
                } else {
                    None
                }
            })
            .collect();
        Ok(holdings)
    }
}

#[async_trait::async_trait]
impl InstrumentProvider for BinanceAdapter {
    /// Binance has no separate option catalog on the spot venue; `option_underlyings`
    /// is ignored — it always returns the spot pairs.
    async fn list_instruments(
        &self,
        _option_underlyings: &[String],
    ) -> Result<Vec<BrokerInstrument>, BrokerError> {
        self.list_spot_instruments().await
    }
}
