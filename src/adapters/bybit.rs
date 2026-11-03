//! Bybit spot broker adapter (v5 API, testnet by default).
//!
//! Auth: every private REST call carries `X-BAPI-API-KEY`, `X-BAPI-TIMESTAMP`
//! (ms), `X-BAPI-RECV-WINDOW` and `X-BAPI-SIGN`, an HMAC-SHA256 (lowercase hex,
//! keyed by the API secret) over `timestamp + api_key + recv_window` followed by
//! the exact query string (GET) or JSON body (POST). The private WebSocket signs
//! `GET/realtime{expires}` with the same key (`bybit_stream.rs`).
//!
//! Every v5 response is an envelope `{"retCode":0,"retMsg":"OK","result":{..}}`,
//! and a failure usually still comes back as HTTP 200 with a non-zero `retCode`,
//! so [`envelope`] is the one place a response is judged.
//!
//! Symbol mapping: Bybit pairs are their own instruments, venue `BYBIT` (not the
//! `BINANCE` rows — separate markets, see `mark_router.rs`). The pair (e.g.
//! `BTCUSDT`) is the routing symbol; the base coin (e.g. `BTC`) is the native id.

use dataprovider::{Identifiers, InstrumentDef};
use hmac::{Hmac, Mac};
use reqwest::Client;
use sha2::Sha256;

use super::{BrokerError, BrokerInstrument, InstrumentProvider};

/// Bybit's default; how long after `X-BAPI-TIMESTAMP` the request stays valid.
const RECV_WINDOW: &str = "5000";

#[derive(Clone)]
pub struct BybitAdapter {
    client: Client,
    api_key: String,
    api_secret: String,
    base_url: &'static str,
    private_ws_url: &'static str,
}

impl BybitAdapter {
    pub fn new(api_key: String, api_secret: String, environment: &str) -> Self {
        let (base_url, private_ws_url) = if environment == "LIVE" {
            ("https://api.bybit.com", "wss://stream.bybit.com/v5/private")
        } else {
            ("https://api-testnet.bybit.com", "wss://stream-testnet.bybit.com/v5/private")
        };
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        Self { client, api_key, api_secret, base_url, private_ws_url }
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    pub fn private_ws_url(&self) -> &'static str {
        self.private_ws_url
    }

    /// HMAC-SHA256 of `payload` under the API secret, lowercase hex.
    pub fn sign(&self, payload: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.api_secret.as_bytes()).expect("HMAC takes any key length");
        mac.update(payload.as_bytes());
        mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
    }

    fn rest_signature(&self, ts: i64, payload: &str) -> String {
        self.sign(&format!("{ts}{}{RECV_WINDOW}{payload}", self.api_key))
    }

    fn signed_headers(&self, req: reqwest::RequestBuilder, payload: &str) -> reqwest::RequestBuilder {
        let ts = chrono::Utc::now().timestamp_millis();
        req.header("X-BAPI-API-KEY", &self.api_key)
            .header("X-BAPI-TIMESTAMP", ts.to_string())
            .header("X-BAPI-RECV-WINDOW", RECV_WINDOW)
            .header("X-BAPI-SIGN", self.rest_signature(ts, payload))
    }

    /// GET `path?query`, signed for private endpoints; returns the envelope's `result`.
    async fn get(&self, path: &str, query: &str, signed: bool) -> Result<serde_json::Value, BrokerError> {
        let url = if query.is_empty() { format!("{}{path}", self.base_url) } else { format!("{}{path}?{query}", self.base_url) };
        let mut req = self.client.get(&url);
        if signed {
            req = self.signed_headers(req, query);
        }
        read(req.send().await).await
    }

    /// Signed POST of a JSON body; returns the envelope's `result`.
    async fn post(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value, BrokerError> {
        let body = body.to_string();
        let req = self
            .client
            .post(format!("{}{path}", self.base_url))
            .header("Content-Type", "application/json");
        read(self.signed_headers(req, &body).body(body).send().await).await
    }

    /// GET /v5/market/instruments-info?category=spot — public. Spot is not paged.
    pub async fn list_spot_instruments(&self) -> Result<Vec<BrokerInstrument>, BrokerError> {
        let result = self.get("/v5/market/instruments-info", "category=spot", false).await?;
        Ok(parse_spot_instruments(&result))
    }

    /// GET /v5/user/query-api — the cheapest authenticated read, used by the
    /// credentials Test button; works for unified and classic accounts alike.
    pub async fn api_key_info(&self) -> Result<serde_json::Value, BrokerError> {
        self.get("/v5/user/query-api", "", true).await
    }
}

/// Turn a sent request into the envelope's `result`, whatever the HTTP status.
async fn read(resp: Result<reqwest::Response, reqwest::Error>) -> Result<serde_json::Value, BrokerError> {
    let resp = resp.map_err(|e| BrokerError::Network(e.to_string()))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| BrokerError::Network(e.to_string()))?;
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(body) => envelope(body),
        Err(_) => Err(BrokerError::BrokerRejected(format!("HTTP {status}: {text}"))),
    }
}

