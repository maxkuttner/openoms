//! Coinbase Advanced Trade spot adapter (LIVE only — Coinbase has no paper
//! trading; its "sandbox" returns canned responses).
//!
//! Auth: every private REST call carries `Authorization: Bearer <jwt>`, a fresh
//! ES256 (P-256) token per request signed with the CDP key's private key:
//! header `{alg: ES256, typ: JWT, kid: <key name>, nonce}`, claims
//! `{iss: "cdp", sub: <key name>, nbf, exp: nbf + 120, uri: "<METHOD> host<path>"}`.
//! The user WebSocket takes the same token without `uri` in its subscribe message.
//!
//! Symbol mapping: Coinbase products are their own instruments, venue `COINBASE`,
//! symbol = `product_id` (`BTC-USD`); the base currency is the native id.

use base64::Engine;
use dataprovider::{Identifiers, InstrumentDef};
use p256::ecdsa::{signature::Signer, Signature, SigningKey};
use rand::RngCore;
use reqwest::Client;

use tracing::info;

use super::{BrokerAdapter, BrokerError, BrokerInstrument, BrokerOrderRequest, BrokerOrderResponse, InstrumentProvider};
use crate::recon_orders::{BrokerOpenOrder, BrokerOrderOutcome, BrokerOrderState};

/// Quantities and prices as plain decimals without float noise — Bybit's rule.
pub(crate) use super::bybit::decimal;

const HOST: &str = "api.coinbase.com";
const BASE: &str = "/api/v3/brokerage";
/// Coinbase rejects a token older than two minutes.
const JWT_TTL_SECS: i64 = 120;

/// A Coinbase CDP API key: its name (`organizations/…/apiKeys/…`) and the EC
/// private key PEM that signs each request's JWT.
/// Stored as `{"kind":"Coinbase", …}` (see [`crate::credentials::BrokerCredentials`]).
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct CoinbaseCredentials {
    pub api_key: String,
    pub private_key: String,
}

impl CoinbaseCredentials {
    pub const KIND: &'static str = "Coinbase";
}

#[derive(Clone)]
pub struct CoinbaseAdapter {
    client: Client,
    key_name: String,
    signing_key: SigningKey,
}

/// `"<METHOD> api.coinbase.com<path>"`, the `uri` claim; the query string is not
/// part of it.
pub(crate) fn rest_uri(method: &str, path: &str) -> String {
    let path = path.split_once('?').map_or(path, |(p, _)| p);
    format!("{method} {HOST}{path}")
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

impl CoinbaseAdapter {
    /// `private_key_pem` is the CDP key's EC private key — SEC1
    /// (`BEGIN EC PRIVATE KEY`, how Coinbase hands it out) or PKCS#8. A key copied
    /// out of Coinbase's JSON download keeps its `\n` escapes; those are accepted.
    pub fn new(key_name: String, private_key_pem: &str) -> Result<Self, String> {
        let pem = private_key_pem.trim().replace("\\n", "\n");
        let secret = p256::SecretKey::from_sec1_pem(&pem)
            .or_else(|_| {
                use p256::pkcs8::DecodePrivateKey;
                p256::SecretKey::from_pkcs8_pem(&pem)
            })
            .map_err(|_| "invalid Coinbase private key: expected an EC (P-256) PEM; Ed25519 keys are not supported".to_string())?;
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        Ok(Self { client, key_name, signing_key: SigningKey::from(&secret) })
    }

    /// A fresh ES256 token; `uri` for REST calls, `None` for the WebSocket.
    pub fn jwt(&self, uri: Option<&str>) -> String {
        let mut nonce = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut nonce);
        let nonce: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
        let header = serde_json::json!({ "alg": "ES256", "typ": "JWT", "kid": self.key_name, "nonce": nonce });
        let now = chrono::Utc::now().timestamp();
        let mut claims = serde_json::json!({ "iss": "cdp", "sub": self.key_name, "nbf": now, "exp": now + JWT_TTL_SECS });
        if let Some(uri) = uri {
            claims["uri"] = uri.into();
        }
        let signed = format!("{}.{}", b64(header.to_string().as_bytes()), b64(claims.to_string().as_bytes()));
        let sig: Signature = self.signing_key.sign(signed.as_bytes());
        format!("{signed}.{}", b64(&sig.to_bytes()))
    }

    /// GET `BASE + path` (+ `?query`); signed for private endpoints.
    async fn get(&self, path: &str, query: &str, signed: bool) -> Result<serde_json::Value, BrokerError> {
        let full = format!("{BASE}{path}");
        let url = if query.is_empty() { format!("https://{HOST}{full}") } else { format!("https://{HOST}{full}?{query}") };
        let mut req = self.client.get(&url);
        if signed {
            req = req.bearer_auth(self.jwt(Some(&rest_uri("GET", &full))));
        }
        read(req.send().await).await
    }

    /// Signed POST of a JSON body to `BASE + path`.
    async fn post(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value, BrokerError> {
        let full = format!("{BASE}{path}");
        let req = self
            .client
            .post(format!("https://{HOST}{full}"))
            .bearer_auth(self.jwt(Some(&rest_uri("POST", &full))))
            .json(body);
        read(req.send().await).await
    }

    /// GET /market/products?product_type=SPOT — public, every spot product in one call.
    pub async fn list_spot_products(&self) -> Result<Vec<BrokerInstrument>, BrokerError> {
        let body = self.get("/market/products", "product_type=SPOT", false).await?;
        Ok(parse_spot_products(&body))
    }

    /// GET /key_permissions — what the credentials Test judges (trade, no transfer).
    pub async fn key_permissions(&self) -> Result<serde_json::Value, BrokerError> {
        self.get("/key_permissions", "", true).await
    }
}

/// The JSON body of a response, or a rejection carrying Coinbase's message for a
/// non-2xx status.
async fn read(resp: Result<reqwest::Response, reqwest::Error>) -> Result<serde_json::Value, BrokerError> {
    let resp = resp.map_err(|e| BrokerError::Network(e.to_string()))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| BrokerError::Network(e.to_string()))?;
    let body: Option<serde_json::Value> = serde_json::from_str(&text).ok();
    if status.is_success() {
        return body.ok_or_else(|| BrokerError::BrokerRejected(format!("HTTP {status}: not JSON: {text}")));
    }
    let message = body
        .as_ref()
        .and_then(|b| b["message"].as_str().or(b["error"].as_str()).map(String::from))
        .unwrap_or(text);
    Err(BrokerError::BrokerRejected(format!("HTTP {status}: {message}")))
}

