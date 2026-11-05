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

use tracing::info;

use crate::adapter::{BrokerAdapter, BrokerError, BrokerInstrument, BrokerOrderRequest, BrokerOrderResponse, InstrumentProvider};
use crate::md::daily_stats::DailyStats;
use crate::contract::{BrokerOpenOrder, BrokerOrderOutcome, BrokerOrderState};

/// Bybit's default; how long after `X-BAPI-TIMESTAMP` the request stays valid.
const RECV_WINDOW: &str = "5000";

/// A Bybit v5 HMAC key pair (System-generated API key).
/// Stored as `{"kind":"Bybit", …}` (see [`crate::credentials::BrokerCredentials`]).
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct BybitCredentials {
    pub api_key: String,
    pub api_secret: String,
}

impl BybitCredentials {
    pub const KIND: &'static str = "Bybit";
}

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

/// A quantity or price as Bybit wants it: plain decimal, no float noise
/// (`0.1 + 0.2` is "0.3"), no trailing zeros. Bybit rejects excess precision.
pub(crate) fn decimal(x: f64) -> String {
    let s = format!("{x:.10}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" { "0".to_string() } else { s.to_string() }
}

/// The POST /v5/order/create body for an OMS order.
pub(crate) fn order_body(req: &BrokerOrderRequest) -> Result<serde_json::Value, BrokerError> {
    let side = if req.side.eq_ignore_ascii_case("sell") { "Sell" } else { "Buy" };
    let mut body = serde_json::json!({
        "category": "spot",
        "symbol": req.symbol,
        "side": side,
        "qty": decimal(req.quantity),
        // Our order UUID (36 chars, Bybit's limit), echoed on every update.
        "orderLinkId": req.order_id,
    });
    match req.order_type.as_str() {
        // A spot market Buy's qty is in the quote coin unless told otherwise;
        // OMS quantities are always in the base coin.
        "market" => {
            body["orderType"] = "Market".into();
            body["marketUnit"] = "baseCoin".into();
        }
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
            body["orderType"] = "Limit".into();
            body["price"] = decimal(price).into();
            body["timeInForce"] = tif.into();
        }
        other => return Err(BrokerError::NotConfigured(format!("unsupported order type: {other}"))),
    }
    Ok(body)
}

/// Bybit's `orderStatus` as a reconcile outcome. `PartiallyFilledCanceled`
/// keeps its fills; `Deactivated` is an untriggered order the venue cancelled.
pub(crate) fn order_outcome(status: &str) -> BrokerOrderOutcome {
    match status {
        "Filled" => BrokerOrderOutcome::Filled,
        "Cancelled" | "PartiallyFilledCanceled" | "Deactivated" => BrokerOrderOutcome::Canceled,
        "Rejected" => BrokerOrderOutcome::Rejected,
        _ => BrokerOrderOutcome::Working, // New / PartiallyFilled / Untriggered — still live
    }
}

/// Order query: by Bybit's id when known, else by our client order id
/// (`orderLinkId`).
pub(crate) fn order_lookup_query(client_order_id: &str, external_order_id: &str) -> String {
    if external_order_id.is_empty() {
        format!("category=spot&orderLinkId={client_order_id}")
    } else {
        format!("category=spot&orderId={external_order_id}")
    }
}

pub(crate) fn parse_open_orders(result: &serde_json::Value) -> Vec<BrokerOpenOrder> {
    let list = result["list"].as_array().cloned().unwrap_or_default();
    list.iter()
        .filter_map(|o| {
            Some(BrokerOpenOrder {
                client_order_id: o["orderLinkId"].as_str()?.to_string(),
                venue_order_id: o["orderId"].as_str().unwrap_or_default().to_string(),
                symbol: o["symbol"].as_str()?.to_string(),
                executed_qty: num(&o["cumExecQty"]).unwrap_or(0.0),
            })
        })
        .collect()
}

pub(crate) fn parse_order_state(order: &serde_json::Value) -> BrokerOrderState {
    BrokerOrderState {
        outcome: order_outcome(order["orderStatus"].as_str().unwrap_or("")),
        executed_qty: num(&order["cumExecQty"]).unwrap_or(0.0),
        avg_px: num(&order["avgPrice"]).unwrap_or(0.0),
    }
}

