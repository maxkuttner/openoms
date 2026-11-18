//! OKX spot broker adapter (v5 API; Demo Trading as PAPER).
//!
//! Auth: every private REST call carries `OK-ACCESS-KEY`, `OK-ACCESS-PASSPHRASE`,
//! `OK-ACCESS-TIMESTAMP` (ISO 8601, ms, UTC) and `OK-ACCESS-SIGN`, the base64
//! HMAC-SHA256 (keyed by the API secret) of `timestamp + METHOD + requestPath`
//! (query included) `+ body`. The private WebSocket signs
//! `{unix seconds}GET/users/self/verify` with the same key (`okx_stream.rs`).
//!
//! Demo Trading uses the production REST host with `x-simulated-trading: 1` on
//! every request, separate demo API keys, and its own WebSocket host.
//!
//! Every v5 response is an envelope `{"code":"0","msg":"","data":[..]}`; an order
//! request that fails carries the venue's reason in `data[0].sMsg` under a
//! non-zero `sCode`, so [`envelope`] is the one place a response is judged.
//!
//! Symbol mapping: OKX pairs are their own instruments, venue `OKX`. The dashed
//! pair (e.g. `BTC-USDT`) is the routing symbol; the base currency the native id.
//! `clOrdId` allows 32 alphanumerics, so the OMS order UUID goes out without its
//! dashes ([`client_order_id`]) and is read back with [`oms_order_id`].

use base64::Engine;
use dataprovider::{Identifiers, InstrumentDef};
use hmac::{Hmac, Mac};
use reqwest::Client;
use sha2::Sha256;
use chrono::{DateTime, TimeZone, Utc};
use tracing::{info, warn};

use crate::venue_event::{Cursor, ExecutionSource, PullHints, PullPage, VenueFill, VenueOrderStatus, VenueOrderUpdate};
use uuid::Uuid;

use crate::adapter::{BrokerAdapter, BrokerError, BrokerInstrument, BrokerOrderRequest, BrokerOrderResponse, InstrumentProvider};
use crate::md::daily_stats::DailyStats;
use crate::contract::{BrokerOpenOrder, BrokerOrderOutcome, BrokerOrderState};

const REST_URL: &str = "https://openapi.okx.com";

/// OKX's page size cap for orders-pending.
const PAGE_LIMIT: usize = 100;

/// An OKX v5 API key: key, secret and the passphrase chosen when creating it.
/// Stored as `{"kind":"Okx", …}` (see [`crate::credentials::BrokerCredentials`]).
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct OkxCredentials {
    pub api_key: String,
    pub api_secret: String,
    pub passphrase: String,
}

impl OkxCredentials {
    pub const KIND: &'static str = "Okx";
}

#[derive(Clone)]
pub struct OkxAdapter {
    client: Client,
    api_key: String,
    api_secret: String,
    passphrase: String,
    /// Demo Trading: `x-simulated-trading: 1` on every REST request.
    simulated: bool,
    private_ws_url: &'static str,
}

impl OkxAdapter {
    pub fn new(api_key: String, api_secret: String, passphrase: String, environment: &str) -> Self {
        let simulated = environment != "LIVE";
        let private_ws_url =
            if simulated { "wss://wspap.okx.com/ws/v5/private" } else { "wss://ws.okx.com/ws/v5/private" };
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        Self { client, api_key, api_secret, passphrase, simulated, private_ws_url }
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    pub fn passphrase(&self) -> &str {
        &self.passphrase
    }

    pub fn private_ws_url(&self) -> &'static str {
        self.private_ws_url
    }

    /// Base64 HMAC-SHA256 of `prehash` under the API secret.
    pub fn sign(&self, prehash: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.api_secret.as_bytes()).expect("HMAC takes any key length");
        mac.update(prehash.as_bytes());
        base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
    }

    fn headers(&self, mut req: reqwest::RequestBuilder, method: &str, path: &str, body: &str, signed: bool) -> reqwest::RequestBuilder {
        if self.simulated {
            req = req.header("x-simulated-trading", "1");
        }
        if !signed {
            return req;
        }
        let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        req.header("OK-ACCESS-KEY", &self.api_key)
            .header("OK-ACCESS-PASSPHRASE", &self.passphrase)
            .header("OK-ACCESS-TIMESTAMP", &ts)
            .header("OK-ACCESS-SIGN", self.sign(&format!("{ts}{method}{path}{body}")))
    }

    /// GET `path` (query included), signed for private endpoints; returns the envelope's `data`.
    async fn get(&self, path: &str, signed: bool) -> Result<serde_json::Value, BrokerError> {
        let req = self.headers(self.client.get(format!("{REST_URL}{path}")), "GET", path, "", signed);
        read(req.send().await).await
    }

    /// Signed POST of a JSON body; returns the envelope's `data`.
    async fn post(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value, BrokerError> {
        let body = body.to_string();
        let req = self.client.post(format!("{REST_URL}{path}")).header("Content-Type", "application/json");
        read(self.headers(req, "POST", path, &body, true).body(body).send().await).await
    }

    /// GET /api/v5/public/instruments?instType=SPOT — public, not paged.
    pub async fn list_spot_instruments(&self) -> Result<Vec<BrokerInstrument>, BrokerError> {
        let data = self.get("/api/v5/public/instruments?instType=SPOT", false).await?;
        Ok(parse_spot_instruments(&data))
    }

    /// GET /api/v5/account/config — the key's `perm` ("read_only,trade,…"), used
    /// by the credentials Test button.
    pub async fn key_permissions(&self) -> Result<String, BrokerError> {
        let data = self.get("/api/v5/account/config", true).await?;
        Ok(data[0]["perm"].as_str().unwrap_or("").to_string())
    }
}

