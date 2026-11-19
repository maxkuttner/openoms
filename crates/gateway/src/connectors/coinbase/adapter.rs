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

use chrono::{DateTime, Utc};
use tracing::{info, warn};

use crate::venue_event::{Cursor, ExecutionSource, PullHints, PullPage, VenueFill, VenueOrderStatus, VenueOrderUpdate};

use crate::adapter::{BrokerAdapter, BrokerError, BrokerInstrument, BrokerOrderRequest, BrokerOrderResponse, InstrumentProvider};
use crate::md::daily_stats::DailyStats;
use crate::contract::{BrokerOpenOrder, BrokerOrderOutcome, BrokerOrderState};

/// Quantities and prices as plain decimals without float noise — Bybit's rule.
pub(crate) use crate::connectors::bybit::adapter::decimal;

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
    let status = resp.status().as_u16();
    let text = resp.text().await.map_err(|e| BrokerError::Network(e.to_string()))?;
    parse_body(status, &text)
}

/// A response body: JSON on success; an unreadable success or a 5xx/429 says
/// nothing about the request (`Network`); any other failure carries Coinbase's
/// message as a rejection.
pub(crate) fn parse_body(status: u16, text: &str) -> Result<serde_json::Value, BrokerError> {
    let body: Option<serde_json::Value> = serde_json::from_str(text).ok();
    if (200..300).contains(&status) {
        return body.ok_or_else(|| BrokerError::Network(format!("HTTP {status}: not JSON: {text}")));
    }
    let message = body
        .as_ref()
        .and_then(|b| b["message"].as_str().or(b["error"].as_str()).map(String::from))
        .unwrap_or_else(|| text.to_string());
    Err(crate::adapter::http_failure(status, format!("HTTP {status}: {message}")))
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
                venue_order_id: o["order_id"].as_str().unwrap_or_default().to_string(),
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

fn iso_time(v: &serde_json::Value) -> Option<DateTime<Utc>> {
    v.as_str().and_then(|s| s.parse().ok())
}

/// The client id an order carries into the OMS: as Coinbase has it, `None` when empty.
fn client_id(v: &serde_json::Value) -> Option<String> {
    v.as_str().filter(|c| !c.is_empty()).map(str::to_string)
}

/// Coinbase's order `status` as the venue's view; `None` for one the OMS does
/// not know (the caller warns).
pub(crate) fn venue_status(status: &str, reject_reason: &str) -> Option<VenueOrderStatus> {
    Some(match status {
        "OPEN" | "PENDING" | "QUEUED" | "CANCEL_QUEUED" => VenueOrderStatus::Working,
        "FILLED" => VenueOrderStatus::Filled,
        "CANCELLED" => VenueOrderStatus::Canceled,
        "EXPIRED" => VenueOrderStatus::Expired,
        "FAILED" => VenueOrderStatus::Rejected { reason: if reject_reason.is_empty() { "FAILED".into() } else { reject_reason.to_string() } },
        _ => return None,
    })
}

/// One order of the `user` channel as the venue's view of it. The channel
/// reports the order's cumulative state; the quantity is what is filled plus
/// what is left.
pub(crate) fn order_update_from_push(o: &serde_json::Value, account_ref: &str) -> Option<VenueOrderUpdate> {
    let order_type = o["order_type"].as_str().unwrap_or("").to_string();
    let cum = num(&o["cumulative_quantity"]).unwrap_or(0.0);
    Some(VenueOrderUpdate {
        venue_order_id: o["order_id"].as_str().filter(|i| !i.is_empty())?.to_string(),
        client_order_id: client_id(&o["client_order_id"]),
        account_ref: account_ref.to_string(),
        symbol: o["product_id"].as_str().unwrap_or("").to_string(),
        buy: o["order_side"].as_str() == Some("BUY"),
        limit_px: if order_type == "MARKET" { None } else { num(&o["limit_price"]).filter(|p| *p != 0.0) },
        order_type,
        qty: cum + num(&o["leaves_quantity"]).unwrap_or(0.0),
        status: venue_status(o["status"].as_str()?, o["reject_reason"].as_str().unwrap_or(""))?,
        cum_qty: cum,
        ts: None,
        created_at: iso_time(&o["creation_time"]).or_else(|| iso_time(&o["created_time"])),
    })
}

/// The order's base size from its `order_configuration` (`{"limit_limit_gtc": {"base_size": ..}}`).
fn configured_size(o: &serde_json::Value) -> Option<f64> {
    o["order_configuration"].as_object()?.values().find_map(|c| num(&c["base_size"]))
}

/// `orders/historical/batch?order_status=OPEN` as the venue's view of each open order.
pub(crate) fn parse_open_order_snapshot(body: &serde_json::Value, account_ref: &str) -> Vec<VenueOrderUpdate> {
    body["orders"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| {
            let order_type = o["order_type"].as_str().unwrap_or("").to_string();
            let cum = num(&o["filled_size"]).unwrap_or(0.0);
            Some(VenueOrderUpdate {
                venue_order_id: o["order_id"].as_str().filter(|i| !i.is_empty())?.to_string(),
                client_order_id: client_id(&o["client_order_id"]),
                account_ref: account_ref.to_string(),
                symbol: o["product_id"].as_str()?.to_string(),
                buy: o["side"].as_str()? == "BUY",
                limit_px: None,
                order_type,
                qty: configured_size(o).unwrap_or(cum),
                status: venue_status(o["status"].as_str()?, "")?,
                cum_qty: cum,
                ts: None,
                created_at: iso_time(&o["created_time"]),
            })
        })
        .collect()
}

const FILLS_LIMIT: usize = 250;
/// Pages one call may walk before it gives up rather than advance its cursor
/// past rows it did not read.
const MAX_PAGES: usize = 400;

/// One page of `orders/historical/fills` rows, read against a scan's bounds.
#[derive(Debug, Default)]
pub(crate) struct FillsRows {
    pub fills: Vec<VenueFill>,
    /// The newest row seen (by trade time) and its trade id, filtered or not.
    pub newest: Option<(DateTime<Utc>, String)>,
    /// Every timed row is older than the resume threshold.
    pub all_older: bool,
    /// The rows are in ascending time order (first before last).
    pub ascending: bool,
}

impl FillsRows {
    /// Nothing further back can be new: the page is entirely behind the
    /// cursor's margin and rows run newest first. An ascending page proves
    /// nothing about the pages after it, so it never stops the walk.
    pub fn stop(&self) -> bool {
        self.all_older && !self.ascending
    }
}

/// How far behind its newest trade a resume reads, for fills Coinbase indexes late.
const RESUME_MARGIN_MS: i64 = 30 * 60 * 1000;

/// The resume cursor: `"{trade_time_ms}:{trade_id}"` of the newest fill seen.
pub(crate) fn parse_cursor(c: &Cursor) -> Option<(DateTime<Utc>, String)> {
    let (ms, id) = c.0.split_once(':')?;
    Some((chrono::TimeZone::timestamp_millis_opt(&Utc, ms.parse().ok()?).single()?, id.to_string()))
}

/// Rows of `orders/historical/fills` as fills. Rows executed before `since`, or
/// before `threshold` (the cursor's time less the resume margin), are dropped;
/// a row missing a field is warned about and skipped. A fill's `size` is in base
/// units unless `size_in_quote`. Fills have no client order id: they join their
/// order by `order_id`. The commission is in the product's quote currency.
pub(crate) fn fills_rows(body: &serde_json::Value, account_ref: &str, since: DateTime<Utc>, threshold: Option<DateTime<Utc>>) -> FillsRows {
    let mut out = FillsRows::default();
    let rows = body["fills"].as_array().map(Vec::as_slice).unwrap_or_default();
    let times: Vec<DateTime<Utc>> = rows.iter().filter_map(|d| d["trade_time"].as_str().and_then(|s| s.parse().ok())).collect();
    out.ascending = times.len() >= 2 && times.first() < times.last();
    out.all_older = threshold.is_some_and(|t| !times.is_empty() && times.iter().all(|at| *at < t));
    for d in rows {
        let trade_id = d["trade_id"].as_str().unwrap_or("");
        let at = d["trade_time"].as_str().and_then(|s| s.parse::<DateTime<Utc>>().ok());
        if let (false, Some(at)) = (trade_id.is_empty(), at) {
            if out.newest.as_ref().is_none_or(|(n, _)| at > *n) {
                out.newest = Some((at, trade_id.to_string()));
            }
        }
        if at.is_some_and(|at| at < since || threshold.is_some_and(|t| at < t)) {
            continue;
        }
        match fill_of(d, trade_id, at, account_ref) {
            Ok(f) => out.fills.push(f),
            Err(missing) => warn!(trade_id, order_id = d["order_id"].as_str().unwrap_or(""), missing, "Coinbase fills: row skipped"),
        }
    }
    out
}

fn fill_of(d: &serde_json::Value, trade_id: &str, at: Option<DateTime<Utc>>, account_ref: &str) -> Result<VenueFill, &'static str> {
    if trade_id.is_empty() {
        return Err("trade_id");
    }
    let venue_order_id = d["order_id"].as_str().filter(|o| !o.is_empty()).ok_or("order_id")?;
    let price = num(&d["price"]).filter(|p| *p > 0.0).ok_or("price")?;
    let size = num(&d["size"]).ok_or("size")?;
    let qty = if d["size_in_quote"].as_bool() == Some(true) { size / price } else { size };
    let product = d["product_id"].as_str().filter(|p| !p.is_empty()).ok_or("product_id")?;
    let buy = match d["side"].as_str() {
        Some("BUY") => true,
        Some("SELL") => false,
        _ => return Err("side"),
    };
    let fee = num(&d["commission"])
        .filter(|c| *c != 0.0)
        .and_then(|c| product.rsplit_once('-').map(|(_, quote)| quote).filter(|q| !q.is_empty()).map(|q| (c, q.to_string())));
    Ok(VenueFill {
        exec_id: trade_id.to_string(),
        venue_order_id: venue_order_id.to_string(),
        client_order_id: None,
        account_ref: account_ref.to_string(),
        symbol: product.to_string(),
        buy,
        qty,
        price,
        fee,
        executed_at: at,
    })
}