/// A decimal string field as f64.
fn num(v: &serde_json::Value) -> Option<f64> {
    v.as_str().and_then(|s| s.parse().ok())
}

/// Digits after the point of a step like "0.01" (2) or "1" (0).
fn decimals(step: &str) -> i32 {
    step.split_once('.').map_or(0, |(_, frac)| frac.trim_end_matches('0').len() as i32)
}

/// Spot products only; every one listed, tradeable only when online and not
/// disabled, cancel-only or view-only.
pub(crate) fn parse_spot_products(body: &serde_json::Value) -> Vec<BrokerInstrument> {
    let list = body["products"].as_array().cloned().unwrap_or_default();
    list.iter()
        .filter(|p| p["product_type"].as_str() == Some("SPOT"))
        .filter_map(|p| {
            let symbol = p["product_id"].as_str()?.to_string();
            let tick = p["price_increment"].as_str().or(p["quote_increment"].as_str()).unwrap_or("0");
            let flag = |k: &str| p[k].as_bool().unwrap_or(false);
            let tradeable = p["status"].as_str() == Some("online")
                && !flag("trading_disabled")
                && !flag("is_disabled")
                && !flag("cancel_only")
                && !flag("view_only");
            let definition = InstrumentDef {
                symbol: symbol.clone(),
                venue: "COINBASE".to_string(),
                currency: p["quote_currency_id"].as_str()?.to_string(),
                asset_class: "CRYPTO".to_string(),
                instrument_class: "SPOT".to_string(),
                name: p["base_name"].as_str().map(String::from),
                price_precision: decimals(tick),
                price_increment: tick.parse().unwrap_or(0.0),
                size_increment: num(&p["base_increment"]).unwrap_or(0.0),
                lot_size: None,
                contract_size: 1.0,
                native_id: Some(p["base_currency_id"].as_str()?.to_string()),
                provider_exchange: Some("COINBASE".to_string()),
                derivative: None,
                identifiers: Identifiers::default(),
            };
            Some(BrokerInstrument {
                broker_symbol: symbol,
                broker_exchange: Some("COINBASE".to_string()),
                is_tradeable: tradeable,
                min_quantity: num(&p["base_min_size"]),
                max_quantity: num(&p["base_max_size"]),
                min_notional: num(&p["quote_min_size"]),
                max_notional: num(&p["quote_max_size"]),
                definition,
            })
        })
        .collect()
}

