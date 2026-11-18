use std::collections::BTreeSet;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use dataprovider::{DerivativeDef, Identifiers, InstrumentDef, OptionKind};
use reqwest::Client;
use serde::Serialize;
use tracing::{info, warn};

use crate::adapter::{
    BrokerAdapter, BrokerError, BrokerInstrument, BrokerOrderRequest,
    BrokerOrderResponse, InstrumentProvider,
};
use crate::md::daily_stats::DailyStats;
use crate::venue_event::{Cursor, ExecutionSource, PullHints, PullPage, VenueFill, VenueOrderStatus, VenueOrderUpdate};
use crate::contract::{BrokerOpenOrder, BrokerOrderOutcome, BrokerOrderState};

/// Alpaca's market-data API is a separate host from the trading API
/// (`api.alpaca.markets`/`paper-api.alpaca.markets`) and is not
/// environment-specific — the same data host serves both live and paper
/// accounts.
const ALPACA_DATA_URL: &str = "https://data.alpaca.markets";

/// Pulls `prevDailyBar.c` (previous close) out of a
/// `GET /v2/stocks/snapshots` response. A symbol with no `prevDailyBar` (a
/// brand-new listing, or one Alpaca hasn't backfilled yet) is skipped, never
/// reported as a zero previous close.
fn parse_alpaca_snapshots(body: &serde_json::Value) -> Vec<(String, f64)> {
    let Some(map) = body.as_object() else { return Vec::new() };
    map.iter()
        .filter_map(|(symbol, snapshot)| {
            let prev_close = snapshot.get("prevDailyBar")?.get("c")?.as_f64()?;
            Some((symbol.clone(), prev_close))
        })
        .collect()
}

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

/// An Alpaca API key pair.
/// Stored as `{"kind":"Alpaca", …}` (see [`crate::credentials::BrokerCredentials`]).
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct AlpacaCredentials {
    pub key: String,
    pub secret: String,
}