/// Turn a sent request into the envelope's `data`, whatever the HTTP status.
async fn read(resp: Result<reqwest::Response, reqwest::Error>) -> Result<serde_json::Value, BrokerError> {
    let resp = resp.map_err(|e| BrokerError::Network(e.to_string()))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.map_err(|e| BrokerError::Network(e.to_string()))?;
    if status == 429 || status >= 500 {
        return Err(crate::adapter::http_failure(status, text));
    }
    parse_body(status, &text)
}

/// A response body: the envelope when it is JSON; a non-JSON body says
/// nothing about the request, so it is `Network`.
pub(crate) fn parse_body(status: u16, text: &str) -> Result<serde_json::Value, BrokerError> {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(body) => envelope(body),
        Err(_) => Err(BrokerError::Network(format!("HTTP {status}: {text}"))),
    }
}

/// The envelope's `data` when `code` is "0" and no order in it failed; else a
/// rejection carrying the order's `sMsg` (or the envelope's `msg`).
pub(crate) fn envelope(body: serde_json::Value) -> Result<serde_json::Value, BrokerError> {
    let failed = body["data"]
        .as_array()
        .and_then(|l| l.iter().find(|d| d["sCode"].as_str().is_some_and(|c| c != "0")));
    match (body["code"].as_str(), failed) {
        (Some("0"), None) => Ok(body["data"].clone()),
        (_, Some(d)) => Err(BrokerError::BrokerRejected(format!(
            "{} (sCode {})",
            d["sMsg"].as_str().unwrap_or("no message"),
            d["sCode"].as_str().unwrap_or("missing")
        ))),
        // Service unavailable / endpoint timeout / system busy: OKX documents
        // these as not saying whether the request succeeded.
        (Some(code @ ("50001" | "50004" | "50013")), None) => Err(BrokerError::Network(format!(
            "{} (code {code})",
            body["msg"].as_str().filter(|m| !m.is_empty()).unwrap_or("no message")
        ))),
        (code, None) => Err(BrokerError::BrokerRejected(format!(
            "{} (code {})",
            body["msg"].as_str().filter(|m| !m.is_empty()).unwrap_or("no message"),
            code.unwrap_or("missing")
        ))),
    }
}

/// OKX's code for an order it no longer knows (51603). Only a cancelled order
/// with nothing filled ages out (after about 2 hours), so that is what it was.
pub(crate) fn missing_order_state(e: &BrokerError) -> Option<BrokerOrderState> {
    matches!(e, BrokerError::BrokerRejected(m) if m.ends_with("(code 51603)") || m.ends_with("(sCode 51603)"))
        .then_some(BrokerOrderState { outcome: BrokerOrderOutcome::Canceled, executed_qty: 0.0, avg_px: 0.0 })
}

/// One order's path: by OKX's id when known, else by our `clOrdId`.
pub(crate) fn order_lookup_path(symbol: &str, client_order_id: &str, external_order_id: &str) -> String {
    if external_order_id.is_empty() {
        format!("/api/v5/trade/order?instId={symbol}&clOrdId={}", self::client_order_id(client_order_id))
    } else {
        format!("/api/v5/trade/order?instId={symbol}&ordId={external_order_id}")
    }
}

/// The OMS order id as an OKX `clOrdId`: the UUID without dashes (32 hex chars).
pub(crate) fn client_order_id(order_id: &str) -> String {
    order_id.replace('-', "")
}

/// The OMS order a `clOrdId` belongs to; `None` for orders the OMS didn't place.
pub(crate) fn oms_order_id(cl_ord_id: &str) -> Option<Uuid> {
    // Only the simple form we send; a dashed id can't be a clOrdId.
    (cl_ord_id.len() == 32).then(|| Uuid::parse_str(cl_ord_id).ok()).flatten()
}

/// A decimal string field as f64.
fn num(v: &serde_json::Value) -> Option<f64> {
    v.as_str().and_then(|s| s.parse().ok())
}

/// Digits after the point of a step like "0.01" (2) or "1" (0).
fn decimals(step: &str) -> i32 {
    step.split_once('.').map_or(0, |(_, frac)| frac.trim_end_matches('0').len() as i32)
}

/// The spot catalog from a public instruments `data`. Every pair is listed;
/// only `live` ones are tradeable. An empty limit is no limit.
pub(crate) fn parse_spot_instruments(data: &serde_json::Value) -> Vec<BrokerInstrument> {
    let list = data.as_array().cloned().unwrap_or_default();
    list.iter()
        .filter_map(|s| {
            let pair = s["instId"].as_str()?.to_string();
            let base = s["baseCcy"].as_str()?.to_string();
            let quote = s["quoteCcy"].as_str()?.to_string();
            let tick = s["tickSz"].as_str().unwrap_or("0");
            let definition = InstrumentDef {
                symbol: pair.clone(),
                venue: "OKX".to_string(),
                currency: quote,
                asset_class: "CRYPTO".to_string(),
                instrument_class: "SPOT".to_string(),
                name: None,
                price_precision: decimals(tick),
                price_increment: tick.parse().unwrap_or(0.0),
                size_increment: num(&s["lotSz"]).unwrap_or(0.0),
                lot_size: None,
                contract_size: 1.0,
                native_id: Some(base),
                provider_exchange: Some("OKX".to_string()),
                derivative: None,
                identifiers: Identifiers::default(),
            };
            Some(BrokerInstrument {
                broker_symbol: pair,
                broker_exchange: Some("OKX".to_string()),
                is_tradeable: s["state"].as_str() == Some("live"),
                min_quantity: num(&s["minSz"]),
                max_quantity: num(&s["maxLmtSz"]),
                min_notional: None,
                max_notional: None,
                definition,
            })
        })
        .collect()
}