/// The POST /orders body for an OMS order. Quantities are always in the base coin.
pub(crate) fn order_body(req: &BrokerOrderRequest) -> Result<serde_json::Value, BrokerError> {
    let side = if req.side.eq_ignore_ascii_case("sell") { "SELL" } else { "BUY" };
    let size = decimal(req.quantity);
    let configuration = match req.order_type.as_str() {
        "market" => serde_json::json!({ "market_market_ioc": { "base_size": size } }),
        "limit" => {
            let price = decimal(
                req.limit_price
                    .ok_or_else(|| BrokerError::BrokerRejected("limit order missing limit_price".into()))?,
            );
            // Day is meaningless on a 24/7 venue — treat as GTC.
            match req.time_in_force.to_uppercase().as_str() {
                "IOC" => serde_json::json!({ "sor_limit_ioc": { "base_size": size, "limit_price": price } }),
                "FOK" => serde_json::json!({ "limit_limit_fok": { "base_size": size, "limit_price": price } }),
                _ => serde_json::json!({ "limit_limit_gtc": { "base_size": size, "limit_price": price, "post_only": false } }),
            }
        }
        other => return Err(BrokerError::NotConfigured(format!("unsupported order type: {other}"))),
    };
    Ok(serde_json::json!({
        // Our order UUID, echoed on every user-channel update.
        "client_order_id": req.order_id,
        "product_id": req.symbol,
        "side": side,
        "order_configuration": configuration,
    }))
}

/// Coinbase answers a refused order with `success: false` (HTTP 200).
pub(crate) fn parse_create(body: &serde_json::Value) -> Result<String, BrokerError> {
    if body["success"].as_bool() == Some(true) {
        if let Some(id) = body["success_response"]["order_id"].as_str().filter(|s| !s.is_empty()) {
            return Ok(id.to_string());
        }
    }
    let e = &body["error_response"];
    let reason = e["new_order_failure_reason"].as_str().or(e["error"].as_str()).unwrap_or("UNKNOWN");
    let message = e["message"].as_str().unwrap_or("order not accepted");
    Err(BrokerError::BrokerRejected(format!("{message} ({reason})")))
}

pub(crate) fn parse_cancel(body: &serde_json::Value) -> Result<(), BrokerError> {
    let first = &body["results"][0];
    if first["success"].as_bool() == Some(true) {
        Ok(())
    } else {
        Err(BrokerError::BrokerRejected(format!(
            "cancel refused ({})",
            first["failure_reason"].as_str().unwrap_or("no reason given")
        )))
    }
}

/// Coinbase's order `status` as a reconcile outcome. EXPIRED (a GTD/IOC
/// remainder) is a cancel; FAILED is a reject.
pub(crate) fn order_outcome(status: &str) -> BrokerOrderOutcome {
    match status {
        "FILLED" => BrokerOrderOutcome::Filled,
        "CANCELLED" | "EXPIRED" => BrokerOrderOutcome::Canceled,
        "FAILED" => BrokerOrderOutcome::Rejected,
        _ => BrokerOrderOutcome::Working, // PENDING / OPEN / CANCEL_QUEUED / QUEUED
    }
}

pub(crate) fn parse_order_state(order: &serde_json::Value) -> BrokerOrderState {
    BrokerOrderState {
        outcome: order_outcome(order["status"].as_str().unwrap_or("")),
        executed_qty: num(&order["filled_size"]).unwrap_or(0.0),
        avg_px: num(&order["average_filled_price"]).unwrap_or(0.0),
    }
}

pub(crate) fn parse_open_orders(body: &serde_json::Value) -> Vec<BrokerOpenOrder> {
    let list = body["orders"].as_array().cloned().unwrap_or_default();
    list.iter()
        .filter_map(|o| {
            Some(BrokerOpenOrder {
                client_order_id: o["client_order_id"].as_str()?.to_string(),
                symbol: o["product_id"].as_str()?.to_string(),
                executed_qty: num(&o["filled_size"]).unwrap_or(0.0),
            })
        })
        .collect()
}

/// Previous close per wanted product, from the price and its 24h change:
/// `price / (1 + change% / 100)`. A product missing either is skipped, never
/// reported as a zero previous close.
pub(crate) fn parse_prev_prices(body: &serde_json::Value, wanted: &[String]) -> Vec<(String, f64)> {
    let list = body["products"].as_array().cloned().unwrap_or_default();
    list.iter()
        .filter_map(|p| {
            let id = p["product_id"].as_str()?;
            if !wanted.iter().any(|w| w == id) {
                return None;
            }
            let price = num(&p["price"])?;
            let change = num(&p["price_percentage_change_24h"])?;
            let prev = price / (1.0 + change / 100.0);
            prev.is_finite().then(|| (id.to_string(), prev))
        })
        .collect()
}