/// `prevPrice24h` per wanted symbol from a tickers `result`; an unparseable price
/// is skipped, never reported as a zero previous close.
pub(crate) fn parse_prev_prices(result: &serde_json::Value, wanted: &[String]) -> Vec<(String, f64)> {
    let list = result["list"].as_array().cloned().unwrap_or_default();
    list.iter()
        .filter_map(|t| {
            let symbol = t["symbol"].as_str()?;
            if !wanted.iter().any(|w| w == symbol) {
                return None;
            }
            Some((symbol.to_string(), num(&t["prevPrice24h"])?))
        })
        .collect()
}

#[async_trait::async_trait]
impl BrokerAdapter for BybitAdapter {
    async fn submit_order(&self, req: &BrokerOrderRequest) -> Result<BrokerOrderResponse, BrokerError> {
        let body = order_body(req)?;
        info!(symbol = %req.symbol, side = %req.side, "submitting order to Bybit");
        let result = self.post("/v5/order/create", &body).await?;
        match result["orderId"].as_str() {
            Some(id) if !id.is_empty() => Ok(BrokerOrderResponse { external_order_id: id.to_string() }),
            _ => Err(BrokerError::BrokerRejected(format!("Bybit accepted the order but returned no orderId: {result}"))),
        }
    }

    async fn cancel_order(&self, external_order_id: &str, symbol: &str) -> Result<(), BrokerError> {
        let body = serde_json::json!({ "category": "spot", "symbol": symbol, "orderId": external_order_id });
        self.post("/v5/order/cancel", &body).await.map(|_| ())
    }

    /// GET /v5/order/realtime — every open spot order, following the page cursor.
    async fn open_orders(&self) -> Result<Vec<BrokerOpenOrder>, BrokerError> {
        let mut out = Vec::new();
        let mut cursor = String::new();
        loop {
            let mut query = "category=spot&limit=50&openOnly=0".to_string();
            if !cursor.is_empty() {
                query.push_str(&format!("&cursor={cursor}"));
            }
            let result = self.get("/v5/order/realtime", &query, true).await?;
            out.extend(parse_open_orders(&result));
            cursor = result["nextPageCursor"].as_str().unwrap_or("").to_string();
            if cursor.is_empty() {
                return Ok(out);
            }
        }
    }

    /// One order by Bybit id: realtime first (open and recently closed), then history.
    async fn order_status(
        &self,
        client_order_id: &str,
        external_order_id: &str,
        _symbol: &str,
    ) -> Result<BrokerOrderState, BrokerError> {
        let query = order_lookup_query(client_order_id, external_order_id);
        for path in ["/v5/order/realtime", "/v5/order/history"] {
            let result = self.get(path, &query, true).await?;
            if let Some(order) = result["list"].as_array().and_then(|l| l.first()) {
                return Ok(parse_order_state(order));
            }
        }
        Err(BrokerError::NoSuchOrder(format!("Bybit has no order {query}")))
    }
}

#[async_trait::async_trait]
impl DailyStats for BybitAdapter {
    /// GET /v5/market/tickers?category=spot — public; one call covers every pair.
    async fn daily_stats(&self, symbols: &[String]) -> Result<Vec<(String, f64)>, BrokerError> {
        if symbols.is_empty() {
            return Ok(Vec::new());
        }
        let result = self.get("/v5/market/tickers", "category=spot", false).await?;
        Ok(parse_prev_prices(&result, symbols))
    }
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
    fn open_orders_carry_the_venue_order_id() {
        let result = serde_json::json!({ "list": [{ "orderId": "v-1", "orderLinkId": "c-1", "symbol": "BTCUSDT", "cumExecQty": "0" }] });
        assert_eq!(parse_open_orders(&result)[0].venue_order_id, "v-1");
    }

    #[test]
    fn order_status_looks_up_by_client_id_without_a_venue_id() {
        assert_eq!(order_lookup_query("c-1", "v-1"), "category=spot&orderId=v-1");
        assert_eq!(order_lookup_query("c-1", ""), "category=spot&orderLinkId=c-1");
    }

    const ORDER_ID: &str = "3f6b1c2e-8a4d-4e5f-9b21-1c2d3e4f5a6b";