impl AlpacaCredentials {
    pub const KIND: &'static str = "Alpaca";
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
        // Matches BinanceAdapter's client (see binance.rs): the poller now
        // calls every registered adapter in sequence, so a hung request here
        // would stall every later adapter's refresh that poll cycle.
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        Self {
            client,
            api_key,
            api_secret,
            base_url,
        }
    }

    pub async fn get_order(&self, external_order_id: &str) -> Result<serde_json::Value, BrokerError> {
        self.get_order_at(&order_lookup_path("", external_order_id)).await
    }

    /// One order at `path` (see `order_lookup_path`); a 404 is `NoSuchOrder`.
    async fn get_order_at(&self, path: &str) -> Result<serde_json::Value, BrokerError> {
        let url = format!("{}{}", self.base_url, path);
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
        } else if resp.status() == reqwest::StatusCode::NOT_FOUND {
            Err(BrokerError::NoSuchOrder(resp.text().await.unwrap_or_default()))
        } else {
            Err(BrokerError::BrokerRejected(resp.text().await.unwrap_or_default()))
        }
    }

    /// `GET /v2/account` — the cheapest authenticated call Alpaca offers, and
    /// the credential test used by `credentials_api::test_broker`: read-only,
    /// no side effects, sub-second, and a wrong key fails it immediately.
    pub async fn get_account(&self) -> Result<serde_json::Value, BrokerError> {
        let url = format!("{}/v2/account", self.base_url);
        let resp = self.get_json(&url).send().await.map_err(|e| BrokerError::Network(e.to_string()))?;
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
    ///
    /// `expiration_date_gte` is not a filter here — it is what makes the endpoint
    /// return the whole chain. Without it Alpaca answers with only the nearest one or
    /// two expiries and no `next_page_token`, so the pagination loop below sees a
    /// complete response and the far month/LEAPS contracts never arrive. Pinning it
    /// to today asks for everything still live; already-expired contracts are dropped
    /// again at ingest (`setup::catalog`), which uses the same UTC date.
    pub async fn list_option_contracts(
        &self,
        underlyings: &[String],
    ) -> Result<Vec<BrokerInstrument>, BrokerError> {
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;
        let today = chrono::Utc::now().date_naive();
        loop {
            let mut url = format!(
                "{}/v2/options/contracts?status=active&limit=10000\
                 &expiration_date_gte={today}&underlying_symbols={}",
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
            let status = response.status().as_u16();
            let err_body = response.text().await.unwrap_or_default();
            Err(crate::adapter::http_failure(status, err_body))
        }
    }

    /// GET /v2/orders?status=open — the snapshot side of order reconciliation, every
    /// open order in one call. `client_order_id` is the UUID we sent.
    async fn open_orders(&self) -> Result<Vec<BrokerOpenOrder>, BrokerError> {
        let url = format!("{}{OPEN_ORDERS_PATH}", self.base_url);
        let resp = self.get_json(&url).send().await.map_err(|e| BrokerError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(BrokerError::BrokerRejected(resp.text().await.unwrap_or_default()));
        }
        let orders: serde_json::Value = resp.json().await.map_err(|e| BrokerError::Network(e.to_string()))?;
        Ok(parse_open_orders(&orders))
    }

    /// One order's current state via GET /v2/orders/{id}, normalized for the shell.
    async fn order_status(
        &self,
        client_order_id: &str,
        external_order_id: &str,
        _symbol: &str,
    ) -> Result<BrokerOrderState, BrokerError> {
        let order = self.get_order_at(&order_lookup_path(client_order_id, external_order_id)).await?;
        let executed_qty: f64 =
            order["filled_qty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let avg_px: f64 =
            order["filled_avg_price"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let outcome = match order["status"].as_str().unwrap_or("") {
            "filled" => BrokerOrderOutcome::Filled,
            "canceled" => BrokerOrderOutcome::Canceled,
            "rejected" | "expired" => BrokerOrderOutcome::Rejected,
            _ => BrokerOrderOutcome::Working, // new / partially_filled / accepted — still live
        };
        Ok(BrokerOrderState { outcome, executed_qty, avg_px })
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
}

#[async_trait::async_trait]
impl DailyStats for AlpacaAdapter {
    async fn daily_stats(&self, symbols: &[String]) -> Result<Vec<(String, f64)>, BrokerError> {
        if symbols.is_empty() {
            return Ok(Vec::new());
        }
        let url = format!(
            "{}/v2/stocks/snapshots?symbols={}",
            ALPACA_DATA_URL,
            symbols.join(",")
        );
        let resp = self
            .client
            .get(&url)
            .header("APCA-API-KEY-ID", &self.api_key)
            .header("APCA-API-SECRET-KEY", &self.api_secret)
            .send()
            .await
            .map_err(|e| BrokerError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(BrokerError::BrokerRejected(format!(
                "snapshot request failed: {}",
                resp.status()
            )));
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| BrokerError::Network(e.to_string()))?;
        Ok(parse_alpaca_snapshots(&body))
    }
}

/// Every open order in one page (Alpaca's default page is 50).
pub(crate) const OPEN_ORDERS_PATH: &str = "/v2/orders?status=open&limit=500";

/// Where one order is read: by Alpaca's id when known, else by the client order
/// id we sent (an order whose routing never got recorded).
pub(crate) fn order_lookup_path(client_order_id: &str, external_order_id: &str) -> String {
    if external_order_id.is_empty() {
        format!("/v2/orders:by_client_order_id?client_order_id={client_order_id}")
    } else {
        format!("/v2/orders/{external_order_id}")
    }
}

/// `GET /v2/orders?status=open` as snapshot rows.
pub(crate) fn parse_open_orders(orders: &serde_json::Value) -> Vec<BrokerOpenOrder> {
    orders
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| {
            Some(BrokerOpenOrder {
                client_order_id: o["client_order_id"].as_str()?.to_string(),
                venue_order_id: o["id"].as_str().unwrap_or_default().to_string(),
                symbol: o["symbol"].as_str()?.to_string(),
                executed_qty: o["filled_qty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
            })
        })
        .collect()
}

/// Activities per page; Alpaca's maximum.
const ACTIVITIES_PAGE: usize = 100;

/// `GET /v2/account/activities` parameters: ascending from the cursor (the last
/// activity id, as `page_token`) when there is one, else from `since`.
/// Assumes `page_token` is exclusive (the page starts after that activity). If
/// the boundary fill were re-served it is harmless: its exec id (order id plus
/// cumulative quantity) matches the one already booked, so ingest dedups it.
pub(crate) fn activities_query(from: Option<&Cursor>, since: DateTime<Utc>) -> Vec<(&'static str, String)> {
    let mut q = vec![
        ("activity_types", "FILL".to_string()),
        ("direction", "asc".to_string()),
        ("page_size", ACTIVITIES_PAGE.to_string()),
    ];
    match from {
        Some(c) => q.push(("page_token", c.0.clone())),
        None => q.push(("after", since.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))),
    }
    q
}

/// FILL activities as a pull page: the next cursor is the last activity id
/// (empty when there were none, so the caller keeps its stamp), and a full page
/// means there may be more. Rows executed before `since` are dropped on every
/// page; the cursor still moves past them. Equity fills carry no fee.
pub(crate) fn activities_page(body: &serde_json::Value, account_ref: &str, limit: usize, since: DateTime<Utc>) -> PullPage {
    let num = |v: &serde_json::Value| v.as_str().and_then(|s| s.parse::<f64>().ok());
    let rows = body.as_array().map(Vec::as_slice).unwrap_or(&[]);
    let next = Cursor(rows.last().and_then(|r| r["id"].as_str()).unwrap_or_default().to_string());
    let fills = rows
        .iter()
        .filter_map(|r| {
            let executed_at = r["transaction_time"].as_str().and_then(|s| s.parse::<DateTime<Utc>>().ok());
            if executed_at.is_some_and(|at| at < since) {
                return None;
            }
            let order_id = r["order_id"].as_str()?;
            Some(VenueFill {
                exec_id: super::fill_exec_id(order_id, num(&r["cum_qty"])?),
                venue_order_id: order_id.to_string(),
                client_order_id: None,
                account_ref: account_ref.to_string(),
                symbol: r["symbol"].as_str()?.to_string(),
                buy: r["side"].as_str()? == "buy",
                qty: num(&r["qty"])?,
                price: num(&r["price"])?,
                fee: None,
                executed_at,
            })
        })
        .collect();
    PullPage { fills, next, more: rows.len() >= limit }
}

/// `GET /v2/orders?status=open` as the venue's view of each open order.
pub(crate) fn parse_open_order_snapshot(orders: &serde_json::Value, account_ref: &str) -> Vec<VenueOrderUpdate> {
    let num = |v: &serde_json::Value| v.as_str().and_then(|s| s.parse::<f64>().ok());
    orders
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| {
            let order_type = o["type"].as_str().unwrap_or("").to_string();
            Some(VenueOrderUpdate {
                venue_order_id: o["id"].as_str()?.to_string(),
                client_order_id: o["client_order_id"].as_str().filter(|c| !c.is_empty()).map(str::to_string),
                account_ref: account_ref.to_string(),
                symbol: o["symbol"].as_str()?.to_string(),
                buy: o["side"].as_str()? == "buy",
                limit_px: if order_type == "market" { None } else { num(&o["limit_price"]) },
                order_type,
                qty: num(&o["qty"])?,
                status: VenueOrderStatus::Working,
                cum_qty: num(&o["filled_qty"]).unwrap_or(0.0),
                ts: o["updated_at"].as_str().and_then(|s| s.parse().ok()),
            })
        })
        .collect()
}

/// One Alpaca key is one account, so `account_ref` is only echoed into what is
/// returned, and there is a single scope.
#[async_trait]
impl ExecutionSource for AlpacaAdapter {
    async fn fills_since(
        &self,
        account_ref: &str,
        _scope: &str,
        from: Option<&Cursor>,
        since: DateTime<Utc>,
    ) -> Result<PullPage, BrokerError> {
        let mut url = reqwest::Url::parse(&format!("{}/v2/account/activities", self.base_url))
            .map_err(|e| BrokerError::NotConfigured(e.to_string()))?;
        url.query_pairs_mut().extend_pairs(activities_query(from, since));
        let resp = self
            .get_json(url.as_str())
            .send()
            .await
            .map_err(|e| BrokerError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(crate::adapter::http_failure(status, resp.text().await.unwrap_or_default()));
        }
        let body: serde_json::Value = resp.json().await.map_err(|e| BrokerError::Network(e.to_string()))?;
        Ok(activities_page(&body, account_ref, ACTIVITIES_PAGE, since))
    }

    async fn open_order_snapshot(&self, account_ref: &str) -> Result<Vec<VenueOrderUpdate>, BrokerError> {
        let url = format!("{}{OPEN_ORDERS_PATH}", self.base_url);
        let resp = self.get_json(&url).send().await.map_err(|e| BrokerError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(crate::adapter::http_failure(status, resp.text().await.unwrap_or_default()));
        }
        let body: serde_json::Value = resp.json().await.map_err(|e| BrokerError::Network(e.to_string()))?;
        Ok(parse_open_order_snapshot(&body, account_ref))
    }

    async fn scopes(&self, _account_ref: &str) -> Result<Vec<String>, BrokerError> {
        Ok(vec![String::new()])
    }

    fn hints(&self) -> PullHints {
        PullHints { intraday_secs: 60, max_history: None, per_symbol: false }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_open_orders_snapshot_asks_for_every_open_order() {
        assert!(OPEN_ORDERS_PATH.contains("limit=500"));
    }

    #[test]
    fn open_orders_carry_the_venue_order_id() {
        let body = serde_json::json!([{ "id": "v-1", "client_order_id": "c-1", "symbol": "AAPL", "filled_qty": "0" }]);
        assert_eq!(parse_open_orders(&body)[0].venue_order_id, "v-1");
    }

    #[test]
    fn order_status_looks_up_by_client_id_without_a_venue_id() {
        assert_eq!(order_lookup_path("c-1", "v-1"), "/v2/orders/v-1");
        assert_eq!(order_lookup_path("c-1", ""), "/v2/orders:by_client_order_id?client_order_id=c-1");
    }

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

    #[test]
    fn parses_prev_close_from_snapshot_response() {
        let body: serde_json::Value = serde_json::from_str(
            r#"{
                "AAPL": {"prevDailyBar": {"c": 227.16}},
                "MSFT": {"prevDailyBar": {"c": 414.10}}
            }"#,
        )
        .unwrap();
        let mut stats = parse_alpaca_snapshots(&body);
        stats.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(stats, vec![("AAPL".to_string(), 227.16), ("MSFT".to_string(), 414.10)]);
    }

    #[test]
    fn a_symbol_missing_prev_daily_bar_is_skipped_not_zeroed() {
        let body: serde_json::Value = serde_json::from_str(
            r#"{"AAPL": {"prevDailyBar": {"c": 227.16}}, "NEWLIST": {}}"#,
        )
        .unwrap();
        let stats = parse_alpaca_snapshots(&body);
        assert_eq!(stats, vec![("AAPL".to_string(), 227.16)]);
    }

    #[test]
    fn activities_page_maps_and_cursors() {
        let body = serde_json::json!([
            { "id": "20261118143000000::a", "activity_type": "FILL", "transaction_time": "2026-11-18T14:30:00.5Z", "type": "fill",
              "price": "101.5", "qty": "2", "side": "buy", "symbol": "AAPL", "order_id": "ord-1", "cum_qty": "2" },
            { "id": "20261118143100000::b", "activity_type": "FILL", "transaction_time": "2026-11-18T14:31:00Z", "type": "partial_fill",
              "price": "50", "qty": "1", "side": "sell", "symbol": "MSFT", "order_id": "ord-2", "cum_qty": "1" },
        ]);
        let page = activities_page(&body, "", 2, chrono::DateTime::<chrono::Utc>::MIN_UTC);
        assert_eq!(page.fills.len(), 2);
        let f = &page.fills[0];
        assert_eq!((f.exec_id.as_str(), f.venue_order_id.as_str(), f.symbol.as_str(), f.buy, f.qty, f.price), ("ord-1:2", "ord-1", "AAPL", true, 2.0, 101.5));
        assert_eq!((f.fee.clone(), f.client_order_id.clone()), (None, None));
        assert!(!page.fills[1].buy);
        assert_eq!(page.next, Cursor("20261118143100000::b".into()));
        assert!(page.more, "a full page asks for the next");
        assert!(!activities_page(&body, "", 3, chrono::DateTime::<chrono::Utc>::MIN_UTC).more);
        // since drops older rows but the cursor still moves past them.
        let since = "2026-11-18T14:30:30Z".parse().unwrap();
        let late = activities_page(&body, "", 100, since);
        assert_eq!(late.fills.len(), 1);
        assert_eq!(late.next, Cursor("20261118143100000::b".into()));
        // An empty page gives no cursor, so the reconciler keeps its stamp.
        let empty = activities_page(&serde_json::json!([]), "", 100, since);
        assert!(empty.fills.is_empty() && empty.next == Cursor(String::new()) && !empty.more);
    }

    #[test]
    fn activities_query_resumes_from_the_cursor_else_from_since() {
        let since = "2026-11-18T14:30:30Z".parse().unwrap();
        let first = activities_query(None, since);
        assert!(first.contains(&("after", "2026-11-18T14:30:30Z".to_string())) && !first.iter().any(|(k, _)| *k == "page_token"));
        let resumed = activities_query(Some(&Cursor("id-9".into())), since);
        assert!(resumed.contains(&("page_token", "id-9".to_string())) && !resumed.iter().any(|(k, _)| *k == "after"));
        assert!(resumed.contains(&("direction", "asc".to_string())));
    }

    #[test]
    fn open_orders_snapshot_rows_are_working_updates() {
        let body = serde_json::json!([{ "id": "o1", "client_order_id": "c1", "symbol": "AAPL", "side": "buy", "type": "limit",
            "qty": "5", "filled_qty": "1", "limit_price": "100" }]);
        let rows = parse_open_order_snapshot(&body, "");
        assert_eq!((rows[0].venue_order_id.as_str(), rows[0].cum_qty, rows[0].qty, rows[0].limit_px), ("o1", 1.0, 5.0, Some(100.0)));
        assert_eq!(ExecutionSource::hints(&AlpacaAdapter::new("k".into(), "s".into(), "PAPER")), PullHints { intraday_secs: 60, max_history: None, per_symbol: false });
    }
}
