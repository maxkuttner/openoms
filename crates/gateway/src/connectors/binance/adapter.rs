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
use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};

use crate::venue_event::{Cursor, ExecutionSource, PullHints, PullPage, VenueFill, VenueOrderStatus, VenueOrderUpdate};
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

/// Binance refuses a signed request that arrives later than this after its
/// `timestamp`, so a lost submit cannot be accepted after it.
pub const RECV_WINDOW_MS: u64 = 5_000;

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

    /// Send a SIGNED request: append `recvWindow` and `timestamp`, sign the query, append the
    /// signature, attach the API-key header. Params values here (symbols, numeric
    /// quantities, UUID client ids) contain no characters needing URL-encoding.
    pub(crate) async fn signed(
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
        query.push_str(&format!("recvWindow={RECV_WINDOW_MS}&timestamp={ts}"));
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

    /// Raw `myTrades` rows (GET /api/v3/myTrades, up to 1000 of the account's
    /// fills on `symbol`, from a trade id or a start time in milliseconds).
    pub(crate) async fn my_trades_body(&self, symbol: &str, from_trade_id: Option<i64>, start_time_ms: Option<i64>) -> Result<serde_json::Value, BrokerError> {
        let mut params = vec![("symbol", symbol.to_string()), ("limit", MY_TRADES_LIMIT.to_string())];
        match (from_trade_id, start_time_ms) {
            (Some(from), _) => params.push(("fromId", from.to_string())),
            (None, Some(start)) => params.push(("startTime", start.to_string())),
            (None, None) => {}
        }
        self.my_trades_query(&params).await
    }

    async fn my_trades_query(&self, params: &[(&'static str, String)]) -> Result<serde_json::Value, BrokerError> {
        let resp = self.signed(Method::GET, "/api/v3/myTrades", &params).await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(crate::adapter::http_failure(status, resp.text().await.unwrap_or_default()));
        }
        resp.json().await.map_err(|e| BrokerError::Network(e.to_string()))
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
    pub(crate) async fn get_order_by(&self, symbol: &str, client_order_id: &str, external_order_id: &str) -> Result<serde_json::Value, BrokerError> {
        let params = order_lookup_params(symbol, client_order_id, external_order_id);
        let resp = self.signed(Method::GET, "/api/v3/order", &params).await?;
        if resp.status().is_success() {
            resp.json().await.map_err(|e| BrokerError::Network(e.to_string()))
        } else {
            Err(lookup_error(resp.text().await.unwrap_or_default()))
        }
    }
}

/// A price or quantity as Binance accepts it: at most 8 decimals, no
/// exponent, trailing zeros dropped. Plain float formatting can print float
/// noise (`0.30000000000000004`), which Binance refuses as too precise.
fn decimal(value: f64) -> String {
    let fixed = format!("{value:.8}");
    let trimmed = fixed.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "-" { "0".into() } else { trimmed.into() }
}

#[async_trait::async_trait]
impl BrokerAdapter for BinanceAdapter {
    async fn submit_order(&self, req: &BrokerOrderRequest) -> Result<BrokerOrderResponse, BrokerError> {
        let side = req.side.to_uppercase(); // "buy"/"sell" -> BUY/SELL
        let mut params: Vec<(&str, String)> = vec![
            ("symbol", req.symbol.clone()), // the Binance pair, e.g. BTCUSDT
            ("side", side),
            ("newClientOrderId", req.order_id.clone()), // our UUID, echoed on fills
            ("quantity", decimal(req.quantity)),
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
                params.push(("price", decimal(price)));
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
            Err(cancel_error(resp.text().await.unwrap_or_default()))
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

/// A failed cancel: Binance's -2011 ("Unknown order sent") means no open order
/// has that id any more: it filled, was cancelled, or never existed.
pub(crate) fn cancel_error(body: String) -> BrokerError {
    if body.contains("\"code\":-2011") {
        BrokerError::NoSuchOrder(body)
    } else {
        BrokerError::BrokerRejected(body)
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

/// `myTrades` rows as venue fills (the history carries no client order id).
pub(crate) fn parse_my_trades(body: &serde_json::Value, account_ref: &str) -> Vec<VenueFill> {
    body.as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            Some(VenueFill {
                exec_id: super::fill_exec_id(t["symbol"].as_str()?, &t["id"].as_i64()?.to_string(), t["isBuyer"].as_bool()?),
                venue_order_id: t["orderId"].as_i64()?.to_string(),
                client_order_id: None,
                account_ref: account_ref.to_string(),
                symbol: t["symbol"].as_str()?.to_string(),
                buy: t["isBuyer"].as_bool()?,
                qty: t["qty"].as_str()?.parse().ok()?,
                price: t["price"].as_str()?.parse().ok()?,
                fee: match (t["commission"].as_str().and_then(|v| v.parse::<f64>().ok()), t["commissionAsset"].as_str()) {
                    (Some(a), Some(c)) if a != 0.0 && !c.is_empty() => Some((a, c.to_string())),
                    _ => None,
                },
                executed_at: t["time"].as_i64().and_then(|ms| Utc.timestamp_millis_opt(ms).single()),
            })
        })
        .collect()
}

/// Most rows one `myTrades` call returns.
const MY_TRADES_LIMIT: usize = 1000;
/// The `fromId` of the next `myTrades` call: after the cursor's trade id, or
/// the symbol's oldest trade when there is no cursor yet (paging by id costs
/// one call per 1000 trades, where scanning time windows costs one per day).
pub(crate) fn from_id(from: Option<&Cursor>) -> i64 {
    from.and_then(|c| c.0.parse::<i64>().ok()).map_or(0, |id| id + 1)
}

/// `myTrades` rows as a pull page: the next cursor is the last trade id (empty
/// when there were none), and a full page (`limit` rows) means there may be
/// more. Rows executed before `since` are dropped (on every page); the
/// cursor still moves past them.
pub(crate) fn trades_page(body: &serde_json::Value, account_ref: &str, limit: usize, since: DateTime<Utc>) -> PullPage {
    let rows = body.as_array().map(Vec::len).unwrap_or(0);
    let all = parse_my_trades(body, account_ref);
    // The cursor is the raw trade id (`fromId`), not the prefixed exec id.
    let next = Cursor(
        body.as_array()
            .and_then(|a| a.iter().rev().find_map(|t| t["id"].as_i64()))
            .map(|id| id.to_string())
            .unwrap_or_default(),
    );
    let fills = all
        .into_iter()
        .filter(|t| match t.executed_at {
            Some(at) => at >= since,
            _ => true,
        })
        .collect();
    PullPage { fills, next, more: rows >= limit }
}

/// `GET /api/v3/openOrders` as the venue's view of each open order.
pub(crate) fn parse_open_order_snapshot(body: &serde_json::Value, account_ref: &str) -> Vec<VenueOrderUpdate> {
    let num = |v: &serde_json::Value| v.as_str().and_then(|s| s.parse::<f64>().ok());
    body.as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| {
            let order_type = o["type"].as_str().unwrap_or("").to_string();
            Some(VenueOrderUpdate {
                venue_order_id: o["orderId"].as_i64()?.to_string(),
                client_order_id: o["clientOrderId"].as_str().filter(|c| !c.is_empty()).map(str::to_string),
                account_ref: account_ref.to_string(),
                symbol: o["symbol"].as_str()?.to_string(),
                buy: o["side"].as_str()? == "BUY",
                limit_px: if order_type == "MARKET" { None } else { num(&o["price"]).filter(|p| *p != 0.0) },
                order_type,
                qty: num(&o["origQty"])?,
                status: VenueOrderStatus::Working,
                cum_qty: num(&o["executedQty"]).unwrap_or(0.0),
                ts: o["updateTime"].as_i64().or(o["time"].as_i64()).and_then(|ms| Utc.timestamp_millis_opt(ms).single()),
                created_at: o["time"].as_i64().and_then(|ms| Utc.timestamp_millis_opt(ms).single()),
            })
        })
        .collect()
}

/// The distinct symbols of `open`, sorted.
pub(crate) fn open_order_symbols(open: &[VenueOrderUpdate]) -> Vec<String> {
    open.iter().map(|o| o.symbol.clone()).collect::<std::collections::BTreeSet<_>>().into_iter().collect()
}

/// Binance serves executions one symbol at a time (`myTrades`), so a scope is
/// a symbol. One API key is one account, so `account_ref` is only echoed into
/// what is returned.
#[async_trait]
impl ExecutionSource for BinanceAdapter {
    async fn fills_since(
        &self,
        account_ref: &str,
        scope: &str,
        from: Option<&Cursor>,
        since: DateTime<Utc>,
    ) -> Result<PullPage, BrokerError> {
        let params = vec![
            ("symbol", scope.to_string()),
            ("limit", MY_TRADES_LIMIT.to_string()),
            ("fromId", from_id(from).to_string()),
        ];
        let body = self.my_trades_query(&params).await?;
        Ok(trades_page(&body, account_ref, MY_TRADES_LIMIT, since))
    }

    async fn open_order_snapshot(&self, account_ref: &str) -> Result<Vec<VenueOrderUpdate>, BrokerError> {
        let resp = self.signed(Method::GET, "/api/v3/openOrders", &[]).await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(crate::adapter::http_failure(status, resp.text().await.unwrap_or_default()));
        }
        let body: serde_json::Value = resp.json().await.map_err(|e| BrokerError::Network(e.to_string()))?;
        Ok(parse_open_order_snapshot(&body, account_ref))
    }

    /// The symbols of the account's open orders. The OMS adds the symbols it
    /// has orders on for the account, since it holds them and this crate has
    /// no database (see the OMS reconciler's `scopes`).
    async fn scopes(&self, account_ref: &str) -> Result<Vec<String>, BrokerError> {
        Ok(open_order_symbols(&self.open_order_snapshot(account_ref).await?))
    }

    /// GET /api/v3/account (weight 20): assets with free + locked above zero.
    async fn held_assets(&self, _account_ref: &str) -> Result<Vec<String>, BrokerError> {
        let resp = self.signed(Method::GET, "/api/v3/account", &[("omitZeroBalances", "true".to_string())]).await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(crate::adapter::http_failure(status, resp.text().await.unwrap_or_default()));
        }
        let body: serde_json::Value = resp.json().await.map_err(|e| BrokerError::Network(e.to_string()))?;
        Ok(nonzero_assets(&body))
    }

    fn hints(&self) -> PullHints {
        PullHints { intraday_secs: 60, max_history: None, per_symbol: true }
    }
}

/// The assets of `/api/v3/account` `balances` whose free + locked is above zero.
pub(crate) fn nonzero_assets(body: &serde_json::Value) -> Vec<String> {
    let num = |v: &serde_json::Value| v.as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
    body["balances"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| num(&b["free"]) + num(&b["locked"]) > 0.0)
        .filter_map(|b| b["asset"].as_str().map(str::to_string))
        .collect()
}

#[cfg(test)]
mod tests {

    #[test]
    fn decimals_carry_no_float_noise_or_exponent() {
        assert_eq!(super::decimal(0.1 + 0.2), "0.3");
        assert_eq!(super::decimal(64000.0), "64000");
        assert_eq!(super::decimal(0.00001), "0.00001");
        assert_eq!(super::decimal(1e-9), "0");
    }
    use super::*;

    #[test]
    fn held_assets_are_the_nonzero_balances() {
        let body = serde_json::json!({ "balances": [
            { "asset": "BTC", "free": "0.5", "locked": "0" },
            { "asset": "ETH", "free": "0", "locked": "1.2" },
            { "asset": "BNB", "free": "0.00000000", "locked": "0.00000000" },
        ]});
        assert_eq!(nonzero_assets(&body), ["BTC", "ETH"]);
    }

    #[test]
    fn my_trades_page_maps_to_venue_fills_with_next_cursor() {
        let body = serde_json::json!([
            { "symbol": "BTCUSDT", "id": 501, "orderId": 9001, "price": "64000.5", "qty": "0.25", "isBuyer": true, "time": 1700000000000i64, "commission": "0.01", "commissionAsset": "BNB" },
            { "symbol": "BTCUSDT", "id": 502, "orderId": 9003, "price": "64010", "qty": "0.1", "isBuyer": false, "commission": "0", "commissionAsset": "USDT" },
        ]);
        let page = trades_page(&body, "acct-1", 1000, DateTime::<Utc>::MIN_UTC);
        assert_eq!(page.fills.len(), 2);
        let f = &page.fills[0];
        assert_eq!((f.exec_id.as_str(), f.venue_order_id.as_str(), f.account_ref.as_str(), f.symbol.as_str()), ("BTCUSDT:501:B", "9001", "acct-1", "BTCUSDT"));
        assert_eq!((f.buy, f.qty, f.price, f.client_order_id.clone()), (true, 0.25, 64000.5, None));
        assert_eq!(f.fee, Some((0.01, "BNB".into())));
        assert_eq!(page.fills[1].fee, None);
        assert_eq!(page.next, crate::venue_event::Cursor("502".into()));
        assert!(!page.more, "a short page is the last");
        let full = trades_page(&body, "acct-1", 2, DateTime::<Utc>::MIN_UTC);
        assert!(full.more, "a full page asks for the next");
    }

    #[test]
    fn a_trade_id_cursor_pages_from_the_next_id() {
        // `fromId` is inclusive, so the cursor's own trade is not pulled again.
        assert_eq!(from_id(Some(&crate::venue_event::Cursor("502".into()))), 503);
    }

    #[test]
    fn no_cursor_starts_from_id_zero_and_filters_by_since() {
        assert_eq!(from_id(None), 0);
        let body = serde_json::json!([
            { "symbol": "X", "id": 1, "orderId": 1, "price": "1", "qty": "1", "isBuyer": true, "time": 1000i64 },
            { "symbol": "X", "id": 2, "orderId": 1, "price": "1", "qty": "1", "isBuyer": true, "time": 3000i64 },
        ]);
        let page = trades_page(&body, "a", 1000, Utc.timestamp_millis_opt(2000).unwrap());
        assert_eq!(page.fills.iter().map(|f| f.exec_id.as_str()).collect::<Vec<_>>(), ["X:2:B"]);
        assert_eq!(page.next.0, "2");
        // Only old trades: none booked, but the cursor still moves past them.
        let page = trades_page(&body, "a", 1000, Utc.timestamp_millis_opt(9000).unwrap());
        assert!(page.fills.is_empty());
        assert_eq!(page.next.0, "2");
    }

    #[test]
    fn empty_symbol_costs_one_call_and_leaves_cursor_empty() {
        let page = trades_page(&serde_json::json!([]), "a", 1000, Utc::now());
        assert_eq!(page, PullPage { fills: vec![], next: crate::venue_event::Cursor(String::new()), more: false });
    }

    #[test]
    fn since_filter_applies_on_every_page_of_an_anchored_scan() {
        // A two-page history (limit 2), every row older than `since` except id 4.
        let row = |id: i64, ms: i64| serde_json::json!({ "symbol": "X", "id": id, "orderId": 1, "price": "1", "qty": "1", "isBuyer": true, "time": ms });
        let history = [row(1, 100), row(2, 200), row(3, 300), row(4, 5000)];
        let since = Utc.timestamp_millis_opt(1000).unwrap();
        let mut from: Option<crate::venue_event::Cursor> = None;
        let mut booked = vec![];
        loop {
            let start = from_id(from.as_ref()) as usize;
            let body = serde_json::Value::Array(history.iter().filter(|r| r["id"].as_i64().unwrap() as usize >= start).take(2).cloned().collect());
            let page = trades_page(&body, "a", 2, since);
            booked.extend(page.fills.iter().map(|f| f.exec_id.clone()));
            if !page.more {
                break;
            }
            from = Some(page.next);
        }
        assert_eq!(booked, ["X:4:B"], "old rows on page 2 are dropped too");
    }

    #[test]
    fn open_orders_snapshot_maps_to_working_updates() {
        let body = serde_json::json!([
            { "symbol": "BTCUSDT", "orderId": 9001, "clientOrderId": "mm1", "side": "SELL", "type": "LIMIT", "origQty": "1.5", "price": "64000", "executedQty": "0.5", "time": 1700000000000i64 },
        ]);
        let snap = parse_open_order_snapshot(&body, "acct-1");
        assert_eq!(snap.len(), 1);
        let o = &snap[0];
        assert_eq!((o.venue_order_id.as_str(), o.client_order_id.as_deref(), o.account_ref.as_str(), o.buy), ("9001", Some("mm1"), "acct-1", false));
        assert_eq!((o.qty, o.limit_px, o.cum_qty, o.status.clone()), (1.5, Some(64000.0), 0.5, crate::venue_event::VenueOrderStatus::Working));
    }

    #[test]
    fn scopes_are_open_order_symbols_and_hints_are_per_symbol() {
        let body = serde_json::json!([
            { "symbol": "ETHUSDT", "orderId": 1, "clientOrderId": "a", "side": "BUY", "type": "LIMIT", "origQty": "1", "price": "1", "executedQty": "0" },
            { "symbol": "BTCUSDT", "orderId": 2, "clientOrderId": "b", "side": "BUY", "type": "LIMIT", "origQty": "1", "price": "1", "executedQty": "0" },
            { "symbol": "ETHUSDT", "orderId": 3, "clientOrderId": "c", "side": "BUY", "type": "LIMIT", "origQty": "1", "price": "1", "executedQty": "0" },
        ]);
        assert_eq!(open_order_symbols(&parse_open_order_snapshot(&body, "")), vec!["BTCUSDT".to_string(), "ETHUSDT".to_string()]);
        let a = BinanceAdapter::new("k".into(), "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIAGvAMHEDQzczRRSN2T3AYeEUyaWSwA6GXlSZ2hjM8TC\n-----END PRIVATE KEY-----\n", "PAPER").unwrap();
        assert_eq!(crate::venue_event::ExecutionSource::hints(&a), crate::venue_event::PullHints { intraday_secs: 60, max_history: None, per_symbol: true });
    }

    #[test]
    fn two_symbols_sharing_a_trade_id_both_book() {
        // Binance trade ids are per symbol: id 501 on BTCUSDT and on ETHUSDT are two fills.
        let body = serde_json::json!([
            { "symbol": "BTCUSDT", "id": 501, "orderId": 1, "price": "1", "qty": "1", "isBuyer": true },
            { "symbol": "ETHUSDT", "id": 501, "orderId": 2, "price": "1", "qty": "1", "isBuyer": true },
        ]);
        let page = trades_page(&body, "a", 1000, DateTime::<Utc>::MIN_UTC);
        let ids: std::collections::HashSet<_> = page.fills.iter().map(|f| f.exec_id.clone()).collect();
        assert_eq!(ids.len(), 2, "{ids:?}");
        assert_eq!(page.next.0, "501", "the cursor stays the raw trade id");
    }

    #[test]
    fn self_trade_both_sides_book() {
        // A self-trade: both sides carry the same trade id on one account.
        let body = serde_json::json!([
            { "symbol": "BTCUSDT", "id": 7, "orderId": 1, "price": "1", "qty": "1", "isBuyer": true },
            { "symbol": "BTCUSDT", "id": 7, "orderId": 2, "price": "1", "qty": "1", "isBuyer": false },
        ]);
        let fills = parse_my_trades(&body, "a");
        assert_ne!(fills[0].exec_id, fills[1].exec_id);
    }

    #[test]
    fn my_trades_parse_into_venue_fills() {
        let body = serde_json::json!([
            { "symbol": "BTCUSDT", "id": 501, "orderId": 9001, "price": "64000.5", "qty": "0.25", "isBuyer": true, "time": 1700000000000i64, "commission": "0.01", "commissionAsset": "BNB" },
            { "symbol": "BTCUSDT", "id": 502, "orderId": 9003, "price": "64010", "qty": "0.1", "isBuyer": false },
        ]);
        let fills = parse_my_trades(&body, "acct-1");
        assert_eq!(fills.len(), 2);
        assert_eq!((fills[0].exec_id.as_str(), fills[0].venue_order_id.as_str(), fills[0].buy, fills[0].qty), ("BTCUSDT:501:B", "9001", true, 0.25));
        assert!(!fills[1].buy);
        assert_eq!(fills[0].fee, Some((0.01, "BNB".into())));
        assert_eq!(fills[0].account_ref, "acct-1");
        assert!(fills[0].executed_at.is_some());
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
        assert!(matches!(cancel_error(r#"{"code":-2011,"msg":"Unknown order sent."}"#.into()), BrokerError::NoSuchOrder(_)));
        assert!(matches!(cancel_error(r#"{"code":-1021,"msg":"Timestamp outside recvWindow."}"#.into()), BrokerError::BrokerRejected(_)));
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

    #[test]
    fn binance_snapshot_order_carries_created_and_update_time() {
        let body = serde_json::json!([{
            "orderId": 7, "clientOrderId": "c", "symbol": "BTCUSDT", "side": "BUY", "type": "LIMIT",
            "price": "1", "origQty": "1", "executedQty": "0", "time": 1700000001000i64, "updateTime": 1700000007000i64,
        }]);
        let o = &parse_open_order_snapshot(&body, "a")[0];
        assert_eq!(o.created_at, Utc.timestamp_millis_opt(1700000001000).single());
        assert_eq!(o.ts, Utc.timestamp_millis_opt(1700000007000).single());
    }
}
