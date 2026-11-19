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

use chrono::{DateTime, TimeZone, Utc};
use tracing::{info, warn};

use crate::venue_event::{Cursor, ExecutionSource, PullHints, PullPage, VenueFill, VenueOrderStatus, VenueOrderUpdate};

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
    let status = resp.status().as_u16();
    let text = resp.text().await.map_err(|e| BrokerError::Network(e.to_string()))?;
    if status == 429 || status >= 500 {
        return Err(crate::adapter::http_failure(status, text));
    }
    parse_body(status, &text)
}

/// A response body: the envelope when it is JSON; a non-JSON body (a proxy's
/// 502 page) says nothing about the request, so it is `Network`.
pub(crate) fn parse_body(status: u16, text: &str) -> Result<serde_json::Value, BrokerError> {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(body) => envelope(body),
        Err(_) => Err(BrokerError::Network(format!("HTTP {status}: {text}"))),
    }
}

/// An accepted create's order id. Accepted without one, the venue may hold the
/// order: that is unknown, not a rejection.
pub(crate) fn created(result: &serde_json::Value) -> Result<BrokerOrderResponse, BrokerError> {
    match result["orderId"].as_str() {
        Some(id) if !id.is_empty() => Ok(BrokerOrderResponse { external_order_id: id.to_string() }),
        _ => Err(BrokerError::Network(format!("Bybit accepted the order but returned no orderId: {result}"))),
    }
}