#[async_trait::async_trait]
impl BrokerAdapter for CoinbaseAdapter {
    async fn submit_order(&self, req: &BrokerOrderRequest) -> Result<BrokerOrderResponse, BrokerError> {
        let body = order_body(req)?;
        info!(symbol = %req.symbol, side = %req.side, "submitting order to Coinbase");
        let external_order_id = parse_create(&self.post("/orders", &body).await?)?;
        Ok(BrokerOrderResponse { external_order_id })
    }

    async fn cancel_order(&self, external_order_id: &str, _symbol: &str) -> Result<(), BrokerError> {
        let body = serde_json::json!({ "order_ids": [external_order_id] });
        parse_cancel(&self.post("/orders/batch_cancel", &body).await?)
    }

    /// GET /orders/historical/batch?order_status=OPEN, following the cursor.
    async fn open_orders(&self) -> Result<Vec<BrokerOpenOrder>, BrokerError> {
        let mut out = Vec::new();
        let mut cursor = String::new();
        loop {
            let mut query = "order_status=OPEN&limit=250".to_string();
            if !cursor.is_empty() {
                query.push_str(&format!("&cursor={}", urlencoding_minimal(&cursor)));
            }
            let body = self.get("/orders/historical/batch", &query, true).await?;
            out.extend(parse_open_orders(&body));
            cursor = body["cursor"].as_str().unwrap_or("").to_string();
            if body["has_next"].as_bool() != Some(true) || cursor.is_empty() {
                return Ok(out);
            }
        }
    }

    async fn order_status(
        &self,
        _client_order_id: &str,
        external_order_id: &str,
        _symbol: &str,
    ) -> Result<BrokerOrderState, BrokerError> {
        let body = self.get(&format!("/orders/historical/{external_order_id}"), "", true).await?;
        Ok(parse_order_state(&body["order"]))
    }

    /// Public GET /market/products?product_type=SPOT — one call covers every product.
    async fn daily_stats(&self, symbols: &[String]) -> Result<Vec<(String, f64)>, BrokerError> {
        if symbols.is_empty() {
            return Ok(Vec::new());
        }
        let body = self.get("/market/products", "product_type=SPOT", false).await?;
        Ok(parse_prev_prices(&body, symbols))
    }
}