    fn req(order_type: &str, side: &str, quantity: f64, limit_price: Option<f64>, tif: &str) -> BrokerOrderRequest {
        BrokerOrderRequest {
            order_id: ORDER_ID.into(),
            symbol: "BTCUSDT".into(),
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
            "open_orders" => include_str!("testdata/open_orders.json"),
            "order_history" => include_str!("testdata/order_history.json"),
            "tickers_spot" => include_str!("testdata/tickers_spot.json"),
            other => panic!("no sample {other}"),
        };
        envelope(serde_json::from_str(text).unwrap()).unwrap()
    }

    #[test]
    fn decimal_drops_float_noise_and_trailing_zeros() {
        assert_eq!(decimal(0.1 + 0.2), "0.3");
        assert_eq!(decimal(0.00001), "0.00001");
        assert_eq!(decimal(2.0), "2");
        assert_eq!(decimal(65000.5), "65000.5");
    }

    #[test]
    fn market_buy_is_sized_in_the_base_coin() {
        let b = order_body(&req("market", "buy", 0.001, None, "day")).unwrap();
        assert_eq!(b["category"], "spot");
        assert_eq!(b["symbol"], "BTCUSDT");
        assert_eq!(b["side"], "Buy");
        assert_eq!(b["orderType"], "Market");
        assert_eq!(b["qty"], "0.001");
        assert_eq!(b["marketUnit"], "baseCoin");
        assert_eq!(b["orderLinkId"], ORDER_ID);
    }

    #[test]
    fn limit_carries_price_and_maps_day_to_gtc() {
        let b = order_body(&req("limit", "sell", 1.5, Some(65000.5), "day")).unwrap();
        assert_eq!(b["side"], "Sell");
        assert_eq!(b["orderType"], "Limit");
        assert_eq!(b["price"], "65000.5");
        assert_eq!(b["timeInForce"], "GTC");
        assert_eq!(order_body(&req("limit", "buy", 1.0, Some(1.0), "ioc")).unwrap()["timeInForce"], "IOC");
        assert_eq!(order_body(&req("limit", "buy", 1.0, Some(1.0), "fok")).unwrap()["timeInForce"], "FOK");
        assert!(order_body(&req("limit", "buy", 1.0, None, "gtc")).is_err());
        assert!(order_body(&req("stop", "buy", 1.0, None, "gtc")).is_err());
    }

    #[test]
    fn submit_rejection_surfaces_retmsg() {
        let e = envelope(serde_json::json!({"retCode":170136,"retMsg":"Order quantity exceeded lower limit.","result":{}}))
            .unwrap_err();
        assert!(e.to_string().contains("lower limit"), "{e}");
    }

    #[test]
    fn order_status_maps_every_bybit_terminal_state() {
        assert_eq!(order_outcome("Filled"), BrokerOrderOutcome::Filled);
        assert_eq!(order_outcome("Cancelled"), BrokerOrderOutcome::Canceled);
        assert_eq!(order_outcome("PartiallyFilledCanceled"), BrokerOrderOutcome::Canceled);
        assert_eq!(order_outcome("Deactivated"), BrokerOrderOutcome::Canceled);
        assert_eq!(order_outcome("Rejected"), BrokerOrderOutcome::Rejected);
        assert_eq!(order_outcome("New"), BrokerOrderOutcome::Working);
        assert_eq!(order_outcome("PartiallyFilled"), BrokerOrderOutcome::Working);
    }

    #[test]
    fn open_orders_and_order_state_read_cum_exec_qty_and_avg_price() {
        let open = parse_open_orders(&sample("open_orders"));
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].client_order_id, ORDER_ID);
        assert_eq!(open[0].symbol, "BTCUSDT");
        assert_eq!(open[0].executed_qty, 0.0005);

        let history = sample("order_history");
        let filled = parse_order_state(&history["list"][0]);
        assert_eq!(filled.outcome, BrokerOrderOutcome::Filled);
        assert_eq!(filled.executed_qty, 0.001);
        assert_eq!(filled.avg_px, 65000.0);
        let cancelled = parse_order_state(&history["list"][1]);
        assert_eq!(cancelled.outcome, BrokerOrderOutcome::Canceled);
        assert_eq!(cancelled.avg_px, 0.0); // avgPrice "" when nothing filled

        let prev = parse_prev_prices(&sample("tickers_spot"), &["BTCUSDT".to_string(), "BADUSDT".to_string()]);
        assert_eq!(prev, vec![("BTCUSDT".to_string(), 64000.0)]);
    }

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
            serde_json::from_str(include_str!("testdata/instruments_spot.json")).unwrap();
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