/// A quantity or price as OKX wants it: plain decimal, no float noise, no
/// trailing zeros.
pub(crate) fn decimal(x: f64) -> String {
    let s = format!("{x:.10}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" { "0".to_string() } else { s.to_string() }
}

/// The POST /api/v5/trade/order body for an OMS order.
pub(crate) fn order_body(req: &BrokerOrderRequest) -> Result<serde_json::Value, BrokerError> {
    let side = if req.side.eq_ignore_ascii_case("sell") { "sell" } else { "buy" };
    let mut body = serde_json::json!({
        "instId": req.symbol,
        // Spot, no margin.
        "tdMode": "cash",
        "side": side,
        "sz": decimal(req.quantity),
        "clOrdId": client_order_id(&req.order_id),
    });
    match req.order_type.as_str() {
        // A spot market buy's sz is in the quote currency unless told otherwise;
        // OMS quantities are always in the base currency.
        "market" => {
            body["ordType"] = "market".into();
            body["tgtCcy"] = "base_ccy".into();
        }
        "limit" => {
            let price = req
                .limit_price
                .ok_or_else(|| BrokerError::BrokerRejected("limit order missing limit_price".into()))?;
            // OKX encodes time in force as the order type; Day is meaningless on a
            // 24/7 venue — treat as GTC.
            let ord_type = match req.time_in_force.to_uppercase().as_str() {
                "IOC" => "ioc",
                "FOK" => "fok",
                _ => "limit",
            };
            body["ordType"] = ord_type.into();
            body["px"] = decimal(price).into();
        }
        other => return Err(BrokerError::NotConfigured(format!("unsupported order type: {other}"))),
    }
    Ok(body)
}

/// OKX's order `state` as a reconcile outcome. A cancelled order keeps its fills.
pub(crate) fn order_outcome(state: &str) -> BrokerOrderOutcome {
    match state {
        "filled" => BrokerOrderOutcome::Filled,
        "canceled" | "mmp_canceled" => BrokerOrderOutcome::Canceled,
        _ => BrokerOrderOutcome::Working, // live / partially_filled
    }
}

pub(crate) fn parse_open_orders(data: &serde_json::Value) -> Vec<BrokerOpenOrder> {
    let list = data.as_array().cloned().unwrap_or_default();
    list.iter()
        .filter_map(|o| {
            let cl = o["clOrdId"].as_str()?;
            Some(BrokerOpenOrder {
                client_order_id: oms_order_id(cl).map_or_else(|| cl.to_string(), |id| id.to_string()),
                venue_order_id: o["ordId"].as_str().unwrap_or_default().to_string(),
                symbol: o["instId"].as_str()?.to_string(),
                executed_qty: num(&o["accFillSz"]).unwrap_or(0.0),
            })
        })
        .collect()
}

pub(crate) fn parse_order_state(order: &serde_json::Value) -> BrokerOrderState {
    BrokerOrderState {
        outcome: order_outcome(order["state"].as_str().unwrap_or("")),
        executed_qty: num(&order["accFillSz"]).unwrap_or(0.0),
        avg_px: num(&order["avgPx"]).unwrap_or(0.0),
    }
}

/// `open24h` (the price 24 hours ago, as Bybit's `prevPrice24h`) per wanted
/// symbol; an unparseable price is skipped, never reported as zero.
pub(crate) fn parse_prev_prices(data: &serde_json::Value, wanted: &[String]) -> Vec<(String, f64)> {
    let list = data.as_array().cloned().unwrap_or_default();
    list.iter()
        .filter_map(|t| {
            let symbol = t["instId"].as_str()?;
            if !wanted.iter().any(|w| w == symbol) {
                return None;
            }
            Some((symbol.to_string(), num(&t["open24h"])?))
        })
        .collect()
}

/// The client id an order carries into the OMS: its UUID for an order the OMS
/// placed, the venue's own `clOrdId` otherwise; `None` when empty.
pub(crate) fn venue_client_id(cl_ord_id: &str) -> Option<String> {
    (!cl_ord_id.is_empty()).then(|| oms_order_id(cl_ord_id).map_or_else(|| cl_ord_id.to_string(), |id| id.to_string()))
}

fn time_ms(v: &serde_json::Value) -> Option<DateTime<Utc>> {
    v.as_str().and_then(|s| s.parse::<i64>().ok()).and_then(|ms| Utc.timestamp_millis_opt(ms).single())
}

/// A fee as OKX reports it (`fee`, `fillFee`): negative when charged, positive
/// for a rebate. The OMS books a charge positive, so the sign flips; a zero or
/// currency-less fee is none.
pub(crate) fn fee_of(amount: &serde_json::Value, currency: &serde_json::Value) -> Option<(f64, String)> {
    let amount: f64 = amount.as_str()?.parse().ok()?;
    let currency = currency.as_str().filter(|c| !c.is_empty())?;
    (amount != 0.0).then(|| (-amount, currency.to_string()))
}

/// One fill as the stream and the pull both read it, from the fields the two
/// share under different names. `Err` names what is missing.
pub(crate) fn fill_of(
    d: &serde_json::Value,
    time: Option<DateTime<Utc>>,
    fee: Option<(f64, String)>,
    account_ref: &str,
) -> Result<VenueFill, &'static str> {
    let trade_id = d["tradeId"].as_str().filter(|t| !t.is_empty()).ok_or("tradeId")?;
    let venue_order_id = d["ordId"].as_str().filter(|o| !o.is_empty()).ok_or("ordId")?;
    let qty = num(&d["fillSz"]).ok_or("fillSz")?;
    let price = num(&d["fillPx"]).ok_or("fillPx")?;
    let buy = match d["side"].as_str() {
        Some("buy") => true,
        Some("sell") => false,
        _ => return Err("side"),
    };
    Ok(VenueFill {
        exec_id: trade_id.to_string(),
        venue_order_id: venue_order_id.to_string(),
        client_order_id: venue_client_id(d["clOrdId"].as_str().unwrap_or("")),
        account_ref: account_ref.to_string(),
        symbol: d["instId"].as_str().unwrap_or("").to_string(),
        buy,
        qty,
        price,
        fee,
        executed_at: time,
    })
}