/// A whole walk as one pull page: the cursor is the newest trade seen
/// (`"{time_ms}:{trade_id}"`), and nothing is left to page. Nothing seen keeps
/// the reconciler's cursor.
pub(crate) fn pull_result(pages: Vec<FillsRows>) -> PullPage {
    let newest = pages
        .iter()
        .filter_map(|p| p.newest.clone())
        .max_by_key(|(at, _)| *at)
        .map(|(at, id)| format!("{}:{id}", at.timestamp_millis()))
        .unwrap_or_default();
    PullPage { fills: pages.into_iter().flat_map(|p| p.fills).collect(), next: Cursor(newest), more: false }
}

#[async_trait::async_trait]
impl ExecutionSource for CoinbaseAdapter {
    /// Pages `orders/historical/fills` (newest first, asked for explicitly) with
    /// the API's own cursor, whole, until a page lies entirely 30 minutes behind
    /// the resume cursor's trade time, or the history ends. The API cursor walks
    /// into the past, so it cannot be resumed from; the resume token is instead
    /// the newest trade's time and id, and one call returns everything new.
    /// Re-reads within the margin are exec-id duplicates.
    async fn fills_since(
        &self,
        account_ref: &str,
        scope: &str,
        from: Option<&Cursor>,
        since: DateTime<Utc>,
    ) -> Result<PullPage, BrokerError> {
        let threshold = from.and_then(parse_cursor).map(|(at, _)| at - chrono::Duration::milliseconds(RESUME_MARGIN_MS));
        let mut pages = Vec::new();
        let mut api_cursor = String::new();
        for _ in 0..MAX_PAGES {
            let mut query = format!("limit={FILLS_LIMIT}&sort_by=TRADE_TIME");
            if since > DateTime::<Utc>::MIN_UTC {
                query.push_str(&format!("&start_sequence_timestamp={}", urlencoding_minimal(&since.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))));
            }
            if !api_cursor.is_empty() {
                query.push_str(&format!("&cursor={}", urlencoding_minimal(&api_cursor)));
            }
            let body = self.get("/orders/historical/fills", &query, true).await?;
            let page = fills_rows(&body, account_ref, since, threshold);
            let rows = body["fills"].as_array().map_or(0, Vec::len);
            let stopped = page.stop();
            pages.push(page);
            api_cursor = body["cursor"].as_str().unwrap_or("").to_string();
            if stopped || api_cursor.is_empty() || rows == 0 {
                return Ok(pull_result(pages));
            }
        }
        Err(BrokerError::Network(format!(
            "Coinbase fills pull for scope \"{scope}\" read more than {MAX_PAGES} pages without reaching its bound; \
             this scope stalls until that is resolved (a shorter gap, or a manual EOD sweep)"
        )))
    }

    /// `GET /orders/historical/batch?order_status=OPEN`, following the cursor.
    async fn open_order_snapshot(&self, account_ref: &str) -> Result<Vec<VenueOrderUpdate>, BrokerError> {
        let mut out = Vec::new();
        let mut cursor = String::new();
        loop {
            let mut query = "order_status=OPEN&limit=250".to_string();
            if !cursor.is_empty() {
                query.push_str(&format!("&cursor={}", urlencoding_minimal(&cursor)));
            }
            let body = self.get("/orders/historical/batch", &query, true).await?;
            out.extend(parse_open_order_snapshot(&body, account_ref));
            cursor = body["cursor"].as_str().unwrap_or("").to_string();
            if body["has_next"].as_bool() != Some(true) || cursor.is_empty() {
                return Ok(out);
            }
        }
    }

    async fn scopes(&self, _account_ref: &str) -> Result<Vec<String>, BrokerError> {
        Ok(vec![String::new()])
    }

    fn hints(&self) -> PullHints {
        PullHints { intraday_secs: 60, max_history: None, per_symbol: false }
    }
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
        if external_order_id.is_empty() {
            // Advanced Trade reads one order only by its own id.
            return Err(BrokerError::NotConfigured("Coinbase cannot look an order up by client order id".into()));
        }
        let body = self.get(&format!("/orders/historical/{external_order_id}"), "", true).await?;
        Ok(parse_order_state(&body["order"]))
    }
}