/// The envelope's `result` when `retCode` is 0, else a rejection carrying `retMsg`.
pub(crate) fn envelope(body: serde_json::Value) -> Result<serde_json::Value, BrokerError> {
    match body["retCode"].as_i64() {
        Some(0) => Ok(body["result"].clone()),
        // Server timeout / server error: the request's outcome is unknown.
        Some(code @ (10000 | 10016)) => Err(BrokerError::Network(format!(
            "{} (retCode {code})",
            body["retMsg"].as_str().unwrap_or("no message")
        ))),
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

/// Quote coins Bybit spot lists; the base is what precedes the quote in the
/// symbol (a pushed execution has no instrument lookup).
const QUOTES: &[&str] = &["USDT", "USDC", "USDE", "FDUSD", "DAI", "EUR", "BRL", "TRY", "BTC", "ETH", "MNT"];

fn base_quote(symbol: &str) -> Option<(&str, &str)> {
    QUOTES.iter().filter_map(|q| symbol.strip_suffix(q).filter(|b| !b.is_empty()).map(|b| (b, *q))).next()
}

/// The fee of one execution row, the same from the stream and from
/// `/v5/execution/list`. `execFee` is positive when charged and negative for a
/// maker rebate, the OMS sign. Its currency is `feeCurrency` when the row has
/// it; otherwise Bybit spot's rule, a buy pays in the base coin and a sell in
/// the quote coin. A zero fee is none.
pub(crate) fn fee_of(d: &serde_json::Value, exec_id: &str) -> Option<(f64, String)> {
    let amount: f64 = d["execFee"].as_str()?.parse().ok()?;
    if amount == 0.0 {
        return None;
    }
    let currency = match d["feeCurrency"].as_str().filter(|c| !c.is_empty()) {
        Some(c) => c.to_string(),
        None => {
            let symbol = d["symbol"].as_str().unwrap_or("");
            let side = d["side"].as_str().unwrap_or("");
            match base_quote(symbol).map(|(base, quote)| if side == "Buy" { base } else { quote }.to_string()) {
                Some(c) => c,
                None => {
                    warn!(%symbol, %exec_id, fee = amount, "Bybit: fee currency unresolved, fee not booked");
                    return None;
                }
            }
        }
    };
    Some((amount, currency))
}

fn time_ms(v: &serde_json::Value) -> Option<DateTime<Utc>> {
    v.as_str().and_then(|s| s.parse::<i64>().ok()).and_then(|ms| Utc.timestamp_millis_opt(ms).single())
}

/// One execution row (private `execution` push or `/v5/execution/list`) as a
/// fill. `Err` names what is missing; rows that are not trades (`execType`
/// other than `Trade`) are `Ok(None)`.
pub(crate) fn fill_from_row(d: &serde_json::Value, account_ref: &str) -> Result<Option<VenueFill>, &'static str> {
    if d["execType"].as_str() != Some("Trade") {
        return Ok(None);
    }
    let exec_id = d["execId"].as_str().filter(|e| !e.is_empty()).ok_or("execId")?;
    let venue_order_id = d["orderId"].as_str().filter(|e| !e.is_empty()).ok_or("orderId")?;
    let qty = num(&d["execQty"]).ok_or("execQty")?;
    let price = num(&d["execPrice"]).ok_or("execPrice")?;
    let buy = match d["side"].as_str() {
        Some("Buy") => true,
        Some("Sell") => false,
        _ => return Err("side"),
    };
    Ok(Some(VenueFill {
        exec_id: exec_id.to_string(),
        venue_order_id: venue_order_id.to_string(),
        client_order_id: d["orderLinkId"].as_str().filter(|c| !c.is_empty()).map(str::to_string),
        account_ref: account_ref.to_string(),
        symbol: d["symbol"].as_str().unwrap_or("").to_string(),
        buy,
        qty,
        price,
        fee: fee_of(d, exec_id),
        executed_at: time_ms(&d["execTime"]),
    }))
}

/// `orderStatus` as the venue's view of an order; `None` for a status the OMS
/// does not know (the caller warns).
pub(crate) fn venue_status(d: &serde_json::Value) -> Option<VenueOrderStatus> {
    Some(match d["orderStatus"].as_str()? {
        "New" | "PartiallyFilled" | "Untriggered" => VenueOrderStatus::Working,
        "Filled" => VenueOrderStatus::Filled,
        "Cancelled" | "PartiallyFilledCanceled" | "Deactivated" => VenueOrderStatus::Canceled,
        "Rejected" => VenueOrderStatus::Rejected {
            reason: d["rejectReason"].as_str().filter(|r| !r.is_empty() && *r != "EC_NoError").unwrap_or("Rejected").to_string(),
        },
        _ => return None,
    })
}

/// One order row (private `order` push or `/v5/order/realtime`) as the venue's
/// view of the order.
pub(crate) fn order_update_from_row(d: &serde_json::Value, account_ref: &str) -> Option<VenueOrderUpdate> {
    let order_type = d["orderType"].as_str().unwrap_or("").to_string();
    Some(VenueOrderUpdate {
        venue_order_id: d["orderId"].as_str().filter(|i| !i.is_empty())?.to_string(),
        client_order_id: d["orderLinkId"].as_str().filter(|c| !c.is_empty()).map(str::to_string),
        account_ref: account_ref.to_string(),
        symbol: d["symbol"].as_str().unwrap_or("").to_string(),
        buy: d["side"].as_str() == Some("Buy"),
        limit_px: if order_type == "Market" { None } else { num(&d["price"]).filter(|p| *p != 0.0) },
        order_type,
        qty: num(&d["qty"]).unwrap_or(0.0),
        status: venue_status(d)?,
        cum_qty: num(&d["cumExecQty"]).unwrap_or(0.0),
        ts: time_ms(&d["updatedTime"]),
    })
}

/// `/v5/order/realtime` rows as the venue's view of each open order.
pub(crate) fn parse_open_order_snapshot(result: &serde_json::Value, account_ref: &str) -> Vec<VenueOrderUpdate> {
    result["list"].as_array().into_iter().flatten().filter_map(|o| order_update_from_row(o, account_ref)).collect()
}

/// `/v5/execution/list` serves one window of at most 7 days per query, so the
/// pull walks windows; the cursor is the window's start plus the venue's own
/// page token (which only holds under the same window): `"{start_ms}:{token}"`.
pub(crate) const WINDOW_MS: i64 = 7 * 86_400_000;
/// Bybit serves two years of executions.
const MAX_HISTORY_DAYS: i64 = 730;
/// Where a pull that ended on its last window resumes: a margin before its
/// newest row, so a late-indexed execution stamped a little earlier is still
/// seen (re-reads are exec-id duplicates).
pub(crate) const LATE_MARGIN_MS: i64 = 1_800_000;
const EXECUTIONS_LIMIT: usize = 100;
/// Most empty windows one call skips (two years of them).
const MAX_EMPTY_HOPS: usize = 110;

/// `"{start_ms}:{newest_ms}:{token}"`: the window's start, the newest row time
/// seen in it so far (rows come newest first, so its first page has it), and
/// the venue's page token (only valid under the same window).
pub(crate) fn parse_cursor(from: Option<&Cursor>) -> Option<(i64, i64, String)> {
    let mut parts = from?.0.splitn(3, ':');
    let start = parts.next()?.parse().ok()?;
    let newest = parts.next()?.parse().ok()?;
    Some((start, newest, parts.next()?.to_string()))
}

fn format_cursor(start_ms: i64, newest_ms: i64, token: &str) -> Cursor {
    Cursor(format!("{start_ms}:{newest_ms}:{token}"))
}

/// Start of a scan with no cursor: `since`, but never before the venue's history.
pub(crate) fn first_window_start(since: DateTime<Utc>, now: DateTime<Utc>) -> i64 {
    since.max(now - chrono::Duration::days(MAX_HISTORY_DAYS)).timestamp_millis()
}

pub(crate) struct ExecutionsPage {
    pub page: PullPage,
    /// Rows the venue returned, before any filtering.
    pub raw_rows: usize,
}

/// One `/v5/execution/list` result as a pull page for the window starting at
/// `start_ms`. Rows before `since` are dropped on every page; a row missing a
/// field is warned about and skipped. `more` and the cursor come from the raw
/// page: the venue's token while it has one, then the next window until `now`.
pub(crate) fn executions_page(result: &serde_json::Value, account_ref: &str, since: DateTime<Utc>, start_ms: i64, carried_newest_ms: i64, now_ms: i64) -> ExecutionsPage {
    let rows = result["list"].as_array().cloned().unwrap_or_default();
    let mut fills = Vec::new();
    for d in &rows {
        match fill_from_row(d, account_ref) {
            Ok(Some(f)) if f.executed_at.is_none_or(|at| at >= since) => fills.push(f),
            Ok(_) => {}
            Err(missing) => warn!(exec_id = d["execId"].as_str().unwrap_or(""), missing, "Bybit executions: row skipped"),
        }
    }
    let token = result["nextPageCursor"].as_str().unwrap_or("");
    let last_window = start_ms + WINDOW_MS >= now_ms;
    // `carried_newest_ms` is the newest row time of the window's earlier pages (0 on the first).
    let newest = rows.iter().filter_map(|d| time_ms(&d["execTime"])).map(|t| t.timestamp_millis()).fold(carried_newest_ms, i64::max);
    let (next, more) = if !token.is_empty() {
        (format_cursor(start_ms, newest, token), true)
    } else if !last_window {
        (format_cursor(start_ms + WINDOW_MS, 0, ""), true)
    } else if newest > 0 {
        // Resume a margin before the window's newest row (from its first page), never before its start.
        let from = (newest - LATE_MARGIN_MS).max(start_ms);
        (format_cursor(from, from, ""), false)
    } else {
        (Cursor(String::new()), false)
    };
    ExecutionsPage { page: PullPage { fills, next, more }, raw_rows: rows.len() }
}

#[async_trait::async_trait]
impl ExecutionSource for BybitAdapter {
    async fn fills_since(
        &self,
        account_ref: &str,
        _scope: &str,
        from: Option<&Cursor>,
        since: DateTime<Utc>,
    ) -> Result<PullPage, BrokerError> {
        let now = Utc::now();
        let (mut start_ms, mut newest_ms, mut token) =
            parse_cursor(from).unwrap_or_else(|| (first_window_start(since, now), 0, String::new()));
        for _ in 0..MAX_EMPTY_HOPS {
            let end_ms = (start_ms + WINDOW_MS).min(now.timestamp_millis());
            let mut query = format!("category=spot&limit={EXECUTIONS_LIMIT}&startTime={start_ms}&endTime={end_ms}");
            if !token.is_empty() {
                query.push_str(&format!("&cursor={token}"));
            }
            let result = self.get("/v5/execution/list", &query, true).await?;
            let page = executions_page(&result, account_ref, since, start_ms, newest_ms, now.timestamp_millis());
            // An empty window in the middle of history: try the next one rather than
            // hand the reconciler an empty page with more to come.
            if page.raw_rows == 0 && page.page.more {
                if let Some((next_start, _, _)) = parse_cursor(Some(&page.page.next)) {
                    start_ms = next_start;
                    newest_ms = 0;
                    token = String::new();
                    continue;
                }
            }
            return Ok(page.page);
        }
        // Two years of empty windows walked: nothing to report, and nothing to resume.
        Ok(PullPage { fills: Vec::new(), next: Cursor(String::new()), more: false })
    }

    /// `GET /v5/order/realtime`: every open spot order, following the page cursor.
    async fn open_order_snapshot(&self, account_ref: &str) -> Result<Vec<VenueOrderUpdate>, BrokerError> {
        let mut out = Vec::new();
        let mut cursor = String::new();
        loop {
            let mut query = "category=spot&limit=50&openOnly=0".to_string();
            if !cursor.is_empty() {
                query.push_str(&format!("&cursor={cursor}"));
            }
            let result = self.get("/v5/order/realtime", &query, true).await?;
            out.extend(parse_open_order_snapshot(&result, account_ref));
            cursor = result["nextPageCursor"].as_str().unwrap_or("").to_string();
            if cursor.is_empty() {
                return Ok(out);
            }
        }
    }

    async fn scopes(&self, _account_ref: &str) -> Result<Vec<String>, BrokerError> {
        Ok(vec![String::new()])
    }

    fn hints(&self) -> PullHints {
        PullHints { intraday_secs: 60, max_history: Some(chrono::Duration::days(MAX_HISTORY_DAYS)), per_symbol: false }
    }
}

#[async_trait::async_trait]
impl BrokerAdapter for BybitAdapter {
    async fn submit_order(&self, req: &BrokerOrderRequest) -> Result<BrokerOrderResponse, BrokerError> {
        let body = order_body(req)?;
        info!(symbol = %req.symbol, side = %req.side, "submitting order to Bybit");
        let result = self.post("/v5/order/create", &body).await?;
        created(&result)
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
    use crate::venue_event::VenueEvent as VEvent;

    #[test]
    fn a_server_timeout_or_unreadable_answer_is_not_a_rejection() {
        assert!(matches!(envelope(serde_json::json!({"retCode":10000,"retMsg":"Server Timeout"})), Err(BrokerError::Network(_))));
        assert!(matches!(envelope(serde_json::json!({"retCode":10016,"retMsg":"Server error"})), Err(BrokerError::Network(_))));
        assert!(matches!(parse_body(502, "<html>bad gateway</html>"), Err(BrokerError::Network(_))));
        assert!(matches!(created(&serde_json::json!({})), Err(BrokerError::Network(_))));
        assert_eq!(created(&serde_json::json!({"orderId": "v-1"})).unwrap().external_order_id, "v-1");
    }

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

    // --- execution pull ---

    fn exec_list() -> serde_json::Value {
        envelope(serde_json::from_str(include_str!("testdata/execution_list.json")).unwrap()).unwrap()
    }

    const T0: i64 = 1_700_000_000_000;

    fn ms(v: i64) -> DateTime<Utc> {
        Utc.timestamp_millis_opt(v).unwrap()
    }

    #[test]
    fn bybit_fill_without_oms_id_is_kept() {
        let page = executions_page(&exec_list(), "", DateTime::<Utc>::MIN_UTC, T0, 0, T0 + 1000);
        // The second row was placed in the Bybit app: no orderLinkId, still a fill.
        let f = page.page.fills.iter().find(|f| f.exec_id == "2100000000007764999").expect("kept");
        assert_eq!(f.client_order_id, None);
        assert_eq!((f.venue_order_id.as_str(), f.symbol.as_str(), f.buy, f.qty, f.price), ("1820000000000000009", "BTCUSDT", false, 0.002, 65010.0));
    }

    #[test]
    fn bybit_pull_page_maps_and_cursors() {
        let page = executions_page(&exec_list(), "acct", DateTime::<Utc>::MIN_UTC, T0, 0, T0 + 1000).page;
        // The third row has no execQty: skipped (and warned), the other two map.
        assert_eq!(page.fills.iter().map(|f| f.exec_id.as_str()).collect::<Vec<_>>(), ["2100000000007764263", "2100000000007764999"]);
        let f = &page.fills[0];
        assert_eq!(f.account_ref, "acct");
        assert_eq!(f.client_order_id.as_deref(), Some("3f6b1c2e-8a4d-4e5f-9b21-1c2d3e4f5a6b"));
        assert_eq!(f.fee, Some((0.000001, "BTC".to_string())));
        assert_eq!(f.executed_at, Some(ms(T0)));
        assert_eq!(page.fills[1].fee, Some((-0.01, "USDT".to_string())), "a maker rebate stays negative");
        // The venue's own token continues the page, tied to its window start.
        assert_eq!(page.next, Cursor(format!("{T0}:{}:page2tokenabc%3A1700000100000%2C1", T0 + 60_000)));
        assert!(page.more);
    }

    #[test]
    fn a_window_with_no_token_hops_to_the_next_window_or_ends_at_now() {
        let mut result = exec_list();
        result["nextPageCursor"] = serde_json::json!("");
        // Window 1 of several: the next call starts where this window ended.
        let mid = executions_page(&result, "", DateTime::<Utc>::MIN_UTC, T0, 0, T0 + 3 * WINDOW_MS).page;
        assert_eq!((mid.next, mid.more), (Cursor(format!("{}:0:", T0 + WINDOW_MS)), true));
        // The last window: resume a margin before the newest row (never before the window start).
        let last = executions_page(&result, "", DateTime::<Utc>::MIN_UTC, T0, 0, T0 + 1000).page;
        assert_eq!((last.next, last.more), (Cursor(format!("{T0}:{T0}:")), false));
        let late = executions_page(&result, "", DateTime::<Utc>::MIN_UTC, T0 - 2 * LATE_MARGIN_MS, 0, T0 + 1000).page;
        let from = T0 + 60_000 - LATE_MARGIN_MS;
        assert_eq!(late.next, Cursor(format!("{from}:{from}:")));
        // An empty window moves nothing and says so.
        let empty = serde_json::json!({ "list": [], "nextPageCursor": "" });
        let p = executions_page(&empty, "", DateTime::<Utc>::MIN_UTC, T0, 0, T0 + 1000).page;
        assert_eq!((p.next, p.more, p.fills.len()), (Cursor(String::new()), false, 0));
    }

    #[test]
    fn bybit_multi_page_last_window_cursor_tracks_newest_row() {
        // Rows come newest first: page 1 holds the newest, the final page the oldest.
        let row = |id: &str, t: i64| serde_json::json!({ "execId": id, "orderId": "o", "symbol": "BTCUSDT", "side": "Buy", "execQty": "1", "execPrice": "1", "execType": "Trade", "execTime": t.to_string() });
        let day = 86_400_000;
        let (newest, oldest) = (T0 + 6 * day, T0 + day);
        let now = T0 + 7 * day;
        let first = executions_page(&serde_json::json!({ "list": [row("a", newest)], "nextPageCursor": "tok" }), "", DateTime::<Utc>::MIN_UTC, T0, 0, now);
        assert_eq!(first.page.next, Cursor(format!("{T0}:{newest}:tok")));
        let (start, carried, _) = parse_cursor(Some(&first.page.next)).unwrap();
        let last = executions_page(&serde_json::json!({ "list": [row("z", oldest)], "nextPageCursor": "" }), "", DateTime::<Utc>::MIN_UTC, start, carried, now).page;
        let from = newest - LATE_MARGIN_MS;
        assert_eq!((last.next, last.more), (Cursor(format!("{from}:{from}:")), false));
        assert_eq!(LATE_MARGIN_MS, 30 * 60 * 1000);
    }

    #[test]
    fn since_filters_every_page_and_the_cursor_still_moves() {
        let page = executions_page(&exec_list(), "", ms(T0 + 40_000), T0, 0, T0 + 1000).page;
        assert_eq!(page.fills.iter().map(|f| f.exec_id.as_str()).collect::<Vec<_>>(), ["2100000000007764999"]);
        assert!(page.more, "from the raw page, not the filtered count");
    }

    #[test]
    fn the_cursor_names_its_window_and_the_first_window_is_capped_at_two_years() {
        assert_eq!(parse_cursor(Some(&Cursor(format!("{T0}:77:abc%3A1")))), Some((T0, 77, "abc%3A1".to_string())));
        assert_eq!(parse_cursor(Some(&Cursor(format!("{T0}:0:")))), Some((T0, 0, String::new())));
        assert_eq!(parse_cursor(None), None);
        let now = ms(T0 * 2);
        assert_eq!(first_window_start(DateTime::<Utc>::MIN_UTC, now), T0 * 2 - 730 * 86_400_000);
        assert_eq!(first_window_start(ms(T0 * 2 - 1000), now), T0 * 2 - 1000);
    }

    #[test]
    fn bybit_stream_and_pull_produce_the_same_exec_id_for_one_fill() {
        let msg: serde_json::Value = serde_json::from_str(include_str!("testdata/ws_execution.json")).unwrap();
        let VEvent::Fill(s) = &crate::connectors::bybit::stream::venue_events(&msg)[0] else { panic!("a fill") };
        let row = serde_json::json!({ "execId": "2100000000007764263", "orderId": "1820000000000000002", "symbol": "BTCUSDT", "side": "Buy",
            "execQty": "0.001", "execPrice": "65000", "execType": "Trade", "execTime": "1700000000000", "execFee": "0.000001", "feeCurrency": "BTC" });
        let p = executions_page(&serde_json::json!({ "list": [row], "nextPageCursor": "" }), "", DateTime::<Utc>::MIN_UTC, T0, 0, T0 + 1000).page;
        assert_eq!(s.exec_id, p.fills[0].exec_id);
        assert_eq!(s.exec_id, "2100000000007764263");
        assert_eq!(s.fee, p.fills[0].fee, "the fee is the same too: one helper for both paths");
    }

    #[test]
    fn fee_currency_is_quote_on_sell_base_on_buy_and_fee_currency_wins() {
        let fee = |v: serde_json::Value| fee_of(&v, "x");
        assert_eq!(fee(serde_json::json!({"symbol": "BTCUSDT", "side": "Sell", "execFee": "0.065"})), Some((0.065, "USDT".to_string())));
        assert_eq!(fee(serde_json::json!({"symbol": "BTCUSDT", "side": "Buy", "execFee": "0.5", "feeCurrency": "MNT"})), Some((0.5, "MNT".to_string())));
        assert_eq!(fee(serde_json::json!({"symbol": "BTCUSDT", "side": "Buy", "execFee": "0.5"})), Some((0.5, "BTC".to_string())));
        assert_eq!(fee(serde_json::json!({"symbol": "BTCUSDT", "side": "Buy", "execFee": "0"})), None);
        assert_eq!(fee(serde_json::json!({"symbol": "FOOBAR", "side": "Buy", "execFee": "0.5"})), None);
    }

    #[test]
    fn open_order_snapshot_maps_working_orders_and_hints_are_two_years() {
        let snap = parse_open_order_snapshot(&sample("open_orders"), "acct");
        assert_eq!(snap.len(), 1);
        let o = &snap[0];
        assert_eq!((o.venue_order_id.as_str(), o.account_ref.as_str(), o.buy, o.qty, o.limit_px, o.cum_qty), ("1820000000000000001", "acct", true, 0.001, Some(30000.0), 0.0005));
        assert_eq!(o.status, VenueOrderStatus::Working);
        assert_eq!(
            ExecutionSource::hints(&BybitAdapter::new("k".into(), "s".into(), "PAPER")),
            PullHints { intraday_secs: 60, max_history: Some(chrono::Duration::days(730)), per_symbol: false }
        );
    }
}