/// OKX's order `state` as the venue's view; `None` for a state the OMS does not know.
pub(crate) fn venue_status(state: &str) -> Option<VenueOrderStatus> {
    Some(match state {
        "live" | "partially_filled" => VenueOrderStatus::Working,
        "filled" => VenueOrderStatus::Filled,
        "canceled" | "mmp_canceled" => VenueOrderStatus::Canceled,
        _ => return None,
    })
}

/// One order row (`orders` push or `orders-pending`) as the venue's view of the order.
pub(crate) fn order_update_from_row(d: &serde_json::Value, account_ref: &str) -> Option<VenueOrderUpdate> {
    let order_type = d["ordType"].as_str().unwrap_or("").to_string();
    Some(VenueOrderUpdate {
        venue_order_id: d["ordId"].as_str().filter(|o| !o.is_empty())?.to_string(),
        client_order_id: venue_client_id(d["clOrdId"].as_str().unwrap_or("")),
        account_ref: account_ref.to_string(),
        symbol: d["instId"].as_str().unwrap_or("").to_string(),
        buy: d["side"].as_str() == Some("buy"),
        limit_px: if order_type == "market" { None } else { num(&d["px"]).filter(|p| *p != 0.0) },
        order_type,
        qty: num(&d["sz"]).unwrap_or(0.0),
        status: venue_status(d["state"].as_str()?)?,
        cum_qty: num(&d["accFillSz"]).unwrap_or(0.0),
        ts: time_ms(&d["uTime"]),
    })
}

/// `orders-pending` rows as the venue's view of each open order.
pub(crate) fn parse_open_order_snapshot(data: &serde_json::Value, account_ref: &str) -> Vec<VenueOrderUpdate> {
    data.as_array().into_iter().flatten().filter_map(|o| order_update_from_row(o, account_ref)).collect()
}

/// `/trade/fills` serves the last 3 days; `/trade/fills-history` goes further.
const FILLS_PATH: &str = "/api/v5/trade/fills";
const FILLS_HISTORY_PATH: &str = "/api/v5/trade/fills-history";
const RECENT_DAYS: i64 = 3;
const MAX_HISTORY_DAYS: i64 = 365;
/// Pages one call may walk (100 rows each) before it gives up rather than
/// advance its cursor past rows it did not read.
const MAX_PAGES: usize = 1000;

/// One page of fills rows (newest first), read against a scan's bounds.
#[derive(Debug, Default)]
pub(crate) struct FillsRows {
    pub fills: Vec<VenueFill>,
    /// Rows the venue returned, before any filtering.
    pub rows: usize,
    pub newest_bill: Option<String>,
    pub oldest_bill: Option<String>,
    /// The walk reached its bound: a row at or before the cursor's bill, or before `since`.
    pub done: bool,
}

/// Rows of `/trade/fills[-history]` as fills, stopping at the first row at or
/// before `after_bill` (the cursor) or executed before `since`, since rows are
/// newest first and everything beyond is older. A row missing a field is warned
/// about and skipped; the bills still count for the cursor.
pub(crate) fn fills_rows(data: &serde_json::Value, account_ref: &str, since: DateTime<Utc>, after_bill: Option<u128>) -> FillsRows {
    let rows = data.as_array().map(Vec::as_slice).unwrap_or_default();
    let mut out = FillsRows { rows: rows.len(), ..Default::default() };
    for d in rows {
        let bill = d["billId"].as_str().unwrap_or("");
        if let (Some(after), Ok(b)) = (after_bill, bill.parse::<u128>()) {
            if b <= after {
                out.done = true;
                break;
            }
        }
        let at = time_ms(&d["ts"]);
        if at.is_some_and(|at| at < since) {
            out.done = true;
            break;
        }
        if !bill.is_empty() {
            out.newest_bill.get_or_insert_with(|| bill.to_string());
            out.oldest_bill = Some(bill.to_string());
        }
        match fill_of(d, at, fee_of(&d["fee"], &d["feeCcy"]), account_ref) {
            Ok(f) => out.fills.push(f),
            Err(missing) => warn!(bill_id = bill, trade_id = d["tradeId"].as_str().unwrap_or(""), missing, "OKX fills: row skipped"),
        }
    }
    out
}

/// A whole walk (newest page first) as one pull page: the cursor is the newest
/// bill seen, and nothing is left to page. Nothing seen keeps the cursor.
pub(crate) fn pull_result(pages: Vec<FillsRows>) -> PullPage {
    let next = pages.iter().find_map(|p| p.newest_bill.clone()).unwrap_or_default();
    PullPage { fills: pages.into_iter().flat_map(|p| p.fills).collect(), next: Cursor(next), more: false }
}