#[async_trait::async_trait]
impl DailyStats for CoinbaseAdapter {
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

    #[test]
    fn an_unreadable_or_5xx_answer_is_not_a_rejection() {
        assert!(matches!(parse_body(200, "not json"), Err(BrokerError::Network(_))));
        assert!(matches!(parse_body(503, r#"{"message":"down"}"#), Err(BrokerError::Network(_))));
        assert!(matches!(parse_body(400, r#"{"message":"INVALID_SIZE"}"#), Err(BrokerError::BrokerRejected(_))));
    }

    #[test]
    fn open_orders_carry_the_venue_order_id() {
        let body = serde_json::json!({ "orders": [{ "order_id": "v-1", "client_order_id": "c-1", "product_id": "BTC-USD", "filled_size": "0" }] });
        assert_eq!(parse_open_orders(&body)[0].venue_order_id, "v-1");
    }
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
            "create_ok" => include_str!("testdata/create_ok.json"),
            "create_failed" => include_str!("testdata/create_failed.json"),
            "cancel_ok" => include_str!("testdata/cancel_ok.json"),
            "cancel_failed" => include_str!("testdata/cancel_failed.json"),
            "orders_open" => include_str!("testdata/orders_open.json"),
            "order_filled" => include_str!("testdata/order_filled.json"),
            other => panic!("no sample {other}"),
        };
        serde_json::from_str(text).unwrap()
    }

    // --- execution pull ---

    fn fills_body() -> serde_json::Value {
        serde_json::from_str(include_str!("testdata/fills.json")).unwrap()
    }

    #[test]
    fn coinbase_fill_without_oms_id_is_kept() {
        // Fills carry no client order id, and the first row is an order the OMS never placed.
        let rows = fills_rows(&fills_body(), "", DateTime::<Utc>::MIN_UTC, None);
        let f = rows.fills.iter().find(|f| f.exec_id == "t-300").expect("kept");
        assert_eq!((f.venue_order_id.as_str(), f.client_order_id.clone(), f.symbol.as_str(), f.buy, f.qty, f.price), ("99999999-0000-4000-8000-000000000009", None, "BTC-USD", false, 1.0, 65020.0));
    }

    #[test]
    fn coinbase_pull_page_maps_and_cursors() {
        let rows = fills_rows(&fills_body(), "acct", DateTime::<Utc>::MIN_UTC, None);
        // The oldest row has no size: skipped (and warned).
        assert_eq!(rows.fills.iter().map(|f| f.exec_id.as_str()).collect::<Vec<_>>(), ["t-300", "t-200"]);
        let f = &rows.fills[0];
        assert_eq!((f.account_ref.as_str(), f.fee.clone()), ("acct", Some((0.5, "USD".to_string()))));
        assert_eq!(f.executed_at, Some("2026-11-03T12:00:03.500Z".parse().unwrap()));
        // A size in quote currency is converted to base at the fill's price.
        assert_eq!(rows.fills[1].qty, 32.5 / 65000.0);
        assert_eq!(rows.newest.as_ref().map(|n| n.1.as_str()), Some("t-300"));
        let page = pull_result(vec![rows]);
        let t300: DateTime<Utc> = "2026-11-03T12:00:03.500Z".parse().unwrap();
        assert_eq!((page.next, page.more, page.fills.len()), (Cursor(format!("{}:t-300", t300.timestamp_millis())), false, 2));
    }

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn the_walk_stops_on_a_page_entirely_behind_the_cursor_margin_and_since_drops_old_rows() {
        // Threshold 12:00:02.5: only t-300 is at or after it, so the rest are dropped; the page is not all older.
        let page = fills_rows(&fills_body(), "", DateTime::<Utc>::MIN_UTC, Some(at("2026-11-03T12:00:02.500Z")));
        assert_eq!(page.fills.iter().map(|f| f.exec_id.as_str()).collect::<Vec<_>>(), ["t-300"]);
        assert!(!page.stop());
        let behind = fills_rows(&fills_body(), "", DateTime::<Utc>::MIN_UTC, Some(at("2026-11-03T12:30:00Z")));
        assert!(behind.fills.is_empty() && behind.stop());
        let recent = fills_rows(&fills_body(), "", at("2026-11-03T12:00:03Z"), None);
        assert_eq!(recent.fills.iter().map(|f| f.exec_id.as_str()).collect::<Vec<_>>(), ["t-300"]);
        assert!(!recent.stop());
        // Nothing seen: the cursor is left to the reconciler.
        let none = fills_rows(&serde_json::json!({ "fills": [] }), "", DateTime::<Utc>::MIN_UTC, None);
        assert_eq!(pull_result(vec![none]), PullPage { fills: vec![], next: Cursor(String::new()), more: false });
    }

    fn row(id: &str, t: &str) -> serde_json::Value {
        serde_json::json!({ "trade_id": id, "order_id": "o", "trade_time": t, "price": "1", "size": "1", "commission": "0", "product_id": "BTC-USD", "side": "BUY" })
    }

    #[test]
    fn coinbase_ascending_rows_do_not_lose_new_fills() {
        // Oldest first: page 1 is entirely behind the cursor, yet the new fill is on page 2.
        let threshold = Some(at("2026-11-03T12:00:00Z") - chrono::Duration::milliseconds(RESUME_MARGIN_MS));
        let page1 = fills_rows(&serde_json::json!({ "fills": [row("a", "2026-11-03T10:00:00Z"), row("b", "2026-11-03T10:05:00Z")] }), "", DateTime::<Utc>::MIN_UTC, threshold);
        assert!(page1.ascending && page1.all_older);
        assert!(!page1.stop(), "an ascending page never ends the walk");
        let page2 = fills_rows(&serde_json::json!({ "fills": [row("c", "2026-11-03T11:59:00Z"), row("new", "2026-11-03T12:05:00Z")] }), "", DateTime::<Utc>::MIN_UTC, threshold);
        assert_eq!(page2.fills.iter().map(|f| f.exec_id.as_str()).collect::<Vec<_>>(), ["c", "new"]);
        let page = pull_result(vec![page1, page2]);
        assert_eq!(page.next, Cursor(format!("{}:new", at("2026-11-03T12:05:00Z").timestamp_millis())));
        // And a descending page of old rows does end it.
        let desc = fills_rows(&serde_json::json!({ "fills": [row("b", "2026-11-03T10:05:00Z"), row("a", "2026-11-03T10:00:00Z")] }), "", DateTime::<Utc>::MIN_UTC, threshold);
        assert!(desc.stop());
    }

    #[test]
    fn coinbase_late_indexed_older_fill_within_margin_is_pulled() {
        // The cursor's trade is at 12:00; a fill stamped 11:50 only now appeared.
        let (cursor_at, id) = parse_cursor(&Cursor(format!("{}:t-1", at("2026-11-03T12:00:00Z").timestamp_millis()))).unwrap();
        assert_eq!(id, "t-1");
        let threshold = Some(cursor_at - chrono::Duration::milliseconds(RESUME_MARGIN_MS));
        let body = serde_json::json!({ "fills": [row("t-2", "2026-11-03T12:01:00Z"), row("t-1", "2026-11-03T12:00:00Z"), row("late", "2026-11-03T11:50:00Z"), row("old", "2026-11-03T11:00:00Z")] });
        let page = fills_rows(&body, "", DateTime::<Utc>::MIN_UTC, threshold);
        assert_eq!(page.fills.iter().map(|f| f.exec_id.as_str()).collect::<Vec<_>>(), ["t-2", "t-1", "late"]);
        assert!(!page.stop());
    }

    #[test]
    fn coinbase_stream_and_pull_produce_the_same_exec_id_for_one_fill() {
        // The user channel reports an order's cumulative state with no trade id or
        // per-fill size, so any exec id it made would differ from the fills endpoint's
        // `trade_id`. A partial fill is therefore an order update only (its cum_qty
        // makes ingest ask for a pull); the fill itself, and its id, come from the pull.
        let msg: serde_json::Value = serde_json::from_str(include_str!("testdata/ws_user_partial.json")).unwrap();
        let events = crate::connectors::coinbase::stream::venue_events(&msg);
        assert!(events.iter().all(|e| matches!(e, crate::venue_event::VenueEvent::Order(_))), "{events:?}");
        let rows = fills_rows(&fills_body(), "", DateTime::<Utc>::MIN_UTC, None);
        assert!(rows.fills.iter().all(|f| !f.exec_id.is_empty()));
    }

    #[test]
    fn open_order_snapshot_maps_working_orders_and_hints_are_unbounded() {
        let snap = parse_open_order_snapshot(&sample("orders_open"), "acct");
        assert_eq!(snap.len(), 1);
        let o = &snap[0];
        assert_eq!((o.venue_order_id.as_str(), o.client_order_id.as_deref(), o.account_ref.as_str(), o.buy, o.cum_qty), ("11111111-2222-4333-8444-555555555555", Some("3f6b1c2e-8a4d-4e5f-9b21-1c2d3e4f5a6b"), "acct", true, 0.0005));
        assert_eq!(o.status, VenueOrderStatus::Working);
        assert_eq!(ExecutionSource::hints(&CoinbaseAdapter::new("k".into(), &test_pem()).unwrap()), PullHints { intraday_secs: 60, max_history: None, per_symbol: false });
    }

    fn test_pem() -> String {
        p256::SecretKey::random(&mut rand::thread_rng()).to_sec1_pem(p256::pkcs8::LineEnding::LF).unwrap().to_string()
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
            serde_json::from_str(include_str!("testdata/products_spot.json")).unwrap();
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

    #[test]
    fn coinbase_snapshot_order_carries_created_time() {
        let mut body = sample("orders_open");
        body["orders"][0]["created_time"] = serde_json::json!("2026-11-03T11:59:00Z");
        let snap = parse_open_order_snapshot(&body, "acct");
        assert_eq!(snap[0].created_at, Some("2026-11-03T11:59:00Z".parse().unwrap()));
    }
}