/// The envelope's `result` when `retCode` is 0, else a rejection carrying `retMsg`.
pub(crate) fn envelope(body: serde_json::Value) -> Result<serde_json::Value, BrokerError> {
    match body["retCode"].as_i64() {
        Some(0) => Ok(body["result"].clone()),
        code => Err(BrokerError::BrokerRejected(format!(
            "{} (retCode {})",
            body["retMsg"].as_str().unwrap_or("no message"),
            code.map_or("missing".to_string(), |c| c.to_string())
        ))),
    }
}

/// A decimal string field as f64.
fn num(v: &serde_json::Value) -> Option<f64> {
    v.as_str().and_then(|s| s.parse().ok())
}

/// Digits after the point of a step like "0.01" (2) or "1" (0).
fn decimals(step: &str) -> i32 {
    step.split_once('.').map_or(0, |(_, frac)| frac.trim_end_matches('0').len() as i32)
}

/// The spot catalog from an instruments-info `result`. Every pair is listed;
/// only `Trading` ones are tradeable.
pub(crate) fn parse_spot_instruments(result: &serde_json::Value) -> Vec<BrokerInstrument> {
    let list = result["list"].as_array().cloned().unwrap_or_default();
    list.iter()
        .filter_map(|s| {
            let pair = s["symbol"].as_str()?.to_string();
            let base = s["baseCoin"].as_str()?.to_string();
            let quote = s["quoteCoin"].as_str()?.to_string();
            let lot = &s["lotSizeFilter"];
            let tick = s["priceFilter"]["tickSize"].as_str().unwrap_or("0");
            let definition = InstrumentDef {
                symbol: pair.clone(),
                venue: "BYBIT".to_string(),
                currency: quote,
                asset_class: "CRYPTO".to_string(),
                instrument_class: "SPOT".to_string(),
                name: None,
                price_precision: decimals(tick),
                price_increment: tick.parse().unwrap_or(0.0),
                size_increment: num(&lot["basePrecision"]).unwrap_or(0.0),
                lot_size: None,
                contract_size: 1.0,
                native_id: Some(base),
                provider_exchange: Some("BYBIT".to_string()),
                derivative: None,
                identifiers: Identifiers::default(),
            };
            Some(BrokerInstrument {
                broker_symbol: pair,
                broker_exchange: Some("BYBIT".to_string()),
                is_tradeable: s["status"].as_str() == Some("Trading"),
                min_quantity: num(&lot["minOrderQty"]),
                max_quantity: num(&lot["maxOrderQty"]),
                min_notional: num(&lot["minOrderAmt"]),
                max_notional: num(&lot["maxOrderAmt"]),
                definition,
            })
        })
        .collect()
}

#[async_trait::async_trait]
impl InstrumentProvider for BybitAdapter {
    /// Spot only: `option_underlyings` is ignored.
    async fn list_instruments(&self, _option_underlyings: &[String]) -> Result<Vec<BrokerInstrument>, BrokerError> {
        self.list_spot_instruments().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_matches_a_known_answer() {
        // `printf '%s' 1700000000000key5000category=spot | openssl dgst -sha256 -hmac secret`
        let a = BybitAdapter::new("key".into(), "secret".into(), "PAPER");
        assert_eq!(
            a.rest_signature(1700000000000, "category=spot"),
            "17c2acfe0756d6851d1c5926859c172d8922e199dc9ad0f0b47bce6e00fd1d5e"
        );
    }

    #[test]
    fn envelope_ok_returns_result() {
        let r = envelope(serde_json::json!({"retCode":0,"retMsg":"OK","result":{"x":1}})).unwrap();
        assert_eq!(r["x"], 1);
    }

    #[test]
    fn envelope_nonzero_retcode_is_a_rejection_with_the_message() {
        let e = envelope(serde_json::json!({"retCode":170131,"retMsg":"Insufficient balance.","result":{}}))
            .unwrap_err();
        assert!(matches!(e, BrokerError::BrokerRejected(ref m) if m.contains("Insufficient balance")), "{e}");
    }

    #[test]
    fn spot_instruments_are_bybit_venue_rows_with_lot_and_tick_sizes() {
        let sample: serde_json::Value =
            serde_json::from_str(include_str!("testdata/bybit/instruments_spot.json")).unwrap();
        let list = parse_spot_instruments(&sample["result"]);
        let btc = list.iter().find(|i| i.broker_symbol == "BTCUSDT").unwrap();
        assert_eq!(btc.definition.venue, "BYBIT");
        assert_eq!(btc.definition.currency, "USDT");
        assert_eq!(btc.definition.native_id.as_deref(), Some("BTC"));
        assert_eq!(btc.definition.instrument_class, "SPOT");
        assert_eq!(btc.definition.price_increment, 0.1);
        assert_eq!(btc.definition.price_precision, 1);
        assert_eq!(btc.definition.size_increment, 0.000001);
        assert_eq!(btc.min_quantity, Some(0.000001));
        assert_eq!(btc.max_quantity, Some(10.0));
        assert_eq!(btc.min_notional, Some(5.0));
        assert!(btc.is_tradeable);
        // A pair whose status is not "Trading" is listed but not tradeable.
        assert!(!list.iter().find(|i| i.broker_symbol == "HALTUSDT").unwrap().is_tradeable);
    }
}