#[async_trait::async_trait]
impl ExecutionSource for OkxAdapter {
    /// Walks fills newest to oldest with `after=<billId>` until the cursor's
    /// bill or `since`, so one call returns everything new: `before=` does not
    /// say which side of the cursor its page is taken from. Recent fills come
    /// from `/trade/fills`; beyond its 3 days, or when the cursor is not
    /// reached there, the walk continues in `/trade/fills-history`.
    async fn fills_since(
        &self,
        account_ref: &str,
        _scope: &str,
        from: Option<&Cursor>,
        since: DateTime<Utc>,
    ) -> Result<PullPage, BrokerError> {
        let after_bill = from.and_then(|c| c.0.parse::<u128>().ok());
        let now = Utc::now();
        let since = since.max(now - chrono::Duration::days(MAX_HISTORY_DAYS));
        let mut history = since < now - chrono::Duration::days(RECENT_DAYS);
        let mut paging: Option<String> = None;
        let mut pages = Vec::new();
        for _ in 0..MAX_PAGES {
            let path = if history { FILLS_HISTORY_PATH } else { FILLS_PATH };
            let mut query = format!("{path}?instType=SPOT&limit={PAGE_LIMIT}&begin={}", since.timestamp_millis());
            if let Some(oldest) = &paging {
                query.push_str(&format!("&after={oldest}"));
            }
            let data = self.get(&query, true).await?;
            let page = fills_rows(&data, account_ref, since, after_bill);
            let (exhausted, done, oldest) = (page.rows < PAGE_LIMIT, page.done, page.oldest_bill.clone());
            pages.push(page);
            if done {
                return Ok(pull_result(pages));
            }
            if exhausted {
                // The last 3 days are read; a cursor not reached in them may be older.
                if !history && after_bill.is_some() {
                    history = true;
                    paging = oldest.or(paging);
                    continue;
                }
                return Ok(pull_result(pages));
            }
            paging = oldest.or(paging);
        }
        Err(BrokerError::Network(format!("OKX fills: more than {MAX_PAGES} pages in one pull")))
    }

