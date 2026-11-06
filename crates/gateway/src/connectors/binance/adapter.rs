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

use crate::adapter::{
    BrokerAdapter, BrokerError, BrokerInstrument, BrokerOrderRequest,
    BrokerOrderResponse, InstrumentProvider,
};
use crate::md::daily_stats::DailyStats;
use crate::contract::{BrokerOpenOrder, BrokerOrderOutcome, BrokerOrderState};

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

/// A Binance Ed25519 API key, plus the FIX session it uses when the transport is FIX
/// (REST ignores the session fields).
/// Stored as `{"kind":"BinanceFix", …}` (see [`crate::credentials::BrokerCredentials`]).
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct BinanceCredentials {
    pub host: String,
    pub port: u16,
    pub sender_comp_id: String,
    pub target_comp_id: String,
    pub api_key: String,
    /// PEM contents, not a path. A filesystem path is precisely what stops a
    /// credential being configurable from anywhere but the server's shell.
    pub private_key: String,
}

impl BinanceCredentials {
    pub const KIND: &'static str = "BinanceFix";
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
        Ok(parse_exchange_info(&body))
    }

    /// One pair's catalog entry (`exchangeInfo?symbol=`), for a pair a followed
    /// account traded that the catalog doesn't have yet.
    pub async fn symbol_instrument(&self, symbol: &str) -> Result<Option<BrokerInstrument>, BrokerError> {
        let url = format!("{}/api/v3/exchangeInfo?symbol={symbol}", self.base_url);
        let resp = self.client.get(&url).send().await.map_err(|e| BrokerError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(crate::adapter::http_failure(status, resp.text().await.unwrap_or_default()));
        }
        let body: serde_json::Value = resp.json().await.map_err(|e| BrokerError::Network(e.to_string()))?;
        Ok(parse_exchange_info(&body).into_iter().find(|i| i.broker_symbol == symbol))
    }

    /// GET /api/v3/myTrades — up to 1000 of the account's fills on `symbol`:
    /// from a trade id on, or else from a start time (milliseconds), for
    /// filling a follow stream's gap.
    pub async fn my_trades(&self, symbol: &str, from_trade_id: Option<i64>, start_time_ms: Option<i64>) -> Result<Vec<crate::contract::ExternalFill>, BrokerError> {
        let mut params = vec![("symbol", symbol.to_string()), ("limit", "1000".to_string())];
        match (from_trade_id, start_time_ms) {
            (Some(from), _) => params.push(("fromId", from.to_string())),
            (None, Some(start)) => params.push(("startTime", start.to_string())),
            (None, None) => {}
        }
        let resp = self.signed(Method::GET, "/api/v3/myTrades", &params).await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(crate::adapter::http_failure(status, resp.text().await.unwrap_or_default()));
        }
        let body: serde_json::Value = resp.json().await.map_err(|e| BrokerError::Network(e.to_string()))?;
        Ok(parse_my_trades(&body))
    }

    /// GET /api/v3/order — fetch one order's state (for startup reconciliation of
    /// routed orders). Needs the symbol alongside the order id.
    pub async fn get_order(
        &self,
        symbol: &str,
        external_order_id: &str,
    ) -> Result<serde_json::Value, BrokerError> {
        self.get_order_by(symbol, "", external_order_id).await
    }

    /// One order, by Binance's id when known, else by our client order id.
    async fn get_order_by(&self, symbol: &str, client_order_id: &str, external_order_id: &str) -> Result<serde_json::Value, BrokerError> {
        let params = order_lookup_params(symbol, client_order_id, external_order_id);
        let resp = self.signed(Method::GET, "/api/v3/order", &params).await?;
        if resp.status().is_success() {
            resp.json().await.map_err(|e| BrokerError::Network(e.to_string()))
        } else {
            Err(lookup_error(resp.text().await.unwrap_or_default()))
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
            let status = resp.status().as_u16();
            Err(submit_error(status, resp.text().await.unwrap_or_default()))
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

    /// GET /api/v3/openOrders — every currently-open order across all symbols in one
    /// signed call (the snapshot side of order reconciliation).
    async fn open_orders(&self) -> Result<Vec<BrokerOpenOrder>, BrokerError> {
        let resp = self.signed(Method::GET, "/api/v3/openOrders", &[]).await?;
        if !resp.status().is_success() {
            return Err(BrokerError::BrokerRejected(resp.text().await.unwrap_or_default()));
        }
        let body: serde_json::Value =
            resp.json().await.map_err(|e| BrokerError::Network(e.to_string()))?;
        Ok(parse_open_orders(&body))
    }

    /// One order's current state via GET /api/v3/order, normalized to a
    /// [`BrokerOrderState`] for the reconcile shell.
    async fn order_status(
        &self,
        client_order_id: &str,
        external_order_id: &str,
        symbol: &str,
    ) -> Result<BrokerOrderState, BrokerError> {
        let order = self.get_order_by(symbol, client_order_id, external_order_id).await?;
        let (executed_qty, avg_px) = exec_stats(&order);
        let outcome = match order["status"].as_str().unwrap_or("") {
            "FILLED" => BrokerOrderOutcome::Filled,
            "CANCELED" => BrokerOrderOutcome::Canceled,
            "REJECTED" | "EXPIRED" => BrokerOrderOutcome::Rejected,
            _ => BrokerOrderOutcome::Working, // NEW / PARTIALLY_FILLED — still live
        };
        Ok(BrokerOrderState { outcome, executed_qty, avg_px })
    }
}

#[async_trait::async_trait]
impl DailyStats for BinanceAdapter {
    async fn daily_stats(&self, symbols: &[String]) -> Result<Vec<(String, f64)>, BrokerError> {
        if symbols.is_empty() {
            return Ok(Vec::new());
        }
        // Public endpoint — no signing needed, same as exchangeInfo.
        let symbols_json = serde_json::to_string(symbols).unwrap_or_default();
        // Inline URL-encoding fallback: replace JSON special chars for URL safety.
        let encoded_json = symbols_json
            .replace('"', "%22")
            .replace(',', "%2C")
            .replace('[', "%5B")
            .replace(']', "%5D");
        let url = format!(
            "{}/api/v3/ticker/24hr?symbols={}",
            self.base_url,
            encoded_json
        );
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| BrokerError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(BrokerError::BrokerRejected(format!(
                "24hr ticker request failed: {}",
                resp.status()
            )));
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| BrokerError::Network(e.to_string()))?;
        Ok(parse_binance_24hr_stats(&body))
    }
}