/// Percent-encode a page cursor for a query string (alphanumerics and `-_.~` pass).
fn urlencoding_minimal(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[async_trait::async_trait]
impl InstrumentProvider for CoinbaseAdapter {
    /// Spot only: `option_underlyings` is ignored.
    async fn list_instruments(&self, _option_underlyings: &[String]) -> Result<Vec<BrokerInstrument>, BrokerError> {
        self.list_spot_products().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use p256::ecdsa::{signature::Verifier, Signature, VerifyingKey};
    use p256::pkcs8::EncodePrivateKey;

    const KEY_NAME: &str = "organizations/org-1/apiKeys/key-1";
    const ORDER_ID: &str = "3f6b1c2e-8a4d-4e5f-9b21-1c2d3e4f5a6b";

    fn req(order_type: &str, side: &str, quantity: f64, limit_price: Option<f64>, tif: &str) -> BrokerOrderRequest {
        BrokerOrderRequest {
            order_id: ORDER_ID.into(),
            symbol: "BTC-USD".into(),
            native_id: Some("BTC".into()),
            quantity,
            side: side.into(),
            order_type: order_type.into(),
            time_in_force: tif.into(),
            limit_price,
            external_account_ref: String::new(),
        }
    }

    fn sample(name: &str) -> serde_json::Value {
        let text = match name {
            "create_ok" => include_str!("testdata/coinbase/create_ok.json"),
            "create_failed" => include_str!("testdata/coinbase/create_failed.json"),
            "cancel_ok" => include_str!("testdata/coinbase/cancel_ok.json"),
            "cancel_failed" => include_str!("testdata/coinbase/cancel_failed.json"),
            "orders_open" => include_str!("testdata/coinbase/orders_open.json"),
            "order_filled" => include_str!("testdata/coinbase/order_filled.json"),
            other => panic!("no sample {other}"),
        };
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn decimal_drops_float_noise() {
        assert_eq!(decimal(0.1 + 0.2), "0.3");
        assert_eq!(decimal(0.0001), "0.0001");
        assert_eq!(decimal(65000.0), "65000");
    }

    #[test]
    fn market_orders_are_sized_in_the_base_coin() {
        let b = order_body(&req("market", "buy", 0.0001, None, "day")).unwrap();
        assert_eq!(b["client_order_id"], ORDER_ID);
        assert_eq!(b["product_id"], "BTC-USD");
        assert_eq!(b["side"], "BUY");
        assert_eq!(b["order_configuration"]["market_market_ioc"]["base_size"], "0.0001");
        assert_eq!(order_body(&req("market", "sell", 1.0, None, "day")).unwrap()["side"], "SELL");
    }

    #[test]
    fn limit_tif_maps_to_coinbase_configurations() {
        let gtc = order_body(&req("limit", "buy", 0.001, Some(30000.5), "day")).unwrap();
        let cfg = &gtc["order_configuration"]["limit_limit_gtc"];
        assert_eq!(cfg["base_size"], "0.001");
        assert_eq!(cfg["limit_price"], "30000.5");
        assert_eq!(cfg["post_only"], false);
        let ioc = order_body(&req("limit", "buy", 0.001, Some(1.0), "ioc")).unwrap();
        assert!(ioc["order_configuration"]["sor_limit_ioc"].is_object());
        let fok = order_body(&req("limit", "buy", 0.001, Some(1.0), "fok")).unwrap();
        assert!(fok["order_configuration"]["limit_limit_fok"].is_object());
        assert!(order_body(&req("limit", "buy", 1.0, None, "gtc")).is_err());
        assert!(order_body(&req("stop", "buy", 1.0, None, "gtc")).is_err());
    }

    #[test]
    fn create_ok_returns_the_order_id() {
        assert_eq!(parse_create(&sample("create_ok")).unwrap(), "11111111-2222-4333-8444-555555555555");
    }

    #[test]
    fn create_failure_is_a_rejection() {
        let e = parse_create(&sample("create_failed")).unwrap_err();
        assert!(matches!(e, BrokerError::BrokerRejected(ref m) if m.contains("INSUFFICIENT_FUND")), "{e}");
    }

    #[test]
    fn cancel_failure_is_a_rejection() {
        assert!(parse_cancel(&sample("cancel_ok")).is_ok());
        let e = parse_cancel(&sample("cancel_failed")).unwrap_err();
        assert!(e.to_string().contains("UNKNOWN_CANCEL_ORDER"), "{e}");
    }

    #[test]
    fn order_status_maps_coinbase_states() {
        assert_eq!(order_outcome("FILLED"), BrokerOrderOutcome::Filled);
        assert_eq!(order_outcome("CANCELLED"), BrokerOrderOutcome::Canceled);
        assert_eq!(order_outcome("EXPIRED"), BrokerOrderOutcome::Canceled);
        assert_eq!(order_outcome("FAILED"), BrokerOrderOutcome::Rejected);
        assert_eq!(order_outcome("OPEN"), BrokerOrderOutcome::Working);
        assert_eq!(order_outcome("PENDING"), BrokerOrderOutcome::Working);
        assert_eq!(order_outcome("CANCEL_QUEUED"), BrokerOrderOutcome::Working);
    }

    #[test]
    fn open_orders_and_order_state_read_cumulative_and_average() {
        let open = parse_open_orders(&sample("orders_open"));
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].client_order_id, ORDER_ID);
        assert_eq!(open[0].symbol, "BTC-USD");
        assert_eq!(open[0].executed_qty, 0.0005);
        let st = parse_order_state(&sample("order_filled")["order"]);
        assert_eq!(st.outcome, BrokerOrderOutcome::Filled);
        assert_eq!(st.executed_qty, 0.001);
        assert_eq!(st.avg_px, 65000.0);
    }

    #[test]
    fn previous_close_comes_from_the_24h_change() {
        let body = serde_json::json!({"products": [
            {"product_id": "BTC-USD", "price": "102", "price_percentage_change_24h": "2"},
            {"product_id": "ETH-EUR", "price": "50", "price_percentage_change_24h": ""},
        ]});
        let prev = parse_prev_prices(&body, &["BTC-USD".to_string(), "ETH-EUR".to_string()]);
        assert_eq!(prev.len(), 1);
        assert_eq!(prev[0].0, "BTC-USD");
        assert!((prev[0].1 - 100.0).abs() < 1e-9);
    }

    fn key_pair() -> (String, VerifyingKey) {
        let secret = p256::SecretKey::random(&mut rand::thread_rng());
        let pem = secret.to_sec1_pem(p256::pkcs8::LineEnding::LF).unwrap().to_string();
        let verifying = VerifyingKey::from(&p256::ecdsa::SigningKey::from(&secret));
        (pem, verifying)
    }

    fn part(token: &str, i: usize) -> serde_json::Value {
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(token.split('.').nth(i).unwrap()).unwrap();
        serde_json::from_slice(&raw).unwrap()
    }

    #[test]
    fn jwt_header_and_claims_follow_coinbase() {
        let (pem, _) = key_pair();
        let a = CoinbaseAdapter::new(KEY_NAME.into(), &pem).unwrap();
        let token = a.jwt(Some("GET api.coinbase.com/api/v3/brokerage/accounts"));
        let header = part(&token, 0);
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["typ"], "JWT");
        assert_eq!(header["kid"], KEY_NAME);
        assert_eq!(header["nonce"].as_str().unwrap().len(), 32);
        let claims = part(&token, 1);
        assert_eq!(claims["iss"], "cdp");
        assert_eq!(claims["sub"], KEY_NAME);
        assert_eq!(claims["exp"].as_i64().unwrap() - claims["nbf"].as_i64().unwrap(), 120);
        assert_eq!(claims["uri"], "GET api.coinbase.com/api/v3/brokerage/accounts");
    }

    #[test]
    fn jwt_signature_verifies_with_the_public_key() {
        let (pem, verifying) = key_pair();
        let token = CoinbaseAdapter::new(KEY_NAME.into(), &pem).unwrap().jwt(None);
        let (signed, sig) = token.rsplit_once('.').unwrap();
        let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(sig).unwrap();
        assert_eq!(sig.len(), 64, "ES256 is raw r||s");
        verifying.verify(signed.as_bytes(), &Signature::from_slice(&sig).unwrap()).unwrap();
    }

    #[test]
    fn websocket_jwt_has_no_uri() {
        let (pem, _) = key_pair();
        let token = CoinbaseAdapter::new(KEY_NAME.into(), &pem).unwrap().jwt(None);
        assert!(part(&token, 1).get("uri").is_none());
    }

    #[test]
    fn rest_uri_excludes_the_query() {
        assert_eq!(
            rest_uri("GET", "/api/v3/brokerage/orders/historical/batch?order_status=OPEN"),
            "GET api.coinbase.com/api/v3/brokerage/orders/historical/batch"
        );
    }

    #[test]
    fn a_pem_pasted_with_literal_newlines_is_accepted() {
        let (pem, _) = key_pair();
        let escaped = pem.trim_end().replace('\n', "\\n");
        assert!(CoinbaseAdapter::new(KEY_NAME.into(), &escaped).is_ok());
        // PKCS#8 works too.
        let pkcs8 = p256::SecretKey::random(&mut rand::thread_rng())
            .to_pkcs8_pem(p256::pkcs8::LineEnding::LF)
            .unwrap()
            .to_string();
        assert!(CoinbaseAdapter::new(KEY_NAME.into(), &pkcs8).is_ok());
    }

    #[test]
    fn a_bad_pem_is_refused() {
        assert!(CoinbaseAdapter::new(KEY_NAME.into(), "not a key").is_err());
    }

    #[test]
    fn spot_products_are_coinbase_venue_rows() {
        let body: serde_json::Value =
            serde_json::from_str(include_str!("testdata/coinbase/products_spot.json")).unwrap();
        let list = parse_spot_products(&body);
        let btc = list.iter().find(|i| i.broker_symbol == "BTC-USD").unwrap();
        assert_eq!(btc.definition.venue, "COINBASE");
        assert_eq!(btc.definition.currency, "USD");
        assert_eq!(btc.definition.native_id.as_deref(), Some("BTC"));
        assert_eq!(btc.definition.price_increment, 0.01);
        assert_eq!(btc.definition.price_precision, 2);
        assert_eq!(btc.definition.size_increment, 0.00000001);
        assert_eq!(btc.min_quantity, Some(0.00000001));
        assert_eq!(btc.max_quantity, Some(3400.0));
        assert_eq!(btc.min_notional, Some(1.0));
        assert!(btc.is_tradeable);
        assert!(!list.iter().find(|i| i.broker_symbol == "HALT-USD").unwrap().is_tradeable);
        assert!(list.iter().all(|i| i.broker_symbol != "BIT-28NOV26-CDE"), "futures are not loaded");
    }
}