    /// `GET /api/v5/trade/orders-pending`: every open spot order, paging back by `after=<ordId>`.
    async fn open_order_snapshot(&self, account_ref: &str) -> Result<Vec<VenueOrderUpdate>, BrokerError> {
        let mut out = Vec::new();
        let mut after = String::new();
        loop {
            let mut path = format!("/api/v5/trade/orders-pending?instType=SPOT&limit={PAGE_LIMIT}");
            if !after.is_empty() {
                path.push_str(&format!("&after={after}"));
            }
            let data = self.get(&path, true).await?;
            out.extend(parse_open_order_snapshot(&data, account_ref));
            let page = data.as_array().map_or(0, Vec::len);
            match data.as_array().and_then(|l| l.last()).and_then(|o| o["ordId"].as_str()) {
                Some(last) if page == PAGE_LIMIT => after = last.to_string(),
                _ => return Ok(out),
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
impl BrokerAdapter for OkxAdapter {
    async fn submit_order(&self, req: &BrokerOrderRequest) -> Result<BrokerOrderResponse, BrokerError> {
        let body = order_body(req)?;
        info!(symbol = %req.symbol, side = %req.side, "submitting order to OKX");
        let data = self.post("/api/v5/trade/order", &body).await?;
        match data[0]["ordId"].as_str() {
            Some(id) if !id.is_empty() => Ok(BrokerOrderResponse { external_order_id: id.to_string() }),
            _ => Err(BrokerError::BrokerRejected(format!("OKX accepted the order but returned no ordId: {data}"))),
        }
    }

    async fn cancel_order(&self, external_order_id: &str, symbol: &str) -> Result<(), BrokerError> {
        let body = serde_json::json!({ "instId": symbol, "ordId": external_order_id });
        self.post("/api/v5/trade/cancel-order", &body).await.map(|_| ())
    }

    /// GET /api/v5/trade/orders-pending — every open spot order, paging back by `after=<ordId>`.
    async fn open_orders(&self) -> Result<Vec<BrokerOpenOrder>, BrokerError> {
        let mut out = Vec::new();
        let mut after = String::new();
        loop {
            let mut path = format!("/api/v5/trade/orders-pending?instType=SPOT&limit={PAGE_LIMIT}");
            if !after.is_empty() {
                path.push_str(&format!("&after={after}"));
            }
            let data = self.get(&path, true).await?;
            out.extend(parse_open_orders(&data));
            let page = data.as_array().map_or(0, Vec::len);
            match data.as_array().and_then(|l| l.last()).and_then(|o| o["ordId"].as_str()) {
                Some(last) if page == PAGE_LIMIT => after = last.to_string(),
                _ => return Ok(out),
            }
        }
    }

    /// GET /api/v5/trade/order — one order by OKX id, open or recently closed.
    async fn order_status(
        &self,
        client_order_id: &str,
        external_order_id: &str,
        symbol: &str,
    ) -> Result<BrokerOrderState, BrokerError> {
        let by_client_id = external_order_id.is_empty();
        let data = match self.get(&order_lookup_path(symbol, client_order_id, external_order_id), true).await {
            Ok(data) => data,
            // Looked up by our id, "no such order" means the venue never had it.
            Err(e) if by_client_id && missing_order_state(&e).is_some() => return Err(BrokerError::NoSuchOrder(e.to_string())),
            Err(e) => return missing_order_state(&e).ok_or(e),
        };
        match data.as_array().and_then(|l| l.first()) {
            Some(order) => Ok(parse_order_state(order)),
            None => Err(BrokerError::NoSuchOrder(format!("OKX has no order {client_order_id}/{external_order_id}"))),
        }
    }
}

#[async_trait::async_trait]
impl DailyStats for OkxAdapter {
    /// GET /api/v5/market/tickers?instType=SPOT — public; one call covers every pair.
    async fn daily_stats(&self, symbols: &[String]) -> Result<Vec<(String, f64)>, BrokerError> {
        if symbols.is_empty() {
            return Ok(Vec::new());
        }
        let data = self.get("/api/v5/market/tickers?instType=SPOT", false).await?;
        Ok(parse_prev_prices(&data, symbols))
    }
}

#[async_trait::async_trait]
impl InstrumentProvider for OkxAdapter {
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
    fn a_timeout_or_busy_answer_is_not_a_rejection() {
        for code in ["50001", "50004", "50013"] {
            assert!(matches!(envelope(serde_json::json!({"code": code, "msg": "x", "data": []})), Err(BrokerError::Network(_))), "{code}");
        }
        assert!(matches!(envelope(serde_json::json!({"code": "51008", "msg": "insufficient", "data": []})), Err(BrokerError::BrokerRejected(_))));
        assert!(matches!(parse_body(504, "<html>timeout</html>"), Err(BrokerError::Network(_))));
    }

    #[test]
    fn open_orders_carry_the_venue_order_id() {
        let data = serde_json::json!([{ "ordId": "v-1", "clOrdId": "c1", "instId": "BTC-USDT", "accFillSz": "0" }]);
        assert_eq!(parse_open_orders(&data)[0].venue_order_id, "v-1");
    }

    #[test]
    fn order_status_looks_up_by_client_id_without_a_venue_id() {
        let id = "3f6b1c2e-8a4d-4e5f-9b21-1c2d3e4f5a6b";
        assert_eq!(order_lookup_path("BTC-USDT", id, "v-1"), "/api/v5/trade/order?instId=BTC-USDT&ordId=v-1");
        assert_eq!(order_lookup_path("BTC-USDT", id, ""), "/api/v5/trade/order?instId=BTC-USDT&clOrdId=3f6b1c2e8a4d4e5f9b211c2d3e4f5a6b");
    }

    const ORDER_ID: &str = "3f6b1c2e-8a4d-4e5f-9b21-1c2d3e4f5a6b";
    const CL_ORD_ID: &str = "3f6b1c2e8a4d4e5f9b211c2d3e4f5a6b";

    fn req(order_type: &str, side: &str, quantity: f64, limit_price: Option<f64>, tif: &str) -> BrokerOrderRequest {
        BrokerOrderRequest {
            order_id: ORDER_ID.into(),
            symbol: "BTC-USDT".into(),
            native_id: Some("BTC".into()),
            quantity,
            side: side.into(),
            order_type: order_type.into(),
            time_in_force: tif.into(),
            limit_price,
            external_account_ref: String::new(),
        }
    }

    fn raw(name: &str) -> serde_json::Value {
        let text = match name {
            "instruments_spot" => include_str!("testdata/instruments_spot.json"),
            "place_ok" => include_str!("testdata/place_ok.json"),
            "place_failed" => include_str!("testdata/place_failed.json"),
            "orders_pending" => include_str!("testdata/orders_pending.json"),
            "order_filled" => include_str!("testdata/order_filled.json"),
            "tickers_spot" => include_str!("testdata/tickers_spot.json"),
            other => panic!("no sample {other}"),
        };
        serde_json::from_str(text).unwrap()
    }

    fn sample(name: &str) -> serde_json::Value {
        envelope(raw(name)).unwrap()
    }

    // --- execution pull ---

    fn fills_data() -> serde_json::Value {
        envelope(serde_json::from_str(include_str!("testdata/fills.json")).unwrap()).unwrap()
    }

    fn ms(v: i64) -> DateTime<Utc> {
        Utc.timestamp_millis_opt(v).unwrap()
    }

    #[test]
    fn okx_fill_without_oms_id_is_kept() {
        let page = fills_rows(&fills_data(), "", DateTime::<Utc>::MIN_UTC, None);
        // The newest row was placed in the OKX app: its clOrdId is not an OMS id, and it is still a fill.
        let f = page.fills.iter().find(|f| f.exec_id == "123456791").expect("kept");
        assert_eq!(f.client_order_id.as_deref(), Some("manual1"));
        assert_eq!((f.venue_order_id.as_str(), f.symbol.as_str(), f.buy, f.qty, f.price), ("312269865356374099", "BTC-USDT", true, 1.0, 65000.0));
        // The OMS's own order reads back as its UUID.
        let ours = page.fills.iter().find(|f| f.exec_id == "123456790").unwrap();
        assert_eq!(ours.client_order_id.as_deref(), Some("3f6b1c2e-8a4d-4e5f-9b21-1c2d3e4f5a6b"));
    }

    #[test]
    fn okx_pull_page_maps_and_cursors() {
        let page = fills_rows(&fills_data(), "acct", DateTime::<Utc>::MIN_UTC, None);
        // The oldest row has no fillSz: skipped (and warned); the other two map.
        assert_eq!(page.fills.iter().map(|f| f.exec_id.as_str()).collect::<Vec<_>>(), ["123456791", "123456790"]);
        let f = &page.fills[0];
        assert_eq!((f.account_ref.as_str(), f.executed_at), ("acct", Some(ms(1700000060000))));
        assert_eq!(f.fee, Some((0.001, "BTC".to_string())), "OKX reports a charge as negative; the OMS books it positive");
        assert_eq!(page.fills[1].fee, Some((-0.01, "USDT".to_string())), "a rebate is negative");
        assert_eq!((page.rows, page.newest_bill.as_deref(), page.oldest_bill.as_deref(), page.done), (3, Some("900000000000000003"), Some("900000000000000001"), false));
        // Whole-scan result: the cursor is the newest bill, and a scan is never "more".
        let pulled = pull_result(vec![page]);
        assert_eq!(pulled.next, Cursor("900000000000000003".into()));
        assert!(!pulled.more);
    }

    #[test]
    fn a_cursor_stops_the_walk_at_its_bill_and_since_stops_it_at_old_rows() {
        let after = fills_rows(&fills_data(), "", DateTime::<Utc>::MIN_UTC, Some(900000000000000002));
        assert_eq!(after.fills.iter().map(|f| f.exec_id.as_str()).collect::<Vec<_>>(), ["123456791"]);
        assert!(after.done);
        let recent = fills_rows(&fills_data(), "", ms(1700000055000), None);
        assert_eq!(recent.fills.iter().map(|f| f.exec_id.as_str()).collect::<Vec<_>>(), ["123456791"]);
        assert!(recent.done);
        // Nothing new: no cursor to move.
        let none = fills_rows(&fills_data(), "", DateTime::<Utc>::MIN_UTC, Some(900000000000000003));
        assert!(none.fills.is_empty() && none.newest_bill.is_none());
        assert_eq!(pull_result(vec![none]), PullPage { fills: vec![], next: Cursor(String::new()), more: false });
    }

    #[test]
    fn okx_stream_and_pull_produce_the_same_exec_id_for_one_fill() {
        let push = serde_json::json!({"arg": {"channel": "orders"}, "data": [{
            "instId": "BTC-USDT", "ordId": "1", "clOrdId": "3f6b1c2e8a4d4e5f9b211c2d3e4f5a6b", "state": "partially_filled", "side": "buy", "ordType": "limit",
            "sz": "0.001", "px": "65000", "fillSz": "0.0004", "fillPx": "65000", "tradeId": "123456789", "accFillSz": "0.0004",
            "fillFee": "-0.026", "fillFeeCcy": "USDT", "fillTime": "1700000000000" }]});
        let VEvent::Fill(s) = &crate::connectors::okx::stream::venue_events(&push)[0] else { panic!("a fill") };
        let row = serde_json::json!({ "instId": "BTC-USDT", "tradeId": "123456789", "ordId": "1", "clOrdId": "3f6b1c2e8a4d4e5f9b211c2d3e4f5a6b", "billId": "5",
            "fillPx": "65000", "fillSz": "0.0004", "side": "buy", "feeCcy": "USDT", "fee": "-0.026", "ts": "1700000000000" });
        let p = fills_rows(&serde_json::json!([row]), "", DateTime::<Utc>::MIN_UTC, None);
        assert_eq!(s.exec_id, p.fills[0].exec_id);
        assert_eq!(s.exec_id, "123456789");
        assert_eq!(s, &p.fills[0], "the whole fill is the same from both paths");
    }

    #[test]
    fn open_order_snapshot_maps_working_orders_and_hints_are_one_year() {
        let snap = parse_open_order_snapshot(&sample("orders_pending"), "acct");
        assert_eq!(snap.len(), 2);
        let o = &snap[0];
        assert_eq!((o.venue_order_id.as_str(), o.client_order_id.as_deref(), o.account_ref.as_str(), o.buy, o.qty, o.cum_qty), ("312269865356374016", Some("3f6b1c2e-8a4d-4e5f-9b21-1c2d3e4f5a6b"), "acct", true, 0.001, 0.0005));
        assert_eq!(o.status, VenueOrderStatus::Working);
        assert_eq!((snap[1].client_order_id.clone(), snap[1].buy), (None, false));
        assert_eq!(
            ExecutionSource::hints(&OkxAdapter::new("k".into(), "s".into(), "p".into(), "PAPER")),
            PullHints { intraday_secs: 60, max_history: Some(chrono::Duration::days(365)), per_symbol: false }
        );
    }

    #[test]
    fn rest_signature_matches_a_known_answer() {
        // printf '%s' '2020-12-08T09:08:57.715ZGET/api/v5/account/config' | openssl dgst -sha256 -hmac secret -binary | base64
        let a = OkxAdapter::new("key".into(), "secret".into(), "pass".into(), "PAPER");
        assert_eq!(a.sign("2020-12-08T09:08:57.715ZGET/api/v5/account/config"), "zblex2fylT7AdMYc3o/Cyc3e7uO62CXRmoRXKd24DKY=");
    }

    #[test]
    fn paper_is_demo_trading_and_live_is_production() {
        let paper = OkxAdapter::new("k".into(), "s".into(), "p".into(), "PAPER");
        assert!(paper.simulated);
        assert_eq!(paper.private_ws_url(), "wss://wspap.okx.com/ws/v5/private");
        let live = OkxAdapter::new("k".into(), "s".into(), "p".into(), "LIVE");
        assert!(!live.simulated);
        assert_eq!(live.private_ws_url(), "wss://ws.okx.com/ws/v5/private");
    }

    #[test]
    fn the_order_uuid_travels_as_a_32_char_cl_ord_id_and_back() {
        assert_eq!(client_order_id(ORDER_ID), CL_ORD_ID);
        assert_eq!(oms_order_id(CL_ORD_ID).unwrap().to_string(), ORDER_ID);
        assert_eq!(oms_order_id("manual1"), None);
        assert_eq!(oms_order_id(""), None);
    }

    #[test]
    fn envelope_ok_returns_data() {
        let data = sample("place_ok");
        assert_eq!(data[0]["ordId"], "312269865356374016");
    }

    #[test]
    fn a_failed_order_surfaces_its_s_msg() {
        let e = envelope(raw("place_failed")).unwrap_err();
        assert!(matches!(e, BrokerError::BrokerRejected(ref m) if m.contains("Insufficient USDT balance") && m.contains("51008")), "{e}");
    }

    #[test]
    fn a_non_zero_s_code_under_code_zero_is_still_a_rejection() {
        let e = envelope(serde_json::json!({"code":"0","msg":"","data":[{"ordId":"","sCode":"51400","sMsg":"Cancellation failed"}]}))
            .unwrap_err();
        assert!(e.to_string().contains("Cancellation failed"), "{e}");
    }

    #[test]
    fn an_envelope_error_without_data_carries_msg() {
        let e = envelope(serde_json::json!({"code":"50113","msg":"Invalid Sign","data":[]})).unwrap_err();
        assert!(e.to_string().contains("Invalid Sign") && e.to_string().contains("50113"), "{e}");
    }

    #[test]
    fn spot_instruments_are_okx_venue_rows_with_lot_and_tick_sizes() {
        let list = parse_spot_instruments(&sample("instruments_spot"));
        let btc = list.iter().find(|i| i.broker_symbol == "BTC-USDT").unwrap();
        assert_eq!(btc.definition.venue, "OKX");
        assert_eq!(btc.broker_exchange.as_deref(), Some("OKX"));
        assert_eq!(btc.definition.currency, "USDT");
        assert_eq!(btc.definition.native_id.as_deref(), Some("BTC"));
        assert_eq!(btc.definition.instrument_class, "SPOT");
        assert_eq!(btc.definition.price_increment, 0.1);
        assert_eq!(btc.definition.price_precision, 1);
        assert_eq!(btc.definition.size_increment, 0.00000001);
        assert_eq!(btc.min_quantity, Some(0.00001));
        assert_eq!(btc.max_quantity, Some(9999999999.0));
        assert!(btc.is_tradeable);
        // An empty max is no limit, not zero.
        assert_eq!(list.iter().find(|i| i.broker_symbol == "HALT-USDT").unwrap().max_quantity, None);
        // A pair whose state is not "live" is listed but not tradeable.
        assert!(!list.iter().find(|i| i.broker_symbol == "HALT-USDT").unwrap().is_tradeable);
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn market_buy_is_sized_in_the_base_currency() {
        let b = order_body(&req("market", "buy", 0.001, None, "day")).unwrap();
        assert_eq!(b["instId"], "BTC-USDT");
        assert_eq!(b["tdMode"], "cash");
        assert_eq!(b["side"], "buy");
        assert_eq!(b["ordType"], "market");
        assert_eq!(b["sz"], "0.001");
        assert_eq!(b["tgtCcy"], "base_ccy");
        assert_eq!(b["clOrdId"], CL_ORD_ID);
        assert!(b.get("px").is_none());
    }

    #[test]
    fn limit_time_in_force_picks_the_order_type() {
        let b = order_body(&req("limit", "sell", 1.5, Some(65000.5), "day")).unwrap();
        assert_eq!(b["side"], "sell");
        assert_eq!(b["ordType"], "limit");
        assert_eq!(b["px"], "65000.5");
        assert_eq!(b["sz"], "1.5");
        assert!(b.get("tgtCcy").is_none());
        assert_eq!(order_body(&req("limit", "buy", 1.0, Some(1.0), "gtc")).unwrap()["ordType"], "limit");
        assert_eq!(order_body(&req("limit", "buy", 1.0, Some(1.0), "ioc")).unwrap()["ordType"], "ioc");
        assert_eq!(order_body(&req("limit", "buy", 1.0, Some(1.0), "fok")).unwrap()["ordType"], "fok");
        assert!(order_body(&req("limit", "buy", 1.0, None, "gtc")).is_err());
        assert!(order_body(&req("stop", "buy", 1.0, None, "gtc")).is_err());
    }

    #[test]
    fn order_state_maps_every_okx_state() {
        assert_eq!(order_outcome("filled"), BrokerOrderOutcome::Filled);
        assert_eq!(order_outcome("canceled"), BrokerOrderOutcome::Canceled);
        assert_eq!(order_outcome("mmp_canceled"), BrokerOrderOutcome::Canceled);
        assert_eq!(order_outcome("live"), BrokerOrderOutcome::Working);
        assert_eq!(order_outcome("partially_filled"), BrokerOrderOutcome::Working);
    }

    #[test]
    fn open_orders_carry_the_oms_order_id_and_filled_size() {
        let open = parse_open_orders(&sample("orders_pending"));
        assert_eq!(open.len(), 2);
        assert_eq!(open[0].client_order_id, ORDER_ID);
        assert_eq!(open[0].symbol, "BTC-USDT");
        assert_eq!(open[0].executed_qty, 0.0005);
        // An order placed outside the OMS keeps its raw clOrdId (reconcile reports it as an orphan).
        assert_eq!(open[1].client_order_id, "");
    }

    #[test]
    fn order_state_reads_acc_fill_sz_and_avg_px() {
        let st = parse_order_state(&sample("order_filled")[0]);
        assert_eq!(st.outcome, BrokerOrderOutcome::Filled);
        assert_eq!(st.executed_qty, 0.001);
        assert_eq!(st.avg_px, 65000.0);
        let none = parse_order_state(&serde_json::json!({"state":"canceled","accFillSz":"0","avgPx":""}));
        assert_eq!((none.outcome, none.executed_qty, none.avg_px), (BrokerOrderOutcome::Canceled, 0.0, 0.0));
    }

    #[test]
    fn previous_price_is_open24h_and_a_blank_one_is_skipped() {
        let prev = parse_prev_prices(&sample("tickers_spot"), &["BTC-USDT".to_string(), "BAD-USDT".to_string()]);
        assert_eq!(prev, vec![("BTC-USDT".to_string(), 64000.0)]);
    }

    // OKX keeps a cancelled order with no fills queryable for about 2 hours; after
    // that /trade/order answers 51603. Reconcile must still close it.
    #[test]
    fn an_order_okx_no_longer_knows_was_cancelled_with_nothing_filled() {
        let gone = envelope(serde_json::json!({"code":"51603","msg":"Order does not exist","data":[]})).unwrap_err();
        let st = missing_order_state(&gone).expect("51603 means cancelled unfilled");
        assert_eq!((st.outcome, st.executed_qty, st.avg_px), (BrokerOrderOutcome::Canceled, 0.0, 0.0));
        let other = envelope(serde_json::json!({"code":"50011","msg":"Too many requests","data":[]})).unwrap_err();
        assert!(missing_order_state(&other).is_none());
    }

    #[test]
    fn decimal_drops_float_noise_and_trailing_zeros() {
        assert_eq!(decimal(0.1 + 0.2), "0.3");
        assert_eq!(decimal(2.0), "2");
    }
}