/// Parse `(executed_qty, avg_px)` from a Binance order JSON. `avg_px` is the executed
/// volume-weighted price (`cummulativeQuoteQty / executedQty`), 0 when nothing filled.
fn exec_stats(order: &serde_json::Value) -> (f64, f64) {
    let executed_qty: f64 =
        order["executedQty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let quote: f64 =
        order["cummulativeQuoteQty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let avg_px = if executed_qty > 0.0 { quote / executed_qty } else { 0.0 };
    (executed_qty, avg_px)
}

/// `GET /api/v3/ticker/24hr?symbols=[...]` returns an array; `prevClosePrice`
/// is a string (Binance quotes all prices as strings to avoid float
/// round-tripping ambiguity). An entry whose price doesn't parse is skipped,
/// never reported as a zero previous close.
fn parse_binance_24hr_stats(body: &serde_json::Value) -> Vec<(String, f64)> {
    let Some(entries) = body.as_array() else { return Vec::new() };
    entries
        .iter()
        .filter_map(|entry| {
            let symbol = entry.get("symbol")?.as_str()?.to_string();
            let price: f64 = entry.get("prevClosePrice")?.as_str()?.parse().ok()?;
            Some((symbol, price))
        })
        .collect()
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

/// `GET /api/v3/order` parameters: by Binance's id when known, else by our
/// client order id.
pub(crate) fn order_lookup_params(symbol: &str, client_order_id: &str, external_order_id: &str) -> Vec<(&'static str, String)> {
    if external_order_id.is_empty() {
        vec![("symbol", symbol.to_string()), ("origClientOrderId", client_order_id.to_string())]
    } else {
        vec![("symbol", symbol.to_string()), ("orderId", external_order_id.to_string())]
    }
}

/// A failed order placement. Binance says -1007 / -1006 when it cannot tell
/// whether the order executed, and a 5xx likewise: not a rejection.
pub(crate) fn submit_error(status: u16, body: String) -> BrokerError {
    if body.contains("\"code\":-1007") || body.contains("\"code\":-1006") {
        BrokerError::Network(body)
    } else {
        crate::adapter::http_failure(status, body)
    }
}

/// A failed order lookup: Binance's -2013 means it has no such order.
pub(crate) fn lookup_error(body: String) -> BrokerError {
    if body.contains("\"code\":-2013") {
        BrokerError::NoSuchOrder(body)
    } else {
        BrokerError::BrokerRejected(body)
    }
}

/// `GET /api/v3/openOrders` as snapshot rows.
pub(crate) fn parse_open_orders(body: &serde_json::Value) -> Vec<BrokerOpenOrder> {
    body.as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| {
            let (executed_qty, _) = exec_stats(o);
            Some(BrokerOpenOrder {
                client_order_id: o["clientOrderId"].as_str()?.to_string(),
                venue_order_id: o["orderId"].as_i64().map(|i| i.to_string()).unwrap_or_default(),
                symbol: o["symbol"].as_str()?.to_string(),
                executed_qty,
            })
        })
        .collect()
}

/// `exchangeInfo` as catalog rows: canonical instrument records plus routing
/// handles. Binance is the source of the master crypto instrument: the pair is
/// the symbol, venue BINANCE, currency the quote asset, and the base asset the
/// routing native id. Spot pairs only.
pub(crate) fn parse_exchange_info(body: &serde_json::Value) -> Vec<BrokerInstrument> {
    let symbols = body["symbols"].as_array().cloned().unwrap_or_default();
    symbols
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
        .collect()
}

/// `myTrades` rows as external fills (the history carries no client order id).
pub(crate) fn parse_my_trades(body: &serde_json::Value) -> Vec<crate::contract::ExternalFill> {
    body.as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            Some(crate::contract::ExternalFill {
                trade_id: t["id"].as_i64()?.to_string(),
                venue_order_id: t["orderId"].as_i64()?.to_string(),
                client_order_id: String::new(),
                symbol: t["symbol"].as_str()?.to_string(),
                buy: t["isBuyer"].as_bool()?,
                qty: t["qty"].as_str()?.parse().ok()?,
                price: t["price"].as_str()?.parse().ok()?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn my_trades_parse_into_external_fills() {
        let body = serde_json::json!([
            { "symbol": "BTCUSDT", "id": 501, "orderId": 9001, "price": "64000.5", "qty": "0.25", "isBuyer": true },
            { "symbol": "BTCUSDT", "id": 502, "orderId": 9003, "price": "64010", "qty": "0.1", "isBuyer": false },
        ]);
        let fills = parse_my_trades(&body);
        assert_eq!(fills.len(), 2);
        assert_eq!((fills[0].trade_id.as_str(), fills[0].venue_order_id.as_str(), fills[0].buy, fills[0].qty), ("501", "9001", true, 0.25));
        assert!(!fills[1].buy);
    }

    #[test]
    fn instrument_from_exchange_info() {
        let body = serde_json::json!({ "symbols": [{
            "symbol": "SOLBTC", "baseAsset": "SOL", "quoteAsset": "BTC", "status": "TRADING", "quoteAssetPrecision": 8,
            "filters": [
                { "filterType": "PRICE_FILTER", "tickSize": "0.0000001" },
                { "filterType": "LOT_SIZE", "stepSize": "0.001", "minQty": "0.001", "maxQty": "9000" }
            ]
        }]});
        let list = parse_exchange_info(&body);
        assert_eq!(list.len(), 1);
        assert_eq!((list[0].broker_symbol.as_str(), list[0].definition.currency.as_str()), ("SOLBTC", "BTC"));
        assert!(list[0].is_tradeable);
    }

    #[test]
    fn an_unknown_execution_status_is_not_a_rejection() {
        assert!(matches!(submit_error(400, r#"{"code":-1007,"msg":"Timeout waiting for response"}"#.into()), BrokerError::Network(_)));
        assert!(matches!(submit_error(400, r#"{"code":-1006,"msg":"Unexpected response"}"#.into()), BrokerError::Network(_)));
        assert!(matches!(submit_error(503, "".into()), BrokerError::Network(_)));
        assert!(matches!(submit_error(400, r#"{"code":-2010,"msg":"insufficient balance"}"#.into()), BrokerError::BrokerRejected(_)));
    }

    #[test]
    fn open_orders_carry_the_venue_order_id() {
        let body = serde_json::json!([{ "orderId": 42, "clientOrderId": "c-1", "symbol": "BTCUSDT", "executedQty": "0", "cummulativeQuoteQty": "0" }]);
        assert_eq!(parse_open_orders(&body)[0].venue_order_id, "42");
    }

    #[test]
    fn order_status_looks_up_by_client_id_without_a_venue_id() {
        assert_eq!(order_lookup_params("BTCUSDT", "c-1", "42")[1], ("orderId", "42".to_string()));
        assert_eq!(order_lookup_params("BTCUSDT", "c-1", "")[1], ("origClientOrderId", "c-1".to_string()));
    }

    #[test]
    fn an_unknown_order_is_no_such_order() {
        assert!(matches!(lookup_error(r#"{"code":-2013,"msg":"Order does not exist."}"#.into()), BrokerError::NoSuchOrder(_)));
        assert!(matches!(lookup_error(r#"{"code":-1021,"msg":"Timestamp"}"#.into()), BrokerError::BrokerRejected(_)));
    }

    #[test]
    fn parses_prev_close_price_from_24hr_ticker_array() {
        let body: serde_json::Value = serde_json::from_str(
            r#"[
                {"symbol": "BTCUSDT", "prevClosePrice": "64000.50"},
                {"symbol": "ETHUSDT", "prevClosePrice": "3200.10"}
            ]"#,
        )
        .unwrap();
        let mut stats = parse_binance_24hr_stats(&body);
        stats.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            stats,
            vec![("BTCUSDT".to_string(), 64000.50), ("ETHUSDT".to_string(), 3200.10)]
        );
    }

    #[test]
    fn an_entry_with_unparseable_price_is_skipped_not_zeroed() {
        let body: serde_json::Value = serde_json::from_str(
            r#"[{"symbol": "BTCUSDT", "prevClosePrice": "64000.50"}, {"symbol": "BROKEN", "prevClosePrice": "not-a-number"}]"#,
        )
        .unwrap();
        let stats = parse_binance_24hr_stats(&body);
        assert_eq!(stats, vec![("BTCUSDT".to_string(), 64000.50)]);
    }
}
